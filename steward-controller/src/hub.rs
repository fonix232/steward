//! What the controller knows, and what can be done with it: the device registry, connected
//! devices, their configurations, and a stream of events. The control socket and the API
//! both go through these operations.

use crate::devices::{Devices, Standing};
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
    pub state: Option<Value>,
    /// Commands to send it.
    pub tx: mpsc::Sender<Outgoing>,
    /// The id of the `steward.adopt` command awaiting its answer.
    pub adopt_id: Option<u64>,
}

pub struct Registry {
    pub connected: HashMap<String, Device>,
    pub devices: Devices,
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
    // A MAC without separators: never a path.
    if serial.len() == 12 && serial.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(OpError::NotFound(format!("no device {serial}")))
    }
}

impl Hub {
    pub fn new(devices: Devices, config_dir: PathBuf) -> Hub {
        Hub {
            registry: Mutex::new(Registry {
                connected: HashMap::new(),
                devices,
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

    /// Every device the controller knows: its record, and whether it's connected, with what.
    /// The credential's hash stays in devices.json: no answer carries it.
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
            if let Some(d) = live {
                v["running"] = json!(d.uuid);
                v["state"] = d.state.clone().unwrap_or(Value::Null);
            }
            out.insert(serial.clone(), v);
        }
        Value::Object(out)
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
                drop(reg);
                self.emit("forgotten", serial, Value::Null);
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
        let io = |e: std::io::Error| OpError::Failed(e.to_string());
        std::fs::create_dir_all(&self.config_dir).map_err(io)?;
        let path = self.config_path(serial);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&config).unwrap_or_default()).map_err(io)?;
        std::fs::rename(&tmp, &path).map_err(io)?;
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
        let Some(d) = reg.connected.get(serial) else {
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
