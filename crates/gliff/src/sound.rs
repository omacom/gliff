//! Play another machine's sound on this one.
//!
//! Over ssh, `pw-record` takes what the other machine's default output
//! plays, from before its volume, as raw 48 kHz stereo, and a local `pw-play`
//! plays it. The other machine's output is muted meanwhile, so the sound
//! moves here rather than playing twice, and unmuted again however the
//! stream ends: the script restores it on exit, and ssh going away ends it.
//! The output muted and unmuted is the one that was the default when the
//! sound started, even if another becomes the default meanwhile.
//! It mutes only once the capture has woken the output: WirePlumber restores
//! a suspended output's saved mute when it wakes, which would undo it.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk4::glib;

/// How long either end buffers, so speech keeps up with the pointer.
const LATENCY: &str = "30ms";
const RAW: [&str; 7] = [
    "-a",
    "--format",
    "s16",
    "--rate",
    "48000",
    "--channels",
    "2",
];
/// Sound held back before the first byte plays: 100 ms of 48 kHz 16-bit
/// stereo. It arrives in bursts about 21 ms apart, and over WiFi one is now
/// and then 70 ms late, which `--latency` does not absorb: `pw-play` keeps
/// no queue of its own, so the queue is what waits in its stdin.
const CUSHION: usize = 19_200;
/// How many of the last stderr lines are kept to explain a failure.
const STDERR_LINES: usize = 4;

/// Runs on the other machine as `sh -c`. It must hold no `'` or `\`, which
/// the remote login shell would not keep.
const REMOTE_SCRIPT: &str = r#"command -v pw-record >/dev/null || { echo "pw-record (PipeWire) is not installed there" >&2; exit 127; }
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
sink=$(pactl get-default-sink 2>/dev/null)
sink=${sink:-@DEFAULT_SINK@}
was=$(pactl get-sink-mute "$sink" 2>/dev/null)
pw-record "$@" -P "{ stream.capture.sink = true }" - &
rec=$!
restore() { kill $rec 2>/dev/null; [ "$was" = "Mute: no" ] && pactl set-sink-mute "$sink" 0; }
trap restore EXIT
trap exit HUP INT TERM PIPE
for i in 1 2 3 4 5 6 7 8 9 10; do pactl list short sinks 2>/dev/null | grep -F "$sink" | grep -q RUNNING && break; sleep 0.2; done
[ "$was" = "Mute: no" ] && { pactl set-sink-mute "$sink" 1; sleep 1; pactl set-sink-mute "$sink" 1; }
wait $rec"#;

/// Another machine's sound, playing here. Dropping it stops the sound.
pub struct Sound {
    ssh: Child,
    player: Option<Child>,
    /// glib has reaped ssh, so its pid may already belong to another
    /// process and must not be signalled.
    ssh_reaped: bool,
}

impl Sound {
    /// Play `host`'s sound here. `ended` runs once, on the main loop, with
    /// why the sound stopped: `None` when [`stop`](Self::stop) was called.
    pub fn start(
        host: &str,
        ended: impl FnOnce(Option<String>) + 'static,
    ) -> std::io::Result<Rc<RefCell<Option<Sound>>>> {
        let mut player = Command::new("pw-play")
            .args(player_args(host))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let to_player = player.stdin.take().expect("piped stdin");
        let ssh = Command::new("ssh")
            .args(["-T", "-o", "Compression=no", "-o", "IPQoS=lowdelay"])
            .arg("--")
            .arg(host)
            // The remote shell joins these words and parses them again, so
            // the script is quoted for it and the arguments are plain words.
            .args(["sh", "-c", &format!("'{REMOTE_SCRIPT}'"), "gliff"])
            .args(RAW)
            .args(["--latency", LATENCY])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut ssh = match ssh {
            Ok(ssh) => ssh,
            Err(e) => {
                let _ = player.kill();
                let _ = player.wait();
                return Err(e);
            }
        };
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(sound) = ssh.stdout.take() {
            std::thread::spawn(move || relay(sound, to_player));
        }
        if let Some(stderr) = ssh.stderr.take() {
            let said = said.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("{line}");
                    if !line.trim().is_empty() {
                        let mut said = said.lock().unwrap();
                        if said.len() == STDERR_LINES {
                            said.remove(0);
                        }
                        said.push(line);
                    }
                }
            });
        }
        let pid = glib::Pid(ssh.id() as i32);
        let sound = Rc::new(RefCell::new(Some(Sound {
            ssh,
            player: Some(player),
            ssh_reaped: false,
        })));
        let watched = Rc::downgrade(&sound);
        let mut ended = Some(ended);
        glib::child_watch_add_local(pid, move |_, status| {
            let stopped = match watched.upgrade().and_then(|s| s.borrow_mut().take()) {
                Some(mut s) => {
                    s.ssh_reaped = true;
                    false
                }
                // `stop` took it first.
                None => true,
            };
            let why = if stopped {
                None
            } else {
                Some(failure(status, &said.lock().unwrap()))
            };
            if let Some(ended) = ended.take() {
                ended(why);
            }
        });
        Ok(sound)
    }

    /// Stop the sound: ssh goes, the other machine unmutes its output, and
    /// the player here drains and ends.
    pub fn stop(&mut self) {
        if !self.ssh_reaped {
            let _ = self.ssh.kill();
        }
    }
}

