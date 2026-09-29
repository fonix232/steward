//! What the controller knows, and what can be done with it: the device registry, connected
//! devices, their configurations, and a stream of events. The control socket and the API
//! both go through these operations.

use crate::devices::{Devices, Standing};
use crate::states::States;
use crate::store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use steward_proto::{self as proto, Message, command};
use tokio::sync::{Mutex, broadcast, mpsc};

/// Where a connected device's messages go.
pub enum Outgoing {
    Send(Message),
    Close(&'static str),
}

/// A connected device.
pub struct Device {
    pub addr: SocketAddr,
    /// The configuration it runs (uuid).
    pub uuid: u64,
    /// What it reported on connecting.
    pub capabilities: Value,
    /// When the controller last heard from it.
    pub seen: u64,
    /// Commands to send it.
    pub tx: mpsc::Sender<Outgoing>,
    /// The id of the `steward.adopt` command awaiting its answer.
    pub adopt_id: Option<u64>,
    /// The id of the `configure` command awaiting its answer, and the uuid it carried.
    pub configure: Option<(u64, u64)>,
}

pub struct Registry {
    pub connected: HashMap<String, Device>,
    pub devices: Devices,
    pub states: States,
    pub next_id: u64,
}

pub struct Hub {
    pub registry: Mutex<Registry>,
    pub config_dir: PathBuf,
    events: broadcast::Sender<Value>,
}

/// Why an operation didn't happen.
#[derive(Debug, PartialEq)]
pub enum OpError {
    NotFound(String),
    Conflict(String),
    Failed(String),
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::NotFound(m) | OpError::Conflict(m) | OpError::Failed(m) => f.write_str(m),
        }
    }
}

fn valid_serial(serial: &str) -> Result<(), OpError> {
    if store::valid_serial(serial) {
        Ok(())
    } else {
        Err(OpError::NotFound(format!("no device {serial}")))
    }
}

impl Hub {
    pub fn new(devices: Devices, mut states: States, config_dir: PathBuf) -> Hub {
        // Only devices on record have a state: nothing else is kept, and a stored state
        // left behind would show on a pending device of the same serial.
        for e in states.retain(|serial| devices.all().contains_key(serial)) {
            crate::log!("state: {e}");
        }
        Hub {
            registry: Mutex::new(Registry {
                connected: HashMap::new(),
                devices,
                states,
                next_id: 0,
            }),
            config_dir,
            events: broadcast::channel(64).0,
        }
    }

