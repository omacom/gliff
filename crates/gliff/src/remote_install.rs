//! Offer to install gliff on a machine that does not have it.
//!
//! The work happens on the remote, over the same ssh the session uses: a
//! script read from stdin by `python3`, so no host or path is ever part of a
//! remote shell command. It installs the signed Omarchy package for the user
//! only; a system-wide install stays the user's own `sudo pacman -S gliff`.

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;
use serde::Deserialize;
use std::time::Duration;

const SCRIPT: &str = include_str!("remote_install.py");
/// Downloading and checking a few megabytes on a slow link.
const TIMEOUT: Duration = Duration::from_secs(180);

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

/// Run the installer on `host` with the session's extra ssh options.
pub async fn install(host: &str, ssh_args: &[String]) -> Outcome {
    let mut argv: Vec<&std::ffi::OsStr> = Vec::new();
    for arg in ["ssh", "-T", "-o", "BatchMode=yes"] {
        argv.push(arg.as_ref());
    }
    argv.extend(ssh_args.iter().map(std::ffi::OsStr::new));
    for arg in ["--", host, "python3", "-I", "-"] {
        argv.push(arg.as_ref());
    }
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
