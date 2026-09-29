//! Files in the controller's state directory. It's on the router's flash, and some files
//! hold secrets (keys in configurations), so every write is atomic (a new temporary file,
//! fsync, rename, fsync of the directory) and private (0600), and nothing is written on a
//! timer that could be written on an event instead.

use serde::Serialize;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir).map_err(io)?;
    // The mode applies only to a file that's created: one left over (a crash, a hand edit)
    // would keep its own, and pass it on to the file it replaces.
    let tmp = path.with_extension("tmp");
    remove(&tmp)?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io)?;
    f.write_all(data).map_err(io)?;
    f.sync_all().map_err(io)?;
    fs::rename(&tmp, path).map_err(io)?;
    // The rename is on flash once the directory is.
    fs::File::open(dir).and_then(|d| d.sync_all()).map_err(io)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    write(path, &data)
}

/// Removes a file; one that isn't there is already removed.
pub fn remove(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

/// A serial is a MAC without separators, as the agent sends it: 12 lower-case hex digits,
/// never a path. Upper case would make one MAC two devices.
pub fn valid_serial(serial: &str) -> bool {
    serial.len() == 12
        && serial
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_write_is_private_whatever_was_there() {
        let dir = std::env::temp_dir().join(format!("steward-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        // A new file, in a directory that isn't there yet.
        write(&path, b"{}").unwrap();
        assert_eq!(mode(&path), 0o600);
        // A readable file, and a readable temporary file left behind.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let tmp = dir.join("devices.tmp");
        fs::write(&tmp, "stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}");
        assert!(!tmp.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_serial_is_12_lower_case_hex_digits() {
        assert!(valid_serial("00005e0053a1"));
        for not in [
            "00005E0053A1",
            "00005e0053A1",
            "00005e0053a",
            "00005e0053a1f",
            "00005e0053g1",
            "../../etc/pa",
            "",
        ] {
            assert!(!valid_serial(not), "{not}");
        }
    }
}
