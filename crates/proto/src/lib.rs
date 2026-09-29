//! The messages a Steward controller and its agents exchange.
//!
//! The wire protocol is uCentral's (Telecom Infra Project, OpenLAN): JSON-RPC
//! 2.0 over a WebSocket the device opens to the controller on port 15002.
//! The device sends events as notifications (no `id`), and the controller
//! sends commands as requests, which the device answers with a result that
//! carries a [`CommandStatus`]. The reference is TIP's `PROTOCOL.md` in
//! `wlan-cloud-ucentralgw`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The port devices connect to.
pub const PORT: u16 = 15002;

/// One JSON-RPC 2.0 message, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// A command: the receiver must answer it with a [`Message::Response`]
    /// carrying the same `id`.
    Request {
        jsonrpc: Version,
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// An event: nothing answers it.
    Notification {
        jsonrpc: Version,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// The answer to a request.
    Response {
        jsonrpc: Version,
        id: u64,
        #[serde(flatten)]
        outcome: Outcome,
    },
}

impl Message {
    pub fn request(id: u64, method: &str, params: impl Serialize) -> serde_json::Result<Self> {
        Ok(Message::Request {
            jsonrpc: Version,
            id,
            method: method.into(),
            params: serde_json::to_value(params)?,
        })
    }

    pub fn notification(method: &str, params: impl Serialize) -> serde_json::Result<Self> {
        Ok(Message::Notification {
            jsonrpc: Version,
            method: method.into(),
            params: serde_json::to_value(params)?,
        })
    }

    pub fn result(id: u64, result: impl Serialize) -> serde_json::Result<Self> {
        Ok(Message::Response {
            jsonrpc: Version,
            id,
            outcome: Outcome::Result(serde_json::to_value(result)?),
        })
    }
}

/// A response's payload: uCentral devices answer commands with a `result`
/// whose [`CommandStatus`] carries the outcome, and use `error` only for a
/// request they could not read at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Result(Value),
    Error(RpcError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The `"jsonrpc": "2.0"` member, which must be exactly that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "2.0" {
            Ok(Version)
        } else {
            Err(serde::de::Error::custom(format!(
                "jsonrpc version {v:?}, not \"2.0\""
            )))
        }
    }
}

/// Device events: the methods of the notifications a device sends.
pub mod event {
    pub const CONNECT: &str = "connect";
    pub const STATE: &str = "state";
    pub const HEALTHCHECK: &str = "healthcheck";
    pub const LOG: &str = "log";
    pub const CRASHLOG: &str = "crashlog";
    pub const EVENT: &str = "event";
    pub const CFG_PENDING: &str = "cfgpending";
    pub const PING: &str = "ping";
    pub const RECOVERY: &str = "recovery";
}

/// Controller commands: the methods of the requests a controller sends.
pub mod command {
    pub const CONFIGURE: &str = "configure";
    pub const REBOOT: &str = "reboot";
    pub const UPGRADE: &str = "upgrade";
    pub const FACTORY: &str = "factory";
    pub const LEDS: &str = "leds";
    pub const TRACE: &str = "trace";
    pub const WIFISCAN: &str = "wifiscan";
    pub const REQUEST: &str = "request";
    pub const PING: &str = "ping";
    pub const POWERCYCLE: &str = "powercycle";
}

/// `connect`: the first event on every connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connect {
    pub serial: String,
    /// The configuration the device runs (0: none from a controller yet).
    pub uuid: u64,
    pub firmware: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wanip: Vec<String>,
    /// The device's capabilities document (ports, radios, platform).
    pub capabilities: Value,
}

/// `state`: the device's periodic state report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub serial: String,
    pub uuid: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_uuid: Option<String>,
    pub state: Value,
}

/// `healthcheck`: how well the device's subsystems work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Healthcheck {
    pub serial: String,
    pub uuid: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_uuid: Option<String>,
    /// 0 (dead) to 100 (all well).
    pub sanity: u8,
    #[serde(default)]
    pub data: Value,
}

