//! The latest state of each adopted device. States arrive every minute or so, and the state
//! directory is on flash, so they're kept in memory as they arrive and written to
//! `<state dir>/state/<serial>.json` only when their device disconnects, hourly when they
//! changed, and when the controller stops. Anything that connects is pending, so a pending
//! device's state isn't kept at all, in memory or on flash: what it sends can't grow the
//! controller's memory, and its page shows it's pending, not its state.

use crate::store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Latest {
    /// When it arrived.
    pub received: u64,
    /// The configuration the device ran.
    pub uuid: u64,
    pub state: Value,
}

pub struct States {
    dir: PathBuf,
    latest: BTreeMap<String, Latest>,
    /// Changed since written, and to be kept.
    dirty: BTreeSet<String>,
}

impl States {
    /// Every stored state in `dir`. A file that can't be read is skipped, and named in the
    /// second value: a damaged state must not keep the controller from starting.
    pub fn load(dir: &Path) -> (States, Vec<String>) {
        let mut latest = BTreeMap::new();
        let mut skipped = vec![];
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(serial) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|s| store::valid_serial(s))
            else {
                continue;
            };
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|d| serde_json::from_slice::<Latest>(&d).map_err(|e| e.to_string()))
            {
                Ok(l) => {
                    latest.insert(serial.to_owned(), l);
                }
                Err(e) => skipped.push(format!("{}: {e}", path.display())),
            }
        }
        let states = States {
            dir: dir.to_owned(),
            latest,
            dirty: BTreeSet::new(),
        };
        (states, skipped)
    }

    fn path(&self, serial: &str) -> PathBuf {
        self.dir.join(format!("{serial}.json"))
    }

    /// A state arrived. It's kept only when the device is `adopted`, and written later.
    pub fn record(&mut self, serial: &str, uuid: u64, state: Value, adopted: bool) {
        if !adopted || !store::valid_serial(serial) {
            return;
        }
        let latest = Latest {
            received: store::now(),
            uuid,
            state,
        };
        self.latest.insert(serial.to_owned(), latest);
        self.dirty.insert(serial.to_owned());
    }

    /// Drops the states of devices that aren't on record (`known` is false for them: a forget
    /// whose removal failed, a devices.json restored or edited), stored and in memory; returns
    /// what failed.
    pub fn retain(&mut self, known: impl Fn(&str) -> bool) -> Vec<String> {
        let unknown: Vec<String> = self.latest.keys().filter(|s| !known(s)).cloned().collect();
        unknown
            .iter()
            .filter_map(|s| self.forget(s).err())
            .collect()
    }

    pub fn get(&self, serial: &str) -> Option<&Latest> {
        self.latest.get(serial)
    }

    /// Writes the device's state if it changed since it was last written.
    pub fn flush(&mut self, serial: &str) -> Result<(), String> {
        if !self.dirty.contains(serial) {
            return Ok(());
        }
        if let Some(l) = self.latest.get(serial) {
            store::write_json(&self.path(serial), l)?;
        }
        self.dirty.remove(serial);
        Ok(())
    }

    /// Writes every changed state; how many were written, and what failed.
    pub fn flush_all(&mut self) -> (usize, Vec<String>) {
        let (mut written, mut failed) = (0, vec![]);
        for serial in self.dirty.clone() {
            match self.flush(&serial) {
                Ok(()) => written += 1,
                Err(e) => failed.push(e),
            }
        }
        (written, failed)
    }

    /// Drops the device's state, stored and in memory.
    pub fn forget(&mut self, serial: &str) -> Result<(), String> {
        self.latest.remove(serial);
        self.dirty.remove(serial);
        if store::valid_serial(serial) {
            store::remove(&self.path(serial))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("steward-states-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_state_is_written_only_when_flushed() {
        let d = dir("flush");
        let (mut s, _) = States::load(&d);
        s.record(
            "00005e005301",
            7,
            json!({ "unit": { "load": [0.1] } }),
            true,
        );
        s.record("00005e005302", 7, json!({}), false);
        // A state message writes nothing.
        assert!(!d.exists());
        assert_eq!(s.flush_all(), (1, vec![]));
        // The pending device's state isn't kept at all.
        assert!(d.join("00005e005301.json").exists());
        assert!(s.get("00005e005302").is_none());
        assert!(!d.join("00005e005302.json").exists());
        // Unchanged since: nothing to write.
        assert_eq!(s.flush_all(), (0, vec![]));
        let mode = std::fs::metadata(d.join("00005e005301.json"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );

        // After a restart, the written one is back as it was.
        let (again, skipped) = States::load(&d);
        assert!(skipped.is_empty());
        assert_eq!(again.get("00005e005301"), s.get("00005e005301"));
        assert!(again.get("00005e005302").is_none());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_damaged_state_is_skipped() {
        let d = dir("damaged");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("00005e005301.json"), "{ truncated").unwrap();
        std::fs::write(d.join("not-a-serial.json"), "{}").unwrap();
        let (mut s, _) = States::load(&d);
        s.record("00005e005302", 1, json!({}), true);
        s.flush_all();
        let (again, skipped) = States::load(&d);
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("00005e005301.json"));
        assert!(again.get("00005e005302").is_some());
        std::fs::remove_dir_all(d).unwrap();
    }

    /// Anything that connects is pending: however many and however big, what they send
    /// isn't kept, and one on record keeps its state.
    #[test]
    fn a_pending_devices_state_is_not_kept() {
        let d = dir("pending");
        let (mut s, _) = States::load(&d);
        s.record("00005e005301", 1, json!({ "unit": {} }), true);
        let big = json!({ "junk": "x".repeat(256 * 1024) });
        for i in 0..100 {
            s.record(&format!("00005e{i:06x}"), 0, big.clone(), false);
        }
        assert_eq!(s.latest.len(), 1);
        assert_eq!(s.get("00005e005301").unwrap().state, json!({ "unit": {} }));
        assert_eq!(s.flush_all(), (1, vec![]));
        std::fs::remove_dir_all(d).unwrap();
    }

    /// A stored state whose device isn't on record goes, from memory and flash.
    #[test]
    fn states_of_devices_not_on_record_are_dropped() {
        let d = dir("retain");
        let (mut s, _) = States::load(&d);
        s.record("00005e005301", 1, json!({}), true);
        s.record("00005e005302", 1, json!({}), true);
        s.flush_all();
        let (mut s, _) = States::load(&d);
        assert!(s.retain(|serial| serial == "00005e005301").is_empty());
        assert!(s.get("00005e005301").is_some() && s.get("00005e005302").is_none());
        assert!(d.join("00005e005301.json").exists());
        assert!(!d.join("00005e005302.json").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn forgetting_removes_the_stored_state() {
        let d = dir("forget");
        let (mut s, _) = States::load(&d);
        s.record("00005e005301", 1, json!({}), true);
        s.flush("00005e005301").unwrap();
        s.forget("00005e005301").unwrap();
        assert!(s.get("00005e005301").is_none());
        assert!(!d.join("00005e005301.json").exists());
        // Forgetting one that has none is fine.
        s.forget("00005e005309").unwrap();
        std::fs::remove_dir_all(d).unwrap();
    }
}
