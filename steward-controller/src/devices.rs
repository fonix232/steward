//! Which devices the controller manages, kept in `<state dir>/devices.json`.
//!
//! A device's first connection makes it **pending**: it stays connected so the controller
//! knows it's there, and it is sent nothing. Adopting it makes it **adopting**: the next
//! time it's connected, the controller issues a credential (a random token) and sends it
//! with `steward.adopt`. Once the device confirms, it is **adopted**, and from then on it
//! must present that credential on every connection; the controller keeps only its SHA-256.
//! Forgetting a device drops its record, and with it the credential.
//!
//! Anything that connects becomes pending, so pending devices are kept in memory only (a
//! controller restart forgets them until they connect again), at most [`MAX_PENDING`] of
//! them, and what they report is cut to [`MAX_TEXT`] bytes; their states aren't kept at all
//! (`states`). Room is made by dropping the oldest pending device that isn't connected, and
//! only when all are connected the oldest one, disconnected with its record
//! ([`Devices::take_dropped`]): no more than that many stay connected either, and devices that
//! connect and hang up can't push out one that's there. Only adopting and adopted devices,
//! which someone approved, are written to the file, on the router's flash. A connection
//! refused for its credential changes nothing.
//!
//! An adopted device's record also keeps what it reported on connecting (its capabilities),
//! when it was last connected and last seen, the address it connected from, and the answer
//! to its last `configure`. The file is on flash: it's written on those events only, and
//! only when something changed.

use crate::store::{self, now};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use steward_proto::CommandStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    Pending,
    Adopting,
    Adopted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub standing: Standing,
    /// SHA-256 (hex) of the credential issued to the device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_sha256: Option<String>,
    pub first_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// What it reported on connecting (adopted devices only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<u64>,
    /// When it connected or disconnected last; the controller hears from a connected device
    /// every minute or so, so while connected this is when it connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    /// The address it last connected from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_answer: Option<Answer>,
    /// It took its credential over loopback: the controller's own host, which `--adopt-local`
    /// may adopt again. A device adopted from the network never is.
    #[serde(default, skip_serializing_if = "is_false")]
    pub adopted_over_loopback: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// A device's answer to `configure`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The configuration it answered for.
    pub uuid: u64,
    pub status: CommandStatus,
    /// When it arrived.
    pub at: u64,
}

/// What a connecting device gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Not adopted: connected for the controller's bookkeeping, sent nothing.
    Pending,
    /// Adopted, credential not delivered yet: deliver this one, then call [`Devices::delivered`].
    Deliver(String),
    /// Adopted, and it presented its credential.
    Admitted,
    /// Adopted, and the credential is missing or wrong.
    Refused,
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Pending devices kept, at most: past it, the oldest one that isn't connected is dropped,
/// or when all are, the oldest (and disconnected).
pub const MAX_PENDING: usize = 64;
/// The longest serial accepted, and model or firmware kept, in bytes.
const MAX_TEXT: usize = 128;