/// `ping` (event): a keepalive naming the configuration the device runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    pub serial: String,
    pub uuid: u64,
}

/// `configure`: apply this configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Configure {
    pub serial: String,
    pub uuid: u64,
    /// When to apply it: 0 now, otherwise a UTC time in seconds (a hint).
    #[serde(default)]
    pub when: u64,
    pub config: Value,
}

/// The answer to a command: `{"serial", "uuid", "status"}` as the result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub serial: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<u64>,
    pub status: CommandStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandStatus {
    /// For `configure`: 0 applied as sent, 1 applied with the substitutions
    /// in `rejected`, 2 not applied. Other commands: 0 success.
    pub error: u32,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<Rejection>,
}

/// A part of a configuration the device would not apply as sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    pub parameter: Value,
    pub reason: String,
    /// What the device applied instead; absent when it left the part out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substitution: Option<Value>,
}

pub mod configure_error {
    pub const APPLIED: u32 = 0;
    pub const APPLIED_WITH_SUBSTITUTIONS: u32 = 1;
    pub const REJECTED: u32 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Message {
        serde_json::from_value(v).expect("parses")
    }

    #[test]
    fn a_connect_event_is_a_notification() {
        let m = parse(json!({
            "jsonrpc": "2.0", "method": "connect",
            "params": { "serial": "00005e005301", "uuid": 0, "firmware": "OpenWrt 25.12.0",
                        "capabilities": { "platform": "ap" } }
        }));
        let Message::Notification { method, params, .. } = m else {
            panic!("{m:?}")
        };
        assert_eq!(method, event::CONNECT);
        let c: Connect = serde_json::from_value(params).unwrap();
        assert_eq!(c.serial, "00005e005301");
        assert!(c.wanip.is_empty());
    }

    #[test]
    fn a_configure_command_is_a_request() {
        let m = parse(json!({
            "jsonrpc": "2.0", "id": 7, "method": "configure",
            "params": { "serial": "00005e005301", "uuid": 1700000000, "config": { "uuid": 1700000000 } }
        }));
        let Message::Request {
            id, method, params, ..
        } = m
        else {
            panic!("{m:?}")
        };
        assert_eq!((id, method.as_str()), (7, command::CONFIGURE));
        let c: Configure = serde_json::from_value(params).unwrap();
        assert_eq!(c.when, 0);
    }

    #[test]
    fn a_command_result_round_trips() {
        let r = CommandResult {
            serial: "00005e005301".into(),
            uuid: Some(2),
            status: CommandStatus {
                error: configure_error::APPLIED_WITH_SUBSTITUTIONS,
                text: "channel 165 is not allowed here".into(),
                when: None,
                rejected: vec![Rejection {
                    parameter: json!({ "channel": 165 }),
                    reason: "regulatory".into(),
                    substitution: Some(json!({ "channel": 149 })),
                }],
            },
        };
        let wire = serde_json::to_value(Message::result(7, &r).unwrap()).unwrap();
        assert_eq!(wire["id"], 7);
        assert_eq!(
            wire["result"]["status"]["rejected"][0]["substitution"]["channel"],
            149
        );
        let Message::Response {
            outcome: Outcome::Result(v),
            ..
        } = parse(wire)
        else {
            panic!()
        };
        assert_eq!(serde_json::from_value::<CommandResult>(v).unwrap(), r);
    }

    #[test]
    fn an_error_response_parses() {
        let m = parse(
            json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32601, "message": "no such method" } }),
        );
        assert!(matches!(
            m,
            Message::Response {
                id: 3,
                outcome: Outcome::Error(RpcError { code: -32601, .. }),
                ..
            }
        ));
    }

    #[test]
    fn another_jsonrpc_version_is_refused() {
        assert!(
            serde_json::from_value::<Message>(
                json!({ "jsonrpc": "1.0", "method": "ping", "params": {} })
            )
            .is_err()
        );
    }
}
