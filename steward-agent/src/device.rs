//! What the agent reports about the device, read through ubus and
//! `/etc/board.json`.

use serde_json::{Map, Value, json};
use std::net::Ipv4Addr;
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
        serial: proto::serial(&board_json),
        uuid: 0,
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

/// The port of a controller enabled on this very device (the router that hosts it), read
/// from its UCI config through ubus: `None` when there's none, or it's off.
pub fn local_controller_port() -> Option<u16> {
    if !std::path::Path::new("/usr/sbin/steward-controller").exists() {
        return None;
    }
    let mut ubus = Ubus::connect().ok()?;
    let args = json!({ "config": "steward-controller", "section": "controller" });
    let values = ubus.call("uci", "get", args.as_object()?).ok()?;
    let section = values.get("values")?;
    let on = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "1" | "on" | "yes" | "true"))
    };
    if !on(section.get("enabled")) {
        return None;
    }
    Some(
        section
            .get("device_port")
            .and_then(Value::as_str)
            .and_then(|p| p.parse().ok())
            .unwrap_or(proto::PORT),
    )
}

/// The IPv4 default gateway, from /proc/net/route.
pub fn default_gateway() -> Option<Ipv4Addr> {
    gateway_in(&std::fs::read_to_string("/proc/net/route").ok()?)
}

/// The gateway of the first default route in a /proc/net/route table, whose
/// addresses are hex in the kernel's (little-endian) byte order.
fn gateway_in(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (dest, gw, mask) = (f.get(1)?, f.get(2)?, f.get(7)?);
        if *dest != "00000000" || *mask != "00000000" {
            return None;
        }
        let gw = u32::from_str_radix(gw, 16).ok()?;
        (gw != 0).then(|| Ipv4Addr::from(gw.to_le_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_route_names_the_gateway() {
        // 192.0.2.1 via br-lan.1; a link route and a VLAN route besides.
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            br-lan.1\t000200C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
            br-lan.1\t00000000\t010200C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
            br-lan.10\t006433C6\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(gateway_in(table), Some(Ipv4Addr::new(192, 0, 2, 1)));
    }

    #[test]
    fn no_default_route_no_gateway() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
            br-lan.1\t000200C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\n";
        assert_eq!(gateway_in(table), None);
    }
}
