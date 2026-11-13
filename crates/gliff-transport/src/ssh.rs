//! Spawn `gliff-server --stdio` over ssh and expose its stdio as a stream.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

/// ssh's exit status when the error was its own: it could not connect or
/// log in, or it lost the connection. Any other status is the remote
/// command's.
const SSH_ERROR: i32 = 255;
/// How much of ssh's stderr to keep. Its last line says why it exited.
const STDERR_TAIL: usize = 4096;
/// How long to wait for ssh to exit, and then for its stderr to drain, once
/// the session is over. An ssh that is still running has not failed.
const EXIT_WAIT: Duration = Duration::from_millis(250);

/// How to reach the server host over ssh.
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// `user@host` or an ssh config alias.
    pub host: String,
    /// Remote `gliff-server` path (default: `gliff-server` on PATH).
    pub server_bin: String,
    /// Extra args passed to the server after `--stdio`.
    pub server_args: Vec<String>,
    /// Extra ssh options (e.g. `-J jump`).
    pub ssh_args: Vec<String>,
}

impl SshTarget {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            server_bin: "gliff-server".into(),
            server_args: Vec::new(),
            ssh_args: Vec::new(),
        }
    }
}

/// The spawned ssh process and its piped stdio, joined into one duplex stream.
pub struct SshStream {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: SshStderr,
}

/// Why ssh gave up, in its own words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SshFailure {
    /// The last line ssh printed, such as
    /// `user@host: Permission denied (publickey).`
    pub message: String,
    /// The remote refused the login. Another attempt fails the same way, and
    /// sshd counts each one against this client.
    pub denied: bool,
}

/// ssh's stderr, copied through to ours with the tail kept.
pub struct SshStderr {
    tail: Arc<Mutex<Vec<u8>>>,
    copied: JoinHandle<()>,
}

impl SshStderr {
    fn copy_to<R, W>(from: R, to: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let tail = Arc::new(Mutex::new(Vec::new()));
        let copied = tokio::spawn(copy_stderr(from, to, tail.clone()));
        Self { tail, copied }
    }

    /// Why ssh exited, when the error was its own. The session only sees
    /// its pipe close, which reads as "Broken pipe"; the reason is the last
    /// line ssh printed. None while ssh still runs, when it exited with the
    /// remote command's status, or when it printed nothing.
    ///
    /// Call it once the session is over, whatever the outcome: it also
    /// waits for the rest of an exited ssh's stderr to be copied through.
    pub async fn failure(mut self, child: &mut Child) -> Option<SshFailure> {
        let status = tokio::time::timeout(EXIT_WAIT, child.wait())
            .await
            .ok()?
            .ok()?;
        // stderr ends when ssh exits, unless a helper it started (a
        // ProxyCommand, a connection master) still holds it open.
        let _ = tokio::time::timeout(EXIT_WAIT, &mut self.copied).await;
        if status.code() != Some(SSH_ERROR) {
            return None;
        }
        let message = last_line(&self.tail.lock().unwrap_or_else(|e| e.into_inner()))?;
        Some(SshFailure {
            denied: login_denied(&message),
            message,
        })
    }
}

/// Spawn ssh. Compression is off (video is already compressed), the socket
/// asks for low-latency queuing, and unknown host keys are accepted unless
/// ssh config says otherwise. stderr is copied to ours so server logs reach
/// the client's terminal, and its tail is kept to explain an ssh failure.
pub fn spawn_ssh(target: &SshTarget) -> std::io::Result<SshStream> {
    let mut cmd = ssh_command(target);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    Ok(SshStream {
        child,
        stdin,
        stdout,
        stderr: SshStderr::copy_to(stderr, tokio::io::stderr()),
    })
}

/// Pass ssh's stderr on as it arrives, keeping the last `STDERR_TAIL` bytes.
async fn copy_stderr<R, W>(mut from: R, mut to: W, tail: Arc<Mutex<Vec<u8>>>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        keep_tail(
            &mut tail.lock().unwrap_or_else(|e| e.into_inner()),
            &buf[..n],
        );
        // Keep reading when ours is gone: ssh would block on a full pipe.
        let _ = to.write_all(&buf[..n]).await;
        let _ = to.flush().await;
    }
}

fn keep_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    let extra = tail.len().saturating_sub(STDERR_TAIL);
    tail.drain(..extra);
}

/// The last line with anything on it. ssh ends its lines with `\r\n`.
fn last_line(tail: &[u8]) -> Option<String> {
    String::from_utf8_lossy(tail)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Whether ssh's last line says the remote refused the login, as in
/// `user@host: Permission denied (publickey,password).`. A connect that
/// fails with EACCES and a server's own "Permission denied (os error 13)"
/// read differently.
fn login_denied(line: &str) -> bool {
    line.contains(": Permission denied (") && line.ends_with(").")
}

fn ssh_command(target: &SshTarget) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-T")
        .arg("-o")
        .arg("Compression=no")
        .arg("-o")
        .arg("IPQoS=lowdelay")
        .args(&target.ssh_args);
    // No terminal to answer the host-key prompt: trust a host on first
    // connect, as typing "yes" would. A changed key still fails. Only when
    // the user's ssh config leaves the default, since -o would override it.
    if host_key_checking_is_default(target) {
        cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
    }
    cmd.arg("--")
        .arg(&target.host)
        .arg(&target.server_bin)
        .arg("--stdio")
        .args(&target.server_args);
    cmd
}

