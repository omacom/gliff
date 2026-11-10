//! A renewable compositor-side lease makes 1Password capturable only while
//! a server session is alive. Disabling our rule restores the user's rules.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCRIPT: &str = include_str!("password_sharing.lua");

pub struct PasswordSharing {
    stop: Sender<()>,
    task: Option<JoinHandle<()>>,
}

impl PasswordSharing {
    pub fn start(instance: hypr_ipc::Instance) -> std::io::Result<Self> {
        let lease = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let command = |connected| {
            format!("eval local lease = '{lease}'; local connected = {connected};\n{SCRIPT}")
        };
        let renew = command(true);
        let release = command(false);
        let (stop, rx) = channel();
        let task = std::thread::Builder::new()
            .name("gliff-password-sharing".into())
            .spawn(move || {
                loop {
                    if let Err(error) = instance.dispatch(&renew) {
                        // Older Hyprland versions cannot run this Lua API.
                        // Leave their protection in place and keep streaming.
                        tracing::warn!(%error, "cannot temporarily enable 1Password capture");
                        break;
                    }
                    if !matches!(rx.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Timeout)) {
                        break;
                    }
                }
                if let Err(error) = instance.dispatch(&release) {
                    tracing::debug!(%error, "1Password capture lease will expire via compositor watchdog");
                }
            })?;
        Ok(Self {
            stop,
            task: Some(task),
        })
    }
}

impl Drop for PasswordSharing {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    // Exercise the real worker and IPC: even a failed renewal (which could
    // mean only its reply was lost) must be followed by a release.
    fn worker_releases(renew_reply: &'static str) {
        let dir = std::env::temp_dir().join(format!(
            "gliff-lease-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let mut commands = Vec::new();
            for reply in [renew_reply, "ok"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = vec![0; SCRIPT.len() + 256];
                let mut count = 0;
                while !bytes[..count].ends_with(SCRIPT.as_bytes()) {
                    let n = socket.read(&mut bytes[count..]).unwrap();
                    assert_ne!(n, 0);
                    count += n;
                }
                commands.push(String::from_utf8(bytes[..count].to_vec()).unwrap());
                socket.write_all(reply.as_bytes()).unwrap();
            }
            commands
        });
        let guard = PasswordSharing::start(hypr_ipc::Instance {
            signature: "test".into(),
            dir: dir.clone(),
        })
        .unwrap();
        drop(guard);
        let commands = server.join().unwrap();
        assert!(commands[0].contains("local connected = true;"));
        assert_eq!(
            commands[1],
            commands[0].replacen("connected = true", "connected = false", 1)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disconnect_releases_the_lease() {
        worker_releases("ok");
    }

    #[test]
    fn failed_renewal_still_releases_the_lease() {
        worker_releases("error");
    }
}
