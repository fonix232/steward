//! What the agent reports about the device, read through ubus and
//! `/etc/board.json`.

use serde_json::{Map, Value, json};
use steward_proto as proto;
use steward_ubus::Ubus;

/// The `connect` event's contents: who the device is and what it has.
pub fn identity() -> Result<proto::Connect, steward_ubus::Error> {
    let mut ubus = Ubus::connect()?;
    let board = ubus.call("system", "board", &Map::new())?;
    let board_json: Value = std::fs::read_to_string("/etc/board.json")
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    Ok(proto::Connect {
        serial: serial(&board_json),
        uuid: running_uuid(),
        firmware: board
            .get("release")
            .and_then(|r| r.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("OpenWrt")
            .into(),
        wanip: vec![],
        capabilities: capabilities(&board, &board_json),
    })
}

/// uCentral's serial: the device's label MAC address, lower case, without
/// separators. The label MAC comes from board.json, else the first
/// Ethernet interface's.
fn serial(board_json: &Value) -> String {
    let mac = board_json
        .pointer("/system/label_macaddr")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            ["eth0", "lan1", "lan", "wan"]
                .iter()
                .find_map(|i| std::fs::read_to_string(format!("/sys/class/net/{i}/address")).ok())
        })
        .unwrap_or_default();
    mac.trim().to_lowercase().replace(':', "")
}

/// The configuration the device runs: 0 until one from the controller is
/// applied (applying is not implemented yet).
pub fn running_uuid() -> u64 {
    0
}

/// A subset of uCentral's capabilities document: what the device is and
/// which ports it has.
fn capabilities(board: &Map<String, Value>, board_json: &Value) -> Value {
    let mut network = Map::new();
    if let Some(nets) = board_json.get("network").and_then(Value::as_object) {
        for (role, net) in nets {
            let mut ports: Vec<Value> = net
                .get("ports")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(dev) = net.get("device") {
                ports.push(dev.clone());
            }
            network.insert(role.clone(), Value::Array(ports));
        }
    }
    json!({
        "compatible": board.get("board_name").and_then(Value::as_str).unwrap_or_default().replace(',', "_"),
        "model": board.get("model"),
        "platform": if board_json.get("wlan").is_some_and(|w| w.as_object().is_some_and(|w| !w.is_empty())) { "ap" } else { "unknown" },
        "network": network,
    })
}

/// The `state` event's document: uCentral's `unit` section.
pub fn state() -> Result<Value, steward_ubus::Error> {
    let mut ubus = Ubus::connect()?;
    let info = ubus.call("system", "info", &Map::new())?;
    let load: Vec<Value> = info
        .get("load")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .map(|v| json!(v.as_f64().unwrap_or(0.0) / 65536.0))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "unit": {
            "load": load,
            "localtime": info.get("localtime"),
            "uptime": info.get("uptime"),
            "memory": info.get("memory"),
        }
    }))
}