/// Whether ssh's effective StrictHostKeyChecking for this host is the default
/// `ask`, i.e. neither ssh config nor ssh_args set it.
fn host_key_checking_is_default(target: &SshTarget) -> bool {
    let Ok(out) = std::process::Command::new("ssh")
        .arg("-G")
        .args(&target.ssh_args)
        .arg("--")
        .arg(&target.host)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l == "stricthostkeychecking ask")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_cannot_supply_ssh_options() {
        let target = SshTarget::new("-oProxyCommand=touch /tmp/unwanted");
        let cmd = ssh_command(&target);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        let host = args.iter().position(|a| *a == target.host).unwrap();
        assert_eq!(args[host - 1], "--");
    }

    #[test]
    fn the_last_line_is_the_reason() {
        let tail = b"Warning: Permanently added 'desk' (ED25519) to the list of known hosts.\r\n\
            user@host: Permission denied (publickey).\r\n\n";
        assert_eq!(
            last_line(tail).as_deref(),
            Some("user@host: Permission denied (publickey).")
        );
        assert_eq!(last_line(b"\r\n \n"), None);
    }

    #[test]
    fn only_a_refused_login_is_denied() {
        assert!(login_denied(
            "user@host: Permission denied (publickey,password)."
        ));
        assert!(!login_denied(
            "ssh: connect to host desk port 22: Connection refused"
        ));
        assert!(!login_denied(
            "ssh: connect to host desk port 22: Permission denied"
        ));
        assert!(!login_denied(
            "Error: open /dev/uinput: Permission denied (os error 13)"
        ));
        assert!(!login_denied(
            "ssh: Could not resolve hostname desk: Name or service not known"
        ));
    }

    #[test]
    fn the_tail_keeps_only_the_end() {
        let mut tail = Vec::new();
        keep_tail(&mut tail, &vec![b'a'; STDERR_TAIL]);
        keep_tail(&mut tail, b"the end");
        assert_eq!(tail.len(), STDERR_TAIL);
        assert!(tail.ends_with(b"the end"));
    }

    /// Run `script` in a shell the way `spawn_ssh` runs ssh, with its
    /// stderr copied to `to`.
    fn spawn_shell<W>(script: &str, to: W) -> (Child, SshStderr)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        (child, SshStderr::copy_to(stderr, to))
    }

    /// What `failure` says once `script` has exited. Waiting first keeps a
    /// slow machine from running into `EXIT_WAIT`.
    async fn failure_of(script: &str) -> Option<SshFailure> {
        let (mut child, stderr) = spawn_shell(script, tokio::io::sink());
        child.wait().await.unwrap();
        stderr.failure(&mut child).await
    }

    #[tokio::test]
    async fn an_ssh_error_is_reported_in_its_own_words() {
        assert_eq!(
            failure_of("echo 'user@host: Permission denied (publickey).' >&2; exit 255").await,
            Some(SshFailure {
                message: "user@host: Permission denied (publickey).".into(),
                denied: true,
            })
        );
        assert_eq!(
            failure_of(
                "echo 'ssh: connect to host desk port 22: Connection refused' >&2; exit 255"
            )
            .await,
            Some(SshFailure {
                message: "ssh: connect to host desk port 22: Connection refused".into(),
                denied: false,
            })
        );
    }

    #[tokio::test]
    async fn the_remote_commands_exit_is_not_an_ssh_failure() {
        assert_eq!(
            failure_of("echo 'gliff-server: no session' >&2; exit 1").await,
            None
        );
        assert_eq!(failure_of("exit 255").await, None);
    }

    #[tokio::test]
    async fn a_running_ssh_has_not_failed() {
        let (mut child, stderr) =
            spawn_shell("echo 'still here' >&2; exec sleep 30", tokio::io::sink());
        assert_eq!(stderr.failure(&mut child).await, None);
    }

    /// A writer the test holds shut, then opens and reads back.
    #[derive(Clone, Default)]
    struct Held(Arc<Mutex<HeldState>>);

    #[derive(Default)]
    struct HeldState {
        open: bool,
        written: Vec<u8>,
        waker: Option<std::task::Waker>,
    }

    impl Held {
        fn open(&self) {
            let mut held = self.0.lock().unwrap();
            held.open = true;
            if let Some(waker) = held.waker.take() {
                waker.wake();
            }
        }
    }

    impl AsyncWrite for Held {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut held = self.0.lock().unwrap();
            if !held.open {
                held.waker = Some(cx.waker().clone());
                return std::task::Poll::Pending;
            }
            held.written.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn the_remote_commands_stderr_is_through_when_failure_returns() {
        let ours = Held::default();
        let (mut child, stderr) = spawn_shell(
            "echo 'sh: gliff-server: command not found' >&2; exit 127",
            ours.clone(),
        );
        child.wait().await.unwrap();
        // The command is gone and its last words are still on the way. The
        // worker drops its runtime once `failure` returns, so they have to
        // be through by then.
        ours.open();
        assert_eq!(stderr.failure(&mut child).await, None);
        assert_eq!(
            ours.0.lock().unwrap().written,
            b"sh: gliff-server: command not found\n"
        );
    }
}
