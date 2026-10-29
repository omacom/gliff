//! Who is viewing this machine, for the loop guard (see [`gliff_proto::Origin`]).
//!
//! A server records each client it serves in a file under
//! `$XDG_RUNTIME_DIR/gliff/`, named by its process id and removed when the
//! session ends. A client on the same machine lists the files in its `Hello`.
//! A file whose process is gone (a server killed before it could clean up) is
//! ignored and swept.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gliff_proto::Viewer;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// An opaque identity of this machine, for the loop guard. machine-id(5) says
/// the id must not appear on the network, so this is the construction of
/// `sd_id128_get_app_specific(3)`: HMAC-SHA256 of a fixed gliff application id,
/// keyed by the machine id, truncated to 128 bits and sent as hex. `None` when
/// the machine has no id.
pub fn machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .map(|id| id.trim().to_owned())
        .find(|id| !id.is_empty())
        .map(|id| app_specific_id(&id))
}

/// Fixed key material naming this use of the machine id.
const APP_ID: &[u8] = b"gliff loop guard machine identity v1";

fn app_specific_id(id: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&machine_id_bytes(id))
        .expect("HMAC accepts a key of any length");
    mac.update(APP_ID);
    mac.finalize().into_bytes()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The 16 bytes of a hex machine id; the text itself for anything else.
fn machine_id_bytes(id: &str) -> Vec<u8> {
    let hex = |i: usize| u8::from_str_radix(id.get(i..i + 2)?, 16).ok();
    if id.len() == 32 {
        if let Some(bytes) = (0..16).map(|i| hex(i * 2)).collect() {
            return bytes;
        }
    }
    id.as_bytes().to_vec()
}

/// The directory under the user's runtime directory (`hypr_ipc::runtime_dir`,
/// which also covers ssh sessions that start without `XDG_RUNTIME_DIR`).
fn dir_in(runtime: &Path) -> PathBuf {
    runtime.join("gliff")
}

/// The record of one served viewer; dropping it removes the file.
pub struct Registration {
    path: PathBuf,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Record that `machine` is viewing `output` of this machine. `None` when
/// the directory cannot be written.
pub fn register(runtime: &Path, machine: &str, output: &str) -> Option<Registration> {
    register_in(&dir_in(runtime), std::process::id(), machine, output)
        .map_err(|e| tracing::warn!(error = %e, "cannot record the viewer; the loop guard is off for it"))
        .ok()
}

fn register_in(dir: &Path, pid: u32, machine: &str, output: &str) -> io::Result<Registration> {
    fs::create_dir_all(dir)?;
    let path = dir.join(pid.to_string());
    let started =
        start_time(pid).ok_or_else(|| io::Error::other("no start time for the process"))?;
    // Written under a name the readers skip, then renamed, so a reader never
    // sees a record that is still empty.
    let partial = dir.join(format!(".{pid}"));
    fs::write(&partial, format!("{machine}\n{output}\n{started}\n"))?;
    fs::rename(&partial, &path)?;
    Ok(Registration { path })
}

/// When process `pid` started, in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). With the pid it names one process, so a reused pid
/// does not pass for the server that wrote a record.
fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may hold spaces and parentheses; skip past its end.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The machines viewing this one right now.
pub fn viewers(runtime: &Path) -> Vec<Viewer> {
    viewers_in(&dir_in(runtime))
}

fn viewers_in(dir: &Path) -> Vec<Viewer> {
    entries_in(dir).into_iter().map(|(_, v)| v).collect()
}

fn entries_in(dir: &Path) -> Vec<(u32, Viewer)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let pid = entry
            .file_name()
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok());
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut lines = text.lines();
        let (machine, output, started) = (lines.next(), lines.next(), lines.next());
        // Alive means the same process still runs under the pid, not just that
        // the pid is taken.
        let pid = pid.filter(|pid| {
            let started = started.and_then(|s| s.parse::<u64>().ok());
            started.is_some() && started == start_time(*pid)
        });
        let Some(pid) = pid else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if let (Some(machine), Some(output)) = (machine, output) {
            found.push((
                pid,
                Viewer {
                    machine: machine.to_owned(),
                    output: output.to_owned(),
                },
            ));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_specific_ids_are_stable_and_distinct() {
        let (a, b) = (
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdee",
        );
        assert_eq!(app_specific_id(a), app_specific_id(a));
        assert_ne!(app_specific_id(a), app_specific_id(b));
        assert_eq!(app_specific_id(a).len(), 32);
        assert!(!app_specific_id(a).contains(a));
        // Not a hex id: keyed by its text instead of failing.
        assert_ne!(app_specific_id("abc"), app_specific_id("abd"));
    }

    #[test]
    fn a_hex_machine_id_keys_with_its_16_bytes() {
        assert_eq!(
            machine_id_bytes("000102030405060708090a0b0c0d0e0f"),
            (0u8..16).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_registration_lasts_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let reg = register_in(dir.path(), std::process::id(), "m", "gliff-1").unwrap();
        assert_eq!(
            viewers_in(dir.path()),
            vec![Viewer {
                machine: "m".into(),
                output: "gliff-1".into()
            }]
        );
        drop(reg);
        assert!(viewers_in(dir.path()).is_empty());
    }

    #[test]
    fn a_dead_process_is_ignored_and_swept() {
        let dir = tempfile::tempdir().unwrap();
        // The kernel's pid limit is below this, so no such process exists.
        let ghost = dir.path().join("4294967295");
        fs::write(&ghost, "m\nDP-1\n").unwrap();
        assert!(viewers_in(dir.path()).is_empty());
        assert!(!ghost.exists());
    }

    #[test]
    fn a_record_in_the_making_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join(format!(".{}", std::process::id()));
        fs::write(&partial, "").unwrap();
        assert!(viewers_in(dir.path()).is_empty());
        assert!(partial.exists());
    }

    #[test]
    fn a_reused_pid_is_not_the_original_server() {
        let dir = tempfile::tempdir().unwrap();
        // A live pid recorded with another process's start time.
        let ours = dir.path().join(std::process::id().to_string());
        let started = start_time(std::process::id()).unwrap();
        fs::write(&ours, format!("m\nDP-1\n{}\n", started + 1)).unwrap();
        assert!(viewers_in(dir.path()).is_empty());
        assert!(!ours.exists());
    }
}
