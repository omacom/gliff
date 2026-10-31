//! Offer to install gliff on a machine that does not have it.
//!
//! The work happens on the remote, over the same ssh the session uses: a
//! script read from stdin by `python3`, so no host or path is ever part of a
//! remote shell command. It installs the signed Omarchy package for the user
//! only; a system-wide install stays the user's own `sudo pacman -S gliff`.
//!
//! A machine whose gliff-server is too old to be controlled is offered this
//! computer's own, which is what has the feature when no package does yet.
//! For a machine of another architecture that is the server kept for it in
//! `gliff/servers/<arch>/` under the user's data directory.

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

const SCRIPT: &str = include_str!("remote_install.py");
/// Downloading and checking a few megabytes on a slow link.
const TIMEOUT: Duration = Duration::from_secs(180);
/// Asking a machine what its gliff-server can do.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Where gliff-server is uploaded to, in the remote home, for the script.
const UPLOAD: &str = "~/.local/bin/.gliff-server.upload";

/// What the remote script reports, as one JSON line.
#[derive(Debug, Deserialize, PartialEq)]
struct Reply {
    ok: bool,
    #[serde(default)]
    status: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    on_path: Option<bool>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// gliff-server is on the remote PATH now, or already was.
    Ready(String),
    Failed(String),
}

/// Ask whether to install gliff on `host`; `true` if the user agreed.
pub async fn confirm(window: &impl IsA<gtk::Widget>, host: &str) -> bool {
    let dialog = adw::AlertDialog::new(
        Some(&format!("gliff is not installed on {host}")),
        Some(
            "gliff can install it for your user on that machine. It downloads the \
             official Omarchy package, checks its signature with that machine's \
             pacman keyring, and copies it to ~/.local. No password or sudo is needed.\n\n\
             To install it for all users instead, run on that machine:\n\
             sudo pacman -S gliff",
        ),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("install", "Install for This User")]);
    dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("install"));
    dialog.set_close_response("cancel");
    dialog.choose_future(Some(window)).await == "install"
}

/// Ask whether to replace the too old gliff-server on `host` with this
/// computer's; `true` if the user agreed.
pub async fn confirm_update(window: &impl IsA<gtk::Widget>, host: &str) -> bool {
    let dialog = adw::AlertDialog::new(
        Some(&format!("{host} runs an incompatible version of gliff")),
        Some(
            "Its gliff-server is too old to be controlled from here. gliff can install \
             this computer's version for your user on that machine, in ~/.local/bin. \
             No password or sudo is needed, and connecting to it keeps working.",
        ),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("install", "Install This Version")]);
    dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("install"));
    dialog.set_close_response("cancel");
    dialog.choose_future(Some(window)).await == "install"
}

/// Whether `server_bin` on `host` can be controlled, or `None` when the
/// machine did not say, which the connection itself will then report.
pub async fn supports_control(host: &str, server_bin: &str) -> Option<bool> {
    let command = format!("{server_bin} --help");
    let argv = ssh_argv(host, &[], &[&command]);
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    let output = glib::future_with_timeout(PROBE_TIMEOUT, process.communicate_utf8_future(None));
    let Ok(Ok((stdout, _))) = output.await else {
        process.force_exit();
        return None;
    };
    process
        .is_successful()
        .then(|| stdout.as_deref().unwrap_or("").contains("--control"))
}

/// Run the installer on `host` with the session's extra ssh options.
pub async fn install(host: &str, ssh_args: &[String]) -> Outcome {
    run_script(host, ssh_args, &[]).await
}

/// Copy this computer's gliff-server to `host`, where the script checks that
/// it runs before it replaces the old one.
pub async fn update_server(host: &str) -> Outcome {
    let arch = match machine_arch(host).await {
        Ok(arch) => arch,
        Err(why) => return Outcome::Failed(why),
    };
    let server = match local_server(&arch) {
        Ok(server) => server,
        Err(why) => return Outcome::Failed(why),
    };
    let size = match std::fs::metadata(&server) {
        Ok(meta) => meta.len().to_string(),
        Err(e) => return Outcome::Failed(format!("Cannot read {}: {e}", server.display())),
    };
    let command = format!("mkdir -p ~/.local/bin && cat > {UPLOAD}");
    let argv = ssh_argv(host, &[], &[&command]);
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDERR_PIPE);
    launcher.set_stdin_file_path(Some(&server));
    let process = match launcher.spawn(&argv) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(format!("Cannot run ssh: {e}")),
    };
    let output = glib::future_with_timeout(TIMEOUT, process.communicate_utf8_future(None)).await;
    match output {
        Ok(Ok((_, stderr))) if !process.is_successful() => {
            let stderr = stderr.as_deref().unwrap_or("");
            let why = stderr.lines().rev().find(|l| !l.trim().is_empty());
            return Outcome::Failed(why.unwrap_or("The upload failed").trim().to_string());
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Outcome::Failed(format!("ssh failed: {e}")),
        Err(_) => {
            process.force_exit();
            return Outcome::Failed("The upload timed out".into());
        }
    }
    run_script(host, &[], &[&size]).await
}