    /// Tells whoever listens (the API's event stream) what happened.
    pub fn emit(&self, kind: &str, serial: &str, data: Value) {
        let _ = self
            .events
            .send(json!({ "event": kind, "serial": serial, "data": data }));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Every device the controller knows: its record, whether it's connected (and from
    /// where, running what), and its latest state, live or the last one kept. The
    /// credential's hash stays in devices.json: no answer carries it.
    pub async fn devices(&self) -> Value {
        let reg = self.registry.lock().await;
        let mut out = serde_json::Map::new();
        for (serial, r) in reg.devices.all() {
            let mut v = serde_json::to_value(r).unwrap_or(json!({}));
            if let Some(record) = v.as_object_mut() {
                record.remove("credential_sha256");
            }
            let live = reg.connected.get(serial);
            v["connected"] = json!(live.is_some());
            let latest = reg.states.get(serial);
            if let Some(l) = latest {
                v["running"] = json!(l.uuid);
                v["state"] = l.state.clone();
                v["state_received"] = json!(l.received);
            }
            if let Some(d) = live {
                v["running"] = json!(d.uuid);
                v["last_seen"] = json!(d.seen);
                v["address"] = json!(d.addr.ip().to_canonical().to_string());
                if v.get("capabilities").is_none() {
                    v["capabilities"] = d.capabilities.clone();
                }
            }
            out.insert(serial.clone(), v);
        }
        Value::Object(out)
    }

    /// Every client of the adopted devices, from the connected ones' latest states; every
    /// adopted device's own MACs are left out (`clients::merge`).
    pub async fn clients(&self) -> Value {
        let reg = self.registry.lock().await;
        let null = Value::Null;
        let reports: Vec<crate::clients::Report> = reg
            .devices
            .all()
            .iter()
            .filter(|(_, r)| r.standing == Standing::Adopted)
            .map(|(serial, r)| {
                let latest = reg.states.get(serial);
                let capabilities = r
                    .capabilities
                    .as_ref()
                    .or(reg.connected.get(serial).map(|d| &d.capabilities))
                    .unwrap_or(&null);
                crate::clients::Report {
                    serial,
                    capabilities,
                    state: latest.map_or(&null, |l| &l.state),
                    received: latest.map_or(0, |l| l.received),
                    connected: reg.connected.contains_key(serial),
                }
            })
            .collect();
        Value::Array(crate::clients::merge(&reports))
    }

    /// The controller is stopping: the connected devices were last seen now, and every
    /// changed state is written.
    pub async fn stop(&self) {
        let mut reg = self.registry.lock().await;
        let connected: Vec<String> = reg.connected.keys().cloned().collect();
        let serials: Vec<&str> = connected.iter().map(String::as_str).collect();
        if let Err(e) = reg.devices.seen(&serials) {
            crate::log!("{e}");
        }
        drop(reg);
        self.flush_states().await;
    }

    /// Writes every state that changed since it was last written.
    pub async fn flush_states(&self) {
        let (written, failed) = self.registry.lock().await.states.flush_all();
        for e in failed {
            crate::log!("state: {e}");
        }
        if written > 0 {
            crate::log!("wrote {written} device state(s)");
        }
    }

    pub async fn adopt(&self, serial: &str) -> Result<String, OpError> {
        let mut reg = self.registry.lock().await;
        match reg.devices.get_standing(serial) {
            None => {
                return Err(OpError::NotFound(format!(
                    "no device {serial} has connected"
                )));
            }
            Some(Standing::Adopted) => {
                return Err(OpError::Conflict(format!("{serial} is already adopted")));
            }
            _ => {}
        }
        reg.devices
            .adopt(serial)
            .map_err(|e| OpError::Failed(e.to_string()))?;
        if !reg.connected.contains_key(serial) {
            return Ok(format!("{serial} will be adopted when it next connects"));
        }
        match reg.devices.issue(serial) {
            Ok(Some(credential)) => {
                drop(reg);
                self.send_credential(serial, credential).await;
                Ok(format!("adopting {serial}: its credential is on the way"))
            }
            Ok(None) => Err(OpError::Conflict(format!("{serial} can't be adopted now"))),
            Err(e) => Err(OpError::Failed(e.to_string())),
        }
    }

    pub async fn forget(&self, serial: &str) -> Result<String, OpError> {
        let mut reg = self.registry.lock().await;
        match reg.devices.forget(serial) {
            Ok(true) => {
                if let Some(d) = reg.connected.get(serial) {
                    let _ =
                        d.tx.try_send(Outgoing::Close("forgotten by the controller"));
                }
                // Its stored state and configuration go with it.
                let mut failed: Vec<String> = reg.states.forget(serial).err().into_iter().collect();
                if store::valid_serial(serial) {
                    failed.extend(store::remove(&self.config_path(serial)).err());
                }
                drop(reg);
                self.emit("forgotten", serial, Value::Null);
                for e in &failed {
                    crate::log!("{serial}: forgetting: {e}");
                }
                Ok(format!("forgot {serial}; its credential no longer works"))
            }
            Ok(false) => Err(OpError::NotFound(format!("no device {serial}"))),
            Err(e) => Err(OpError::Failed(e.to_string())),
        }
    }

    fn config_path(&self, serial: &str) -> PathBuf {
        self.config_dir.join(format!("{serial}.json"))
    }

    /// The device's stored configuration.
    pub fn config(&self, serial: &str) -> Result<Option<Value>, OpError> {
        valid_serial(serial)?;
        match std::fs::read_to_string(self.config_path(serial)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| OpError::Failed(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(OpError::Failed(e.to_string())),
        }
    }

    /// Stores a configuration for a device the controller knows, under a new uuid (the time,
    /// in seconds, and never the same as the one before), and sends it when the device is
    /// adopted and connected.
    pub async fn set_config(&self, serial: &str, mut config: Value) -> Result<u64, OpError> {
        valid_serial(serial)?;
        if !config.is_object() {
            return Err(OpError::Conflict("a configuration is a JSON object".into()));
        }
        if self
            .registry
            .lock()
            .await
            .devices
            .get_standing(serial)
            .is_none()
        {
            return Err(OpError::NotFound(format!(
                "no device {serial} has connected"
            )));
        }
        let previous = self
            .config(serial)?
            .and_then(|c| c.get("uuid").and_then(Value::as_u64))
            .unwrap_or(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let uuid = now.max(previous + 1);
        config["uuid"] = json!(uuid);
        store::write_json(&self.config_path(serial), &config).map_err(OpError::Failed)?;
        self.emit("configuration", serial, json!({ "uuid": uuid }));
        self.provision(serial).await;
        Ok(uuid)
    }

    /// Sends an adopted, connected device its stored configuration, if it runs another.
    pub async fn provision(&self, serial: &str) {
        let config = match self.config(serial) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                crate::log!("{serial}: stored configuration: {e}");
                return;
            }
        };
        let Some(uuid) = config.get("uuid").and_then(Value::as_u64) else {
            crate::log!("{serial}: stored configuration has no uuid");
            return;
        };
        let mut reg = self.registry.lock().await;
        if !reg.devices.is_adopted(serial) {
            return;
        }
        reg.next_id += 1;
        let id = reg.next_id;
        let Some(d) = reg.connected.get_mut(serial) else {
            return;
        };
        if d.uuid == uuid {
            return;
        }
        let params = proto::Configure {
            serial: serial.into(),
            uuid,
            when: 0,
            config,
        };
        match Message::request(id, command::CONFIGURE, &params) {
            Ok(m) => {
                crate::log!("{serial}: sending configuration {uuid} (command {id})");
                d.configure = Some((id, uuid));
                let _ = d.tx.try_send(Outgoing::Send(m));
            }
            Err(e) => crate::log!("{serial}: {e}"),
        }
    }

    /// Sends a connected device its credential (`steward.adopt`); its answer completes the adoption.
    pub async fn send_credential(&self, serial: &str, credential: String) {
        let mut reg = self.registry.lock().await;
        reg.next_id += 1;
        let id = reg.next_id;
        let Some(d) = reg.connected.get_mut(serial) else {
            return;
        };
        match Message::request(
            id,
            command::ADOPT,
            &proto::Adopt {
                serial: serial.into(),
                credential,
            },
        ) {
            Ok(m) => {
                d.adopt_id = Some(id);
                let _ = d.tx.try_send(Outgoing::Send(m));
            }
            Err(e) => crate::log!("{serial}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use steward_proto::CommandStatus;

    const SERIAL: &str = "00005e005301";

    fn open(dir: &std::path::Path) -> Hub {
        let (states, skipped) = States::load(&dir.join("state"));
        assert!(skipped.is_empty(), "{skipped:?}");
        Hub::new(
            Devices::load(&dir.join("devices.json")).unwrap(),
            states,
            dir.join("configs"),
        )
    }

    #[tokio::test]
    async fn what_the_controller_knows_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("steward-hub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = open(&dir);
        {
            let mut reg = hub.registry.lock().await;
            let d = &mut reg.devices;
            d.admit(SERIAL, None, Some("E8450"), "OpenWrt", |_| false)
                .unwrap();
            d.adopt(SERIAL).unwrap();
            d.issue(SERIAL).unwrap();
            d.delivered(SERIAL, false).unwrap();
            d.connected(SERIAL, "192.0.2.4", &json!({ "model": "E8450" }))
                .unwrap();
            let status = CommandStatus {
                error: 1,
                text: "applied with substitutions".into(),
                when: None,
                rejected: vec![],
            };
            d.answered(SERIAL, 7, status).unwrap();
            reg.states
                .record(SERIAL, 7, json!({ "unit": { "load": [0.5] } }), true);
        }
        hub.set_config(SERIAL, json!({ "radios": [] }))
            .await
            .unwrap();
        // Connected when the controller stops.
        let (tx, _rx) = mpsc::channel(1);
        hub.registry.lock().await.connected.insert(
            SERIAL.into(),
            Device {
                addr: "192.0.2.4:40000".parse().unwrap(),
                uuid: 7,
                capabilities: json!({}),
                seen: 0,
                tx,
                adopt_id: None,
                configure: None,
            },
        );
        // A state message wrote nothing; the controller stopping writes it, and when the
        // device was last seen.
        assert!(!dir.join("state").exists());
        hub.stop().await;
        drop(hub);
        // A stored state whose device isn't on record (a forget whose removal failed) goes.
        let orphan = dir.join("state").join("00005e0053ff.json");
        std::fs::copy(dir.join("state").join(format!("{SERIAL}.json")), &orphan).unwrap();

        let hub = open(&dir);
        assert!(!orphan.exists());
        assert!(
            hub.registry
                .lock()
                .await
                .states
                .get("00005e0053ff")
                .is_none()
        );
        let all = hub.devices().await;
        let d = &all[SERIAL];
        assert_eq!(d["standing"], "adopted");
        assert_eq!(d["connected"], false);
        assert_eq!(d["address"], "192.0.2.4");
        assert_eq!(d["capabilities"]["model"], "E8450");
        assert!(d["last_seen"].as_u64() >= d["last_connected"].as_u64());
        assert_eq!(d["running"], 7);
        assert_eq!(d["state"]["unit"]["load"][0], 0.5);
        assert_eq!(d["last_answer"]["uuid"], 7);
        assert_eq!(d["last_answer"]["status"]["error"], 1);
        assert!(hub.config(SERIAL).unwrap().is_some());

        // Forgotten: its record, stored state and configuration go.
        hub.forget(SERIAL).await.unwrap();
        assert!(!dir.join("state").join(format!("{SERIAL}.json")).exists());
        assert!(!dir.join("configs").join(format!("{SERIAL}.json")).exists());
        assert!(open(&dir).devices().await.get(SERIAL).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An adopted device that isn't connected has a stored state, but its clients aren't
    /// known any more.
    #[tokio::test]
    async fn an_offline_devices_clients_arent_listed() {
        let dir = std::env::temp_dir().join(format!("steward-hub-clients-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = open(&dir);
        {
            let mut reg = hub.registry.lock().await;
            let d = &mut reg.devices;
            d.admit(SERIAL, None, Some("E8450"), "OpenWrt", |_| false)
                .unwrap();
            d.adopt(SERIAL).unwrap();
            d.issue(SERIAL).unwrap();
            d.delivered(SERIAL, false).unwrap();
            let state = json!({ "interfaces": [{ "name": "lan",
                "ssids": [{ "ssid": "Home", "iface": "wl0-ap0", "associations": [
                    { "station": "00:00:5e:00:53:21", "inactive": 0 }] }],
                "clients": [{ "mac": "00:00:5e:00:53:31", "ports": ["lan1"] }] }] });
            reg.states.record(SERIAL, 0, state, true);
        }
        assert_eq!(hub.clients().await, json!([]));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