pub struct Devices {
    path: PathBuf,
    records: BTreeMap<String, Record>,
    /// Pending devices' serials, oldest first. Their records are in memory only.
    pending: VecDeque<String>,
    /// Pending devices dropped to make room, whose connections are still to be closed.
    dropped: Vec<String>,
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new credential: 32 random bytes, as hex.
fn new_credential() -> Result<String, Error> {
    let mut b = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
        .map_err(|_| Error("no randomness for a credential".into()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Equal hashes, compared in constant time.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Whether `credential` is the one issued to the device.
fn presents(record: &Record, credential: Option<&str>) -> bool {
    match (credential, &record.credential_sha256) {
        (Some(c), Some(hash)) => same(&sha256_hex(c.as_bytes()), hash),
        _ => false,
    }
}

/// What a device reported, cut to [`MAX_TEXT`] bytes (at a character boundary).
fn clip(s: &str) -> String {
    let mut end = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

impl Devices {
    /// The registry at `path`, empty when there's none. One that can't be read is an error,
    /// and stops the controller: starting empty would drop every adoption, and the next write
    /// would replace the file.
    pub fn load(path: &Path) -> Result<Devices, Error> {
        let mut records: BTreeMap<String, Record> = match fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).map_err(|e| {
                Error(format!(
                    "{}: unreadable ({e}). It holds every adoption, so the controller won't \
                     start without it: repair or restore it, or move it away to start with no \
                     devices",
                    path.display()
                ))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(Error(format!("{}: {e}", path.display()))),
        };
        // Pending devices aren't kept across restarts: they're back when they next connect.
        records.retain(|_, r| r.standing != Standing::Pending);
        Ok(Devices {
            path: path.to_owned(),
            records,
            pending: VecDeque::new(),
            dropped: Vec::new(),
        })
    }

    /// Writes the adopting and adopted devices: pending ones stay in memory.
    fn save(&self) -> Result<(), Error> {
        let kept: BTreeMap<&String, &Record> = self
            .records
            .iter()
            .filter(|(_, r)| r.standing != Standing::Pending)
            .collect();
        store::write_json(&self.path, &kept).map_err(Error)
    }

    pub fn get_standing(&self, serial: &str) -> Option<Standing> {
        self.records.get(serial).map(|r| r.standing)
    }

    pub fn all(&self) -> &BTreeMap<String, Record> {
        &self.records
    }

    pub fn is_adopted(&self, serial: &str) -> bool {
        self.records
            .get(serial)
            .is_some_and(|r| r.standing == Standing::Adopted)
    }

    /// A device connected, presenting `credential` if it has one. `connected` tells which
    /// others are connected now, for making room among the pending ones. What it reports
    /// (model, firmware) is taken only when it's let in: a refused connection changes nothing,
    /// in memory or on flash, since anyone can claim an adopted device's serial (its MAC).
    pub fn admit(
        &mut self,
        serial: &str,
        credential: Option<&str>,
        model: Option<&str>,
        firmware: &str,
        connected: impl Fn(&str) -> bool,
    ) -> Result<Admission, Error> {
        if serial.len() > MAX_TEXT {
            return Err(Error(format!("a serial of {} bytes", serial.len())));
        }
        let before = self.records.get(serial).cloned();
        if let Some(r) = &before
            && r.standing == Standing::Adopted
            && !presents(r, credential)
        {
            return Ok(Admission::Refused);
        }
        if before.is_none() {
            self.pending.retain(|s| s != serial);
            self.pending.push_back(serial.to_owned());
        }
        let record = self
            .records
            .entry(serial.to_owned())
            .or_insert_with(|| Record {
                standing: Standing::Pending,
                credential_sha256: None,
                first_seen: now(),
                adopted_at: None,
                model: None,
                firmware: None,
                adopted_over_loopback: false,
                capabilities: None,
                last_connected: None,
                last_seen: None,
                address: None,
                last_answer: None,
            });
        record.model = model.map(clip).or(record.model.take());
        record.firmware = Some(clip(firmware));
        let admission = match record.standing {
            Standing::Pending => Admission::Pending,
            Standing::Adopting => {
                let credential = new_credential()?;
                record.credential_sha256 = Some(sha256_hex(credential.as_bytes()));
                Admission::Deliver(credential)
            }
            // It presented its credential: refusals are decided above.
            Standing::Adopted => Admission::Admitted,
        };
        // devices.json lives on flash: write only what changed, and never for a pending
        // device (anything that connects is one).
        let after = &self.records[serial];
        if after.standing != Standing::Pending && before.as_ref() != Some(after) {
            self.save()?;
        }
        // This one is connecting now.
        self.trim(|s| s == serial || connected(s));
        Ok(admission)
    }

    /// Drops pending devices past [`MAX_PENDING`]: the oldest that isn't `connected`, or when
    /// all are, the oldest.
    fn trim(&mut self, connected: impl Fn(&str) -> bool) {
        let records = &self.records;
        self.pending.retain(|s| {
            records
                .get(s)
                .is_some_and(|r| r.standing == Standing::Pending)
        });
        while self.pending.len() > MAX_PENDING {
            let i = self.pending.iter().position(|s| !connected(s));
            if let Some(dropped) = self.pending.remove(i.unwrap_or(0)) {
                self.records.remove(&dropped);
                self.dropped.push(dropped);
            }
        }
    }

    /// The pending devices dropped to make room since the last call. The caller closes the
    /// connections of those that are connected, so records and connections go together: a
    /// connected device is always one that's listed (and can be adopted), and at most
    /// [`MAX_PENDING`] pending devices are connected. A dropped device is pending again when it
    /// reconnects.
    pub fn take_dropped(&mut self) -> Vec<String> {
        std::mem::take(&mut self.dropped)
    }

    /// Approve a pending device. It gets its credential the next time it's connected.
    pub fn adopt(&mut self, serial: &str) -> Result<(), Error> {
        let record = self
            .records
            .get_mut(serial)
            .ok_or_else(|| Error(format!("no device {serial} has connected")))?;
        match record.standing {
            Standing::Adopted => return Err(Error(format!("{serial} is already adopted"))),
            Standing::Adopting => {}
            Standing::Pending => record.standing = Standing::Adopting,
        }
        self.save()
    }

    /// Adopt the controller's own host (`--adopt-local`) without asking: pending, or adopted
    /// over loopback before and it lost its credential. Its old credential stops working, and
    /// [`Devices::issue`] gives it a new one. A device adopted from the network is never taken
    /// over this way, whoever claims its serial: false, and it keeps its credential.
    pub fn readopt(&mut self, serial: &str) -> Result<bool, Error> {
        let record = self
            .records
            .get_mut(serial)
            .ok_or_else(|| Error(format!("no device {serial} has connected")))?;
        if record.standing == Standing::Adopted && !record.adopted_over_loopback {
            return Ok(false);
        }
        record.standing = Standing::Adopting;
        record.credential_sha256 = None;
        self.save()?;
        Ok(true)
    }

    /// Issue a credential to a device being adopted that is connected now.
    pub fn issue(&mut self, serial: &str) -> Result<Option<String>, Error> {
        match self.records.get_mut(serial) {
            Some(r) if r.standing == Standing::Adopting => {
                let credential = new_credential()?;
                r.credential_sha256 = Some(sha256_hex(credential.as_bytes()));
                self.save()?;
                Ok(Some(credential))
            }
            _ => Ok(None),
        }
    }

    /// The device confirmed it stored its credential, on a connection over loopback or not.
    pub fn delivered(&mut self, serial: &str, over_loopback: bool) -> Result<(), Error> {
        if let Some(r) = self.records.get_mut(serial)
            && r.standing == Standing::Adopting
            && r.credential_sha256.is_some()
        {
            r.standing = Standing::Adopted;
            r.adopted_at = Some(now());
            r.adopted_over_loopback = over_loopback;
            self.save()?;
        }
        Ok(())
    }

    /// An adopted device connected (or finished adopting) from `address`, reporting
    /// `capabilities`. Others' connections aren't recorded.
    pub fn connected(
        &mut self,
        serial: &str,
        address: &str,
        capabilities: &Value,
    ) -> Result<(), Error> {
        if let Some(r) = self.records.get_mut(serial)
            && r.standing == Standing::Adopted
        {
            let t = now();
            r.last_connected = Some(t);
            r.last_seen = Some(t);
            r.address = Some(address.to_owned());
            // Nothing (over the controller's limit): what it reported before stays.
            if !capabilities.is_null() {
                r.capabilities = Some(capabilities.clone());
            }
            self.save()?;
        }
        Ok(())
    }

    /// The controller last heard from these devices now: they disconnected, or it's
    /// stopping while they're connected. One write for all of them.
    pub fn seen(&mut self, serials: &[&str]) -> Result<(), Error> {
        let t = now();
        let mut changed = false;
        for serial in serials {
            if let Some(r) = self.records.get_mut(*serial)
                && r.standing == Standing::Adopted
            {
                r.last_seen = Some(t);
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// An adopted device answered `configure`.
    pub fn answered(
        &mut self,
        serial: &str,
        uuid: u64,
        status: CommandStatus,
    ) -> Result<(), Error> {
        if let Some(r) = self.records.get_mut(serial)
            && r.standing == Standing::Adopted
        {
            r.last_answer = Some(Answer {
                uuid,
                status,
                at: now(),
            });
            self.save()?;
        }
        Ok(())
    }

    /// Drop a device and its credential. True when there was one.
    pub fn forget(&mut self, serial: &str) -> Result<bool, Error> {
        let Some(forgotten) = self.records.remove(serial) else {
            return Ok(false);
        };
        if forgotten.standing != Standing::Pending {
            self.save()?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (PathBuf, Devices) {
        let dir =
            std::env::temp_dir().join(format!("steward-devices-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        let d = Devices::load(&path).unwrap();
        (dir, d)
    }

    /// No other device is connected.
    fn offline(_: &str) -> bool {
        false
    }

    #[test]
    fn a_device_goes_from_pending_to_adopted() {
        let (dir, mut d) = store("flow");
        let serial = "00005e005301";
        // First connection: pending, sent nothing, however often it reconnects.
        assert_eq!(
            d.admit(serial, None, Some("E8450"), "OpenWrt", offline)
                .unwrap(),
            Admission::Pending
        );
        assert_eq!(
            d.admit(serial, Some("guess"), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Pending
        );
        assert!(!d.is_adopted(serial));

        // Adopted while connected: a credential is issued; until it's confirmed, not adopted.
        d.adopt(serial).unwrap();
        let credential = d.issue(serial).unwrap().expect("a credential");
        assert_eq!(credential.len(), 64);
        assert!(!d.is_adopted(serial));
        d.delivered(serial, false).unwrap();
        assert!(d.is_adopted(serial));
        assert!(d.adopt(serial).is_err(), "adopting twice");

        // From now on the credential is required, and only it works.
        assert_eq!(
            d.admit(serial, Some(&credential), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(
            d.admit(serial, None, None, "OpenWrt", offline).unwrap(),
            Admission::Refused
        );
        // One character off: its last, changed to another hex digit.
        let last = if credential.ends_with('0') { '1' } else { '0' };
        let forged = format!("{}{last}", &credential[..63]);
        assert_ne!(forged, credential);
        assert_eq!(
            d.admit(serial, Some(&forged), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Refused
        );

        // It survives a restart, and the file holds no credential, only its hash.
        let again = Devices::load(&dir.join("devices.json")).unwrap();
        assert!(again.is_adopted(serial));
        assert_eq!(again.all()[serial].model.as_deref(), Some("E8450"));
        let text = fs::read_to_string(dir.join("devices.json")).unwrap();
        assert!(!text.contains(&credential));
        let mode = fs::metadata(dir.join("devices.json"))
            .unwrap()
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );

        // Forgotten: the credential is revoked, and the device starts over as pending.
        assert!(d.forget(serial).unwrap());
        assert_eq!(
            d.admit(serial, Some(&credential), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Pending
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_adoption_approved_while_offline_is_delivered_on_connect() {
        let (dir, mut d) = store("offline");
        let serial = "00005e005302";
        d.admit(serial, None, None, "OpenWrt", offline).unwrap();
        d.adopt(serial).unwrap();
        let Admission::Deliver(credential) =
            d.admit(serial, None, None, "OpenWrt", offline).unwrap()
        else {
            panic!("expected a credential to deliver")
        };
        // Not confirmed (the connection dropped): the next connection gets a new one, and
        // the undelivered one is worthless.
        let Admission::Deliver(second) = d
            .admit(serial, Some(&credential), None, "OpenWrt", offline)
            .unwrap()
        else {
            panic!("expected a new credential")
        };
        assert_ne!(credential, second);
        d.delivered(serial, false).unwrap();
        assert_eq!(
            d.admit(serial, Some(&credential), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Refused
        );
        assert_eq!(
            d.admit(serial, Some(&second), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Admitted
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readopting_replaces_the_credential() {
        let (dir, mut d) = store("readopt");
        let serial = "00005e005304";
        d.admit(serial, None, None, "OpenWrt", offline).unwrap();
        d.adopt(serial).unwrap();
        let old = d.issue(serial).unwrap().unwrap();
        d.delivered(serial, true).unwrap();
        // The host lost its credential: re-adopted, the old one no longer works.
        assert!(d.readopt(serial).unwrap());
        assert!(!d.is_adopted(serial));
        let new = d.issue(serial).unwrap().unwrap();
        d.delivered(serial, true).unwrap();
        assert_eq!(
            d.admit(serial, Some(&old), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Refused
        );
        assert_eq!(
            d.admit(serial, Some(&new), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Admitted
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_device_adopted_from_the_network_is_never_readopted() {
        let (dir, mut d) = store("network");
        let serial = "00005e005305";
        d.admit(serial, None, None, "OpenWrt", offline).unwrap();
        d.adopt(serial).unwrap();
        let credential = d.issue(serial).unwrap().unwrap();
        d.delivered(serial, false).unwrap();
        // Also after a restart: it keeps its standing and its credential.
        let mut d = Devices::load(&dir.join("devices.json")).unwrap();
        assert!(!d.all()[serial].adopted_over_loopback);
        assert!(!d.readopt(serial).unwrap());
        assert!(d.is_adopted(serial));
        assert_eq!(d.issue(serial).unwrap(), None);
        assert_eq!(
            d.admit(serial, Some(&credential), None, "OpenWrt", offline)
                .unwrap(),
            Admission::Admitted
        );
        // A pending device, and one adopted over loopback, can be.
        d.admit("00005e005306", None, None, "OpenWrt", offline)
            .unwrap();
        assert!(d.readopt("00005e005306").unwrap());
        d.issue("00005e005306").unwrap().unwrap();
        d.delivered("00005e005306", true).unwrap();
        let d = Devices::load(&dir.join("devices.json")).unwrap();
        assert!(d.all()["00005e005306"].adopted_over_loopback);
    }

    #[test]
    fn only_adopted_devices_connections_are_recorded() {
        let (dir, mut d) = store("connections");
        let serial = "00005e005305";
        d.admit(serial, None, None, "OpenWrt", offline).unwrap();
        // Pending devices live in memory only: there's no file yet.
        let before = fs::read(dir.join("devices.json")).ok();
        // Pending: nothing it reports beyond its record is kept, and nothing is written.
        d.connected(serial, "192.0.2.5", &serde_json::json!({ "model": "x" }))
            .unwrap();
        d.seen(&[serial]).unwrap();
        assert_eq!(fs::read(dir.join("devices.json")).ok(), before);
        assert!(d.all()[serial].capabilities.is_none());
        d.adopt(serial).unwrap();
        d.issue(serial).unwrap();
        d.delivered(serial, false).unwrap();
        d.connected(serial, "192.0.2.5", &serde_json::json!({ "model": "x" }))
            .unwrap();
        let again = Devices::load(&dir.join("devices.json")).unwrap();
        let r = &again.all()[serial];
        assert_eq!(r.address.as_deref(), Some("192.0.2.5"));
        assert!(r.last_connected.is_some() && r.capabilities.is_some());
        // Capabilities the controller didn't keep (over its limit) don't replace them.
        d.connected(serial, "192.0.2.6", &Value::Null).unwrap();
        let again = Devices::load(&dir.join("devices.json")).unwrap();
        let r = &again.all()[serial];
        assert_eq!(r.address.as_deref(), Some("192.0.2.6"));
        assert_eq!(r.capabilities, Some(serde_json::json!({ "model": "x" })));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Anyone can claim an adopted device's serial (its MAC, seen on the LAN): a connection
    /// refused for its credential changes nothing, in memory or on flash. An admitted one
    /// updates what the device reports, and a pending one in memory only.
    #[test]
    fn a_refused_connection_changes_nothing() {
        let (dir, mut d) = store("refused");
        let path = dir.join("devices.json");
        let serial = "00005e005301";
        d.admit(serial, None, Some("E8450"), "OpenWrt 25.12.0", offline)
            .unwrap();
        d.adopt(serial).unwrap();
        let credential = d.issue(serial).unwrap().unwrap();
        d.delivered(serial, false).unwrap();
        let record = d.all()[serial].clone();
        // Any write would put the file back.
        fs::remove_file(&path).unwrap();
        for presented in [None, Some("forged")] {
            let a = d
                .admit(serial, presented, Some("IMPOSTOR"), "IMPOSTOR", offline)
                .unwrap();
            assert_eq!(a, Admission::Refused);
        }
        assert_eq!(d.all()[serial], record);
        assert!(
            !path.exists(),
            "devices.json written for a refused connection"
        );

        // Admitted: what it reports now is kept, and written.
        let a = d.admit(
            serial,
            Some(&credential),
            Some("E8450"),
            "OpenWrt 25.12.1",
            offline,
        );
        assert_eq!(a.unwrap(), Admission::Admitted);
        let again = Devices::load(&path).unwrap();
        assert_eq!(
            again.all()[serial].firmware.as_deref(),
            Some("OpenWrt 25.12.1")
        );
        // Pending: in memory only.
        fs::remove_file(&path).unwrap();
        d.admit("00005e005302", None, Some("E8450"), "OpenWrt", offline)
            .unwrap();
        d.admit("00005e005302", None, Some("E8450"), "OpenWrt 2", offline)
            .unwrap();
        assert_eq!(
            d.all()["00005e005302"].firmware.as_deref(),
            Some("OpenWrt 2")
        );
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_connected_devices_can_be_adopted() {
        let (dir, mut d) = store("unknown");
        assert!(d.adopt("00005e005303").is_err());
        assert!(!d.forget("00005e005303").unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pending_devices_stay_in_memory_and_the_oldest_go_first() {
        let (dir, mut d) = store("pending");
        let path = dir.join("devices.json");
        let serial = |i: usize| format!("00005e{i:06x}");
        let pending = |d: &Devices| {
            d.all()
                .values()
                .filter(|r| r.standing == Standing::Pending)
                .count()
        };
        // Anything that connects is pending, however many: nothing is written for them, and
        // past MAX_PENDING the oldest are dropped.
        for i in 0..MAX_PENDING + 3 {
            let a = d.admit(&serial(i), None, None, "OpenWrt", offline).unwrap();
            assert_eq!(a, Admission::Pending);
        }
        assert!(!path.exists(), "pending devices written to flash");
        assert_eq!(pending(&d), MAX_PENDING);
        for i in 0..3 {
            assert!(
                !d.all().contains_key(&serial(i)),
                "{i} is one of the oldest"
            );
        }
        // The dropped ones are handed over once, for their connections to be closed.
        assert_eq!(d.take_dropped(), [serial(0), serial(1), serial(2)]);
        assert!(d.take_dropped().is_empty());
        // One that connects again keeps its place in the queue.
        d.admit(&serial(3), None, Some("E8450"), "OpenWrt", offline)
            .unwrap();
        assert!(d.take_dropped().is_empty());
        d.admit(&serial(MAX_PENDING + 3), None, None, "OpenWrt", offline)
            .unwrap();
        assert!(!d.all().contains_key(&serial(3)));
        assert_eq!(d.take_dropped(), [serial(3)]);

        // Adopted, a device is written, and no longer counts among the pending.
        d.adopt(&serial(10)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(&serial(10)) && !text.contains(&serial(11)),
            "{text}"
        );
        for i in 100..100 + MAX_PENDING {
            d.admit(&serial(i), None, None, "OpenWrt", offline).unwrap();
        }
        assert_eq!(pending(&d), MAX_PENDING);
        assert!(d.all().contains_key(&serial(10)));
        let dropped = d.take_dropped();
        assert_eq!(dropped.len(), MAX_PENDING - 1);
        assert!(!dropped.contains(&serial(10)));
        // Forgetting a pending device writes nothing either.
        fs::remove_file(&path).unwrap();
        assert!(d.forget(&serial(100)).unwrap());
        assert!(!path.exists());

        // A restart forgets the pending devices; they're back when they next connect.
        d.adopt(&serial(101)).unwrap();
        let again = Devices::load(&path).unwrap();
        let kept: Vec<&String> = again.all().keys().collect();
        assert_eq!(kept, [&serial(10), &serial(101)]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pending_devices_that_are_not_connected_make_room_first() {
        let (dir, mut d) = store("room");
        let serial = |i: usize| format!("00005e{i:06x}");
        // One device stays connected while 100 others connect and hang up: it's kept, and the
        // oldest of the others go.
        let there = serial(0);
        let connected = |s: &str| s == there;
        d.admit(&there, None, None, "OpenWrt", connected).unwrap();
        for i in 1..=100 {
            d.admit(&serial(i), None, None, "OpenWrt", connected)
                .unwrap();
        }
        assert!(d.all().contains_key(&there));
        let dropped: Vec<String> = (1..=100 - (MAX_PENDING - 1)).map(serial).collect();
        assert_eq!(d.take_dropped(), dropped);
        // All of them connected: the oldest goes, connected or not (and is disconnected).
        d.admit(&serial(101), None, None, "OpenWrt", |_| true)
            .unwrap();
        assert_eq!(d.take_dropped(), [there]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn what_a_device_reports_is_cut_to_size() {
        let (dir, mut d) = store("sizes");
        let serial = "00005e005301";
        let long = "x".repeat(10_000);
        // Two bytes a character, one byte off: cut at a character boundary.
        let firmware = format!("v{}", "é".repeat(1000));
        d.admit(serial, None, Some(&long), &firmware, offline)
            .unwrap();
        let r = &d.all()[serial];
        assert_eq!(r.model.as_deref(), Some(&long[..MAX_TEXT]));
        let fw = r.firmware.as_deref().unwrap();
        assert_eq!((fw.len(), fw.chars().count()), (MAX_TEXT - 1, MAX_TEXT / 2));
        // A serial longer than that is refused, and nothing is recorded.
        assert!(
            d.admit(&long[..MAX_TEXT + 1], None, None, "OpenWrt", offline)
                .is_err()
        );
        assert!(
            d.admit(&long[..MAX_TEXT], None, None, "OpenWrt", offline)
                .is_ok()
        );
        assert_eq!(d.all().len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    /// An unreadable registry stops the controller, with what to do, and stays as it was.
    #[test]
    fn an_unreadable_registry_is_an_error_and_left_alone() {
        let (dir, _) = store("unreadable");
        let path = dir.join("devices.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "{ broken").unwrap();
        let e = Devices::load(&path).err().expect("an error");
        assert!(
            e.0.contains("devices.json") && e.0.contains("won't start"),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ broken");
        fs::remove_dir_all(dir).unwrap();
    }
}
