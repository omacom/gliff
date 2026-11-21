//! Use another machine's screen as one more screen of this one.
//!
//! A local `gliff-server --headless` adds an output to this desktop, and the
//! other machine shows it with `gliff --stdio`, started over ssh with the
//! server's stdout as its stdin and the other way around. Only this machine
//! needs ssh access to the other one; nothing connects back.
//!
//! A gliff without `--stdio` (0.4.1 and older) still shows it: it connects
//! to a host as usual, through an `ssh` on its PATH that is really a relay
//! to this stream. Hyprland makes its window fullscreen, and the screen
//! follows the window's size.
//!
//! Stopping kills only ssh. The other window then sees its stream end and
//! closes, and the server sees stdin end and removes its output, so no
//! signal can skip that cleanup, not even this process exiting.

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk4::glib;

/// How the other machine is reached and what runs there.
pub struct Target {
    pub host: String,
    /// The other machine's gliff.
    pub remote_bin: String,
}

/// A running screen on another machine. Dropping it stops the screen.
pub struct Display {
    ssh: Child,
    server: Child,
    /// glib has reaped ssh, so its pid may already belong to another
    /// process and must not be signalled.
    ssh_reaped: bool,
}

impl Display {
    /// Start the local server and the window on `target`. `ended` runs once,
    /// on the main loop, with why the screen stopped: `None` when the other
    /// window was closed or [`stop`](Self::stop) was called.
    pub fn start(
        target: &Target,
        side: &str,
        align: &str,
        ended: impl FnOnce(Option<String>) + 'static,
    ) -> std::io::Result<Rc<RefCell<Option<Display>>>> {
        let mut server = Command::new(local_server())
            .args([
                "--stdio",
                "--headless",
                "--position",
                side,
                "--align",
                align,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let to_server = server.stdin.take().expect("piped stdin");
        let from_server = server.stdout.take().expect("piped stdout");
        let ssh = Command::new("ssh")
            .args(["-T", "-o", "Compression=no", "-o", "IPQoS=lowdelay"])
            .arg("--")
            .arg(&target.host)
            // The remote shell joins these words and parses them again: the
            // script is quoted for it, the gliff path is left to it, as
            // `--server-bin` is, and the name is a plain word.
            .args(["sh", "-c", &format!("'{REMOTE_SCRIPT}'"), "gliff"])
            .arg(&target.remote_bin)
            .arg(this_machine())
            .stdin(Stdio::from(from_server))
            .stdout(Stdio::from(to_server))
            .stderr(Stdio::piped())
            .spawn();
        let mut ssh = match ssh {
            Ok(ssh) => ssh,
            Err(e) => {
                // Its stdin is already closed, so it ends and removes its
                // output by itself; reap it.
                std::thread::spawn(move || server.wait());
                return Err(e);
            }
        };
        // The last lines ssh and the other gliff wrote, to explain a failure.
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
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
        let display = Rc::new(RefCell::new(Some(Display {
            ssh,
            server,
            ssh_reaped: false,
        })));
        let watched = Rc::downgrade(&display);
        let host = target.host.clone();
        let mut ended = Some(ended);
        glib::child_watch_add_local(pid, move |_, status| {
            // glib reaped ssh; let the server finish its own cleanup.
            if let Some(mut d) = watched.upgrade().and_then(|d| d.borrow_mut().take()) {
                d.ssh_reaped = true;
                d.finish_server();
            }
            let why = failure(&host, status, &said.lock().unwrap());
            if let Some(ended) = ended.take() {
                ended(why);
            }
        });
        Ok(display)
    }

    /// Stop the screen: the child watch reports it as ended by the user.
    pub fn stop(&mut self) {
        if !self.ssh_reaped {
            let _ = self.ssh.kill();
        }
    }

    /// The server ends when its stdin does; give it time to remove its
    /// output, then make sure it is gone.
    fn finish_server(mut self) {
        std::thread::spawn(move || {
            for _ in 0..50 {
                if !matches!(self.server.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.server.kill();
            let _ = self.server.wait();
        });
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs on the other machine as `sh -c` with its gliff as `$1` and this
/// machine's name as `$2`. A gliff that knows `--stdio` takes the stream
/// itself. An older one gets `ssh` on its PATH replaced by a relay: the
/// stream reaches it as fds 3 and 4, and when the stream ends the relay
/// closes the window, as `--stdio` does. Its window is made fullscreen
/// through Hyprland once it maps, with a Lua config or an older one. That gliff
/// keeps its own list of machines, so it gets a config directory that has
/// everything but it. It cannot leave a GPU whose decoding fails, as every
/// NVIDIA one does through VA-API, so where all GPUs are NVIDIA it decodes
/// on the CPU.
///
/// It must hold no `'` or `\`, which the remote login shell would not keep.
const REMOTE_SCRIPT: &str = r#"if "$1" --help 2>/dev/null | grep -q -- --stdio; then exec "$1" --stdio --fullscreen; fi
d=$(mktemp -d) || exit 1
trap "rm -rf $d" EXIT
cat >"$d/ssh" <<"END" || exit 1
#!/bin/sh
st() { read -r s </proc/$PPID/stat && set -- ${s##*) } && shift 19 && echo "$1"; } 2>/dev/null
t=$(st)
{ cat <&3; [ "$(st)" = "$t" ] && kill "$PPID"; } 4>&- 2>/dev/null &
exec cat 3<&- >&4 2>/dev/null
END
chmod +x "$d/ssh" || exit 1
c=${XDG_CONFIG_HOME:-$HOME/.config}
mkdir "$d/config" || exit 1
for f in "$c"/* "$c"/.[!.]*; do [ -e "$f" ] && ln -s "$f" "$d/config/"; done
rm -f "$d/config/gliff"
# The newest Hyprland that answers and its Wayland display, as --stdio
# finds them.
r=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
if [ -z "$HYPRLAND_INSTANCE_SIGNATURE" ]; then
  for i in $(ls -t "$r/hypr" 2>/dev/null); do
    [ -S "$r/hypr/$i/.socket.sock" ] && HYPRLAND_INSTANCE_SIGNATURE=$i hyprctl version >/dev/null 2>&1 &&
      HYPRLAND_INSTANCE_SIGNATURE=$i && break
  done
fi
[ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] || { echo "No Hyprland session is running there" >&2; exit 1; }
[ -n "$WAYLAND_DISPLAY" ] || WAYLAND_DISPLAY=$(sed -n 2p "$r/hypr/$HYPRLAND_INSTANCE_SIGNATURE/hyprland.lock")
export XDG_RUNTIME_DIR="$r" HYPRLAND_INSTANCE_SIGNATURE WAYLAND_DISPLAY
v=
for n in /sys/class/drm/renderD*/device/driver; do
  case $(readlink "$n") in */nvidia) v=${v:-cpu} ;; *) v=gpu; break ;; esac
done
[ -n "$GLIFF_VIDEO" ] || [ "$v" != cpu ] || export GLIFF_VIDEO=cpu
exec 3<&0 4>&1
PATH="$d:$PATH" XDG_CONFIG_HOME="$d/config" "$1" --headless "$2" </dev/null >/dev/null &
g=$!
{
  for i in $(seq 50); do hyprctl clients | grep -q "pid: $g$" && break; sleep 0.2; done
  # A Lua config sets it; an older one only toggles the focused window.
  hyprctl dispatch "hl.dsp.window.fullscreen({ window = [[pid:$g]], action = [[set]] })" | grep -qx ok ||
    { hyprctl dispatch focuswindow pid:$g | grep -qx ok &&
      hyprctl activewindow | grep -q "pid: $g$" && hyprctl activewindow | grep -q "fullscreen: 0$" &&
      hyprctl dispatch fullscreen 0; }
} </dev/null >/dev/null 2>&1 3<&- 4>&- &
wait "$g"
"#;

/// This machine's name for the other window's title: a host name, which
/// the remote shell takes as one plain word.
fn this_machine() -> String {
    let name = glib::host_name();
    let plain = !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if plain {
        name.to_string()
    } else {
        "gliff".into()
    }
}

/// How many of the last stderr lines are kept to explain a failure.
const STDERR_LINES: usize = 16;

/// The gliff-server next to this gliff, so a build runs its own server, or
/// the one on PATH.
fn local_server() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("gliff-server")))
        .filter(|server| server.is_file())
        .unwrap_or_else(|| "gliff-server".into())
}

/// Why ssh ended, for the user, from its wait status and the last lines it
/// wrote; `None` for a normal end.
fn failure(host: &str, status: i32, said: &[String]) -> Option<String> {
    // A killed ssh (Stop) or the other window closing ends the screen
    // without an error.
    let code = if libc_exited(status) {
        (status >> 8) & 0xff
    } else {
        return None;
    };
    // clap ends an unknown-flag error with usage and help lines.
    let unknown_flag = said.iter().any(|l| l.contains("unexpected argument '--"));
    let reason = said.iter().rev().map(|l| l.trim()).find(|l| {
        !l.starts_with("Usage:") && !l.starts_with("For more information") && !l.starts_with("tip:")
    });
    match code {
        0 => None,
        127 => Some(format!("gliff is not installed on {host}")),
        _ if unknown_flag => Some(format!(
            "gliff on {host} is too old to show another machine's screen"
        )),
        _ => Some(match reason {
            Some(line) => line.to_string(),
            None => format!("The screen on {host} stopped (exit {code})"),
        }),
    }
}

/// `WIFEXITED` for the wait status glib passes to a child watch.
fn libc_exited(status: i32) -> bool {
    status & 0x7f == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_statuses_explain_themselves() {
        let exited = |code: i32| code << 8;
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(failure("h", exited(0), &[]), None);
        // Killed by Stop: SIGKILL, not an exit.
        assert_eq!(failure("h", 9, &lines("anything")), None);
        assert_eq!(
            failure("h", exited(127), &lines("bash: gliff: command not found")),
            Some("gliff is not installed on h".into())
        );
        // What clap prints for a flag an older gliff does not know.
        let old = "error: unexpected argument '--headless' found\n  tip: to pass '--headless' as a value, use '-- --headless'\nUsage: gliff [OPTIONS] [HOST]\nFor more information, try '--help'.";
        assert_eq!(
            failure("h", exited(2), &lines(old)),
            Some("gliff on h is too old to show another machine's screen".into())
        );
        assert_eq!(
            failure(
                "h",
                exited(255),
                &lines("ssh: connect to host h port 22: No route to host")
            ),
            Some("ssh: connect to host h port 22: No route to host".into())
        );
        assert_eq!(
            failure("h", exited(1), &[]),
            Some("The screen on h stopped (exit 1)".into())
        );
    }

    #[test]
    fn remote_script_survives_the_remote_shell() {
        // Single quotes keep it whole in sh, bash, zsh and fish, as long
        // as it holds no quote or backslash of its own.
        assert!(!REMOTE_SCRIPT.contains(['\'', '\\']));
        let name = this_machine();
        assert!(!name.is_empty() && !name.starts_with('-'));
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
    }
}