impl Drop for Sound {
    fn drop(&mut self) {
        self.stop();
        // Its input is closed with ssh; once it has played what it holds,
        // reap it off the main loop.
        if let Some(mut player) = self.player.take() {
            std::thread::spawn(move || player.wait());
        }
    }
}

/// Pass the sound on once [`CUSHION`] of it has arrived, so the player's
/// input stays that far ahead. Ending `from` ends `to`.
fn relay(mut from: impl Read, mut to: impl Write) {
    let mut held = vec![0; CUSHION];
    let mut len = 0;
    while len < CUSHION {
        match from.read(&mut held[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
    }
    if to.write_all(&held[..len]).is_ok() {
        let _ = std::io::copy(&mut from, &mut to);
    }
}

/// The local player's arguments: raw audio on stdin, in a stream named
/// after the machine it comes from.
fn player_args(host: &str) -> Vec<String> {
    let name = host.replace(['"', '\\'], "");
    let mut args: Vec<String> = RAW.iter().map(|a| a.to_string()).collect();
    args.extend([
        "--latency".into(),
        LATENCY.into(),
        "-P".into(),
        format!("{{ media.name = \"{name} (gliff)\" }}"),
        "-".into(),
    ]);
    args
}

/// Why the sound ended on its own, from the wait status glib passes to a
/// child watch and the last lines ssh wrote.
fn failure(status: i32, said: &[String]) -> String {
    let exited = status & 0x7f == 0;
    let code = (status >> 8) & 0xff;
    match said.last() {
        Some(line) => line.clone(),
        None if exited && code == 0 => "the other machine stopped sending sound".into(),
        None if exited => format!("ssh exited with status {code}"),
        None => "ssh was stopped by a signal".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_script_survives_the_remote_shell() {
        assert!(!REMOTE_SCRIPT.contains(['\'', '\\']));
    }

    #[test]
    fn sound_is_held_until_the_cushion_fills() {
        /// Sends what it reads into a channel, so the test can see when.
        struct Tell(std::sync::mpsc::Sender<Vec<u8>>);
        impl Write for Tell {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.send(data.to_vec());
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (mut feed, from) = std::os::unix::net::UnixStream::pair().unwrap();
        let (tx, written) = std::sync::mpsc::channel();
        let relaying = std::thread::spawn(move || relay(from, Tell(tx)));
        let wait = std::time::Duration::from_millis(50);

        feed.write_all(&vec![1; CUSHION - 1]).unwrap();
        assert!(written.recv_timeout(wait).is_err(), "nothing plays yet");
        feed.write_all(&[2]).unwrap();
        assert_eq!(written.recv().unwrap().len(), CUSHION);
        feed.write_all(&[3]).unwrap();
        assert_eq!(written.recv().unwrap(), vec![3]);
        // The end of the sound lets go of what is held.
        drop(feed);
        relaying.join().unwrap();

        let mut out = Vec::new();
        relay(&[4u8; 10][..], &mut out);
        assert_eq!(out, vec![4; 10]);
    }

    #[test]
    fn the_player_takes_its_arguments() {
        let args = player_args("me@box \"x\"");
        assert!(args.contains(&"{ media.name = \"me@box x (gliff)\" }".to_string()));
        // Where PipeWire is installed, it must accept them: it plays a
        // moment of silence at no volume.
        let Ok(mut player) = Command::new("pw-play")
            .args(args)
            .arg("--volume")
            .arg("0")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        use std::io::Write;
        let _ = player.stdin.take().unwrap().write_all(&[0; 1920]);
        let status = player.wait().unwrap();
        // No PipeWire server to play on (as in CI) fails differently from
        // an argument it does not know, which prints the help and exits 1
        // at once; only the latter is checked where a server runs.
        if std::env::var_os("XDG_RUNTIME_DIR").is_some()
            && std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap())
                .join("pipewire-0")
                .exists()
        {
            assert!(status.success(), "pw-play refused its arguments: {status}");
        }
    }

    #[test]
    fn explains_how_the_sound_ended() {
        assert_eq!(failure(0, &[]), "the other machine stopped sending sound");
        assert_eq!(failure(255 << 8, &[]), "ssh exited with status 255");
        assert_eq!(failure(9, &[]), "ssh was stopped by a signal");
        let said = ["pw-record (PipeWire) is not installed there".to_string()];
        assert_eq!(failure(127 << 8, &said), said[0]);
    }
}