/// What `uname -m` says on `host`.
async fn machine_arch(host: &str) -> Result<String, String> {
    let argv = ssh_argv(host, &[], &["uname -m"]);
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .map_err(|e| format!("Cannot run ssh: {e}"))?;
    let output = glib::future_with_timeout(PROBE_TIMEOUT, process.communicate_utf8_future(None));
    let Ok(Ok((stdout, _))) = output.await else {
        process.force_exit();
        return Err("That machine did not say what it runs on".into());
    };
    let stdout = stdout.as_deref().unwrap_or("");
    stdout
        .lines()
        .last()
        .filter(|_| process.is_successful())
        .map(|arch| arch.trim().to_string())
        .filter(|arch| !arch.is_empty())
        .ok_or_else(|| "That machine did not say what it runs on".into())
}

/// A gliff-server that runs on `arch`: the one beside this program or on
/// PATH for this computer's own, otherwise the one kept for that architecture.
fn local_server(arch: &str) -> Result<PathBuf, String> {
    if arch != std::env::consts::ARCH {
        let kept = glib::user_data_dir()
            .join("gliff/servers")
            .join(arch)
            .join("gliff-server");
        return if kept.is_file() {
            Ok(kept)
        } else {
            Err(format!(
                "That machine is {arch} and this computer is {}: this computer has no \
                 gliff-server for it. Keep one at {}",
                std::env::consts::ARCH,
                kept.display()
            ))
        };
    }
    let beside = std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("gliff-server"))
        .filter(|path| path.is_file());
    let on_path = || {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("gliff-server"))
            .find(|path| path.is_file())
    };
    beside
        .or_else(on_path)
        .ok_or_else(|| "gliff-server was not found on this computer".to_string())
}

fn ssh_argv(host: &str, ssh_args: &[String], command: &[&str]) -> Vec<std::ffi::OsString> {
    let mut argv: Vec<std::ffi::OsString> = ["ssh", "-T", "-o", "BatchMode=yes"]
        .iter()
        .map(Into::into)
        .collect();
    argv.extend(ssh_args.iter().map(Into::into));
    argv.extend(["--", host].iter().map(Into::into));
    argv.extend(command.iter().map(Into::into));
    argv
}

/// Run the script on `host`, with `args` after it.
async fn run_script(host: &str, ssh_args: &[String], args: &[&str]) -> Outcome {
    let mut command = vec!["python3", "-I", "-"];
    command.extend(args);
    let argv = ssh_argv(host, ssh_args, &command);
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let process = match gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDIN_PIPE
            | gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(format!("Cannot run ssh: {e}")),
    };
    let output = glib::future_with_timeout(
        TIMEOUT,
        process.communicate_utf8_future(Some(SCRIPT.to_string())),
    )
    .await;
    let (stdout, stderr) = match output {
        Ok(Ok((stdout, stderr))) => (stdout, stderr),
        Ok(Err(e)) => return Outcome::Failed(format!("ssh failed: {e}")),
        Err(_) => {
            process.force_exit();
            return Outcome::Failed("The installation timed out".into());
        }
    };
    outcome(
        stdout.as_deref().unwrap_or(""),
        stderr.as_deref().unwrap_or(""),
    )
}

fn outcome(stdout: &str, stderr: &str) -> Outcome {
    let reply = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Reply>(line).ok());
    let Some(reply) = reply else {
        // No python3, or ssh itself failed: its last line says why.
        let why = stderr.lines().rev().find(|l| !l.trim().is_empty());
        return Outcome::Failed(match why {
            Some(line) if line.contains("python3") => {
                "python3 is missing on that machine".to_string()
            }
            Some(line) => line.trim().to_string(),
            None => "The installer gave no answer".to_string(),
        });
    };
    match reply {
        Reply {
            ok: false, message, ..
        } => Outcome::Failed(message),
        Reply {
            on_path: Some(false),
            ..
        } => Outcome::Failed(
            "gliff was installed in ~/.local/bin, but ssh sessions on that machine \
             do not have it on PATH"
                .into(),
        ),
        Reply {
            status, version, ..
        } if status == "installed" => Outcome::Ready(format!("Installed gliff {version}")),
        _ => Outcome::Ready("gliff is already installed".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The script's answer to adopting `upload`, with `home` as its home.
    fn adopt(home: &std::path::Path, upload: &str) -> Outcome {
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let uploaded = bin.join(".gliff-server.upload");
        std::fs::write(&uploaded, upload).unwrap();
        let mut python = Command::new("python3")
            .args(["-I", "-", &upload.len().to_string()])
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        python
            .stdin
            .take()
            .unwrap()
            .write_all(SCRIPT.as_bytes())
            .unwrap();
        let output = python.wait_with_output().unwrap();
        assert!(!uploaded.exists(), "the upload is cleaned up");
        outcome(&String::from_utf8_lossy(&output.stdout), "")
    }

    #[test]
    fn a_new_server_replaces_only_what_gliff_installed() {
        let home = std::env::temp_dir().join(format!("gliff-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let server = home.join(".local/bin/gliff-server");
        let good = "#!/bin/sh\necho --control; echo gliff-server 9.9\n";

        // An upload that cannot run changes nothing.
        let broken = "#!/nonexistent/interpreter\n";
        assert!(matches!(adopt(&home, broken), Outcome::Failed(_)));
        assert!(!server.exists());
        // Without the feature it is no better than what is there.
        let old = "#!/bin/sh\necho gliff-server 0.1\n";
        assert!(matches!(adopt(&home, old), Outcome::Failed(_)));
        assert!(!server.exists());

        assert_eq!(
            adopt(&home, good),
            Outcome::Ready("Installed gliff 9.9".into())
        );
        assert_eq!(std::fs::read_to_string(&server).unwrap(), good);
        // gliff put that one there, so it may replace it.
        let newer = "#!/bin/sh\necho --control; echo gliff-server 10\n";
        assert!(matches!(adopt(&home, newer), Outcome::Ready(_)));
        assert_eq!(std::fs::read_to_string(&server).unwrap(), newer);
        // The user's own is left alone.
        std::fs::write(&server, "mine").unwrap();
        assert!(matches!(adopt(&home, good), Outcome::Failed(_)));
        assert_eq!(std::fs::read_to_string(&server).unwrap(), "mine");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn another_architecture_needs_a_server_of_its_own() {
        let why = local_server("no-such-arch").unwrap_err();
        assert!(why.contains("no-such-arch"), "{why}");
        assert!(
            why.contains("gliff/servers/no-such-arch/gliff-server"),
            "{why}"
        );
    }

    #[test]
    fn replies_map_to_outcomes() {
        let installed = r#"{"ok":true,"status":"installed","version":"0.4.1-1","path":"/h/.local/bin/gliff-server","on_path":true}"#;
        assert_eq!(
            outcome(installed, ""),
            Outcome::Ready("Installed gliff 0.4.1-1".into())
        );
        assert_eq!(
            outcome(
                r#"{"ok":true,"status":"present","path":"/usr/bin/gliff-server"}"#,
                ""
            ),
            Outcome::Ready("gliff is already installed".into())
        );
        let refused =
            r#"{"ok":false,"code":"hyprland","message":"gliff needs Hyprland on that machine"}"#;
        assert_eq!(
            outcome(refused, ""),
            Outcome::Failed("gliff needs Hyprland on that machine".into())
        );
        let off_path = r#"{"ok":true,"status":"installed","version":"1","on_path":false}"#;
        assert!(matches!(outcome(off_path, ""), Outcome::Failed(_)));
    }

    #[test]
    fn without_a_reply_the_ssh_error_is_shown() {
        assert_eq!(
            outcome("", "bash: line 1: python3: command not found\n"),
            Outcome::Failed("python3 is missing on that machine".into())
        );
        assert_eq!(
            outcome(
                "motd\n",
                "ssh: connect to host h port 22: No route to host\n"
            ),
            Outcome::Failed("ssh: connect to host h port 22: No route to host".into())
        );
    }
}
