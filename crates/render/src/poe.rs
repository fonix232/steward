//! uCentral's `ethernet` → PoE on the device's ports, through realtek-poe.
//!
//! realtek-poe keeps one `port` section per powered port (`id`, `name`, `enable`, `priority`,
//! `poe_plus`), made with the device's image: they're the device's own, like its radios. The
//! renderer sets `enable` on them (`ethernet[].poe.admin-mode`) and lists each in
//! [`Plan::device_options`]. realtek-poe reloads its config when it changes (a procd config
//! trigger), and the rollback covers it with the rest.
//!
//! `ethernet`'s link settings (`speed`, `duplex`, `enabled`, `services`) and any other key aren't
//! supported yet.

use crate::{Op, Plan, Ports, reject, unsupported};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// A port realtek-poe powers.
#[derive(Debug, Clone, PartialEq)]
pub struct PoePort {
    pub section: String,
    /// The port it powers (`lan3`).
    pub name: String,
    /// Whether its power is on.
    pub enable: bool,
}

/// realtek-poe's config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Poe {
    pub ports: Vec<PoePort>,
}

impl Poe {
    /// From `uci get {"config": "poe"}` over ubus.
    pub fn from_uci(answer: &Map<String, Value>) -> Poe {
        let mut ports: Vec<(u64, PoePort)> = answer
            .get("values")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, s)| s.get(".type").and_then(Value::as_str) == Some("port"))
            .filter_map(|(section, s)| {
                let id = port_id(s.get("id")?.as_str()?)?;
                Some((
                    id,
                    PoePort {
                        section: section.clone(),
                        name: s.get("name")?.as_str()?.to_owned(),
                        enable: s.get("enable").and_then(Value::as_str) == Some("1"),
                    },
                ))
            })
            .collect();
        ports.sort_by_key(|(id, _)| *id);
        Poe {
            ports: ports.into_iter().map(|(_, p)| p).collect(),
        }
    }

    pub fn port(&self, name: &str) -> Option<&PoePort> {
        self.ports.iter().find(|p| p.name == name)
    }
}

/// realtek-poe's highest port id (`MAX_PORT`).
const MAX_PORT: u64 = 48;

/// A port's `id` as realtek-poe reads it (`load_port_config`): C's `strtoul(id, NULL, 0)`,
/// then 1 to `MAX_PORT`, or the port is dropped. `strtoul` skips leading white space and a
/// sign, reads `0x` as hex and a leading 0 as octal, and stops at the first character that
/// isn't a digit. A `-` wraps a non-zero id past `MAX_PORT`, and so does an overflow.
fn port_id(s: &str) -> Option<u64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_hexdigit()));
    let (radix, digits) = match hex {
        Some(rest) => (16, rest),
        None if s.starts_with('0') => (8, s),
        None => (10, s),
    };
    let mut id: u64 = 0;
    for d in digits.chars().map_while(|c| c.to_digit(radix)) {
        id = id.checked_mul(radix.into())?.checked_add(d.into())?;
    }
    if negative && id != 0 {
        return None;
    }
    (1..=MAX_PORT).contains(&id).then_some(id)
}

/// The `ethernet[]` keys handled: the rest (`speed`, `duplex`, `enabled`, `services`, ...) are
/// rejected, so an ignored one never gets answer 0.
const ETHERNET_KEYS: [&str; 2] = ["select-ports", "poe"];

/// `ethernet`: each entry's PoE mode for the ports it selects, and refusals for the rest.
pub(crate) fn ethernet(config: &Value, poe: Option<&Poe>, ports: &Ports, plan: &mut Plan) {
    let list = match config.get("ethernet") {
        None => return,
        Some(Value::Array(a)) => a,
        Some(other) => return reject(plan, "/ethernet", other, "ethernet is a list"),
    };
    // Port → (power on, the entry that asked).
    let mut wanted: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for (e, entry) in list.iter().enumerate() {
        let at = format!("/ethernet/{e}");
        if !entry.is_object() {
            reject(plan, &at, entry, "an ethernet entry is an object");
            continue;
        }
        unsupported(plan, &at, entry, &ETHERNET_KEYS);
        let Some(p) = entry.get("poe") else {
            continue;
        };
        let Some(fields) = p.as_object() else {
            reject(plan, &format!("{at}/poe"), p, "poe is an object");
            continue;
        };
        for (key, value) in fields {
            if key != "admin-mode" {
                reject(
                    plan,
                    &format!("{at}/poe/{key}"),
                    value,
                    format!("{key} isn't supported yet"),
                );
            }
        }
        // Only a missing admin-mode takes the schema's default: "false" isn't false.
        let on = match fields.get("admin-mode") {
            None => true,
            Some(Value::Bool(on)) => *on,
            Some(v) => {
                reject(
                    plan,
                    &format!("{at}/poe/admin-mode"),
                    v,
                    "admin-mode is true or false",
                );
                continue;
            }
        };
        let Some(poe) = poe else {
            reject(
                plan,
                &format!("{at}/poe"),
                p,
                "this device has no PoE controller (realtek-poe)",
            );
            continue;
        };
        // A pattern that isn't a string is refused, not skipped: the rest would be answered 0.
        let list = entry.get("select-ports").and_then(Value::as_array);
        let mut patterns = vec![];
        for (n, v) in list.into_iter().flatten().enumerate() {
            match v.as_str() {
                Some(p) => patterns.push(p),
                None => reject(
                    plan,
                    &format!("{at}/select-ports/{n}"),
                    v,
                    "a port pattern is a string (LAN2, LAN*)",
                ),
            }
        }
        if list.is_none_or(|l| l.is_empty()) {
            reject(
                plan,
                &format!("{at}/select-ports"),
                entry.get("select-ports").unwrap_or(&Value::Null),
                "select-ports names the ports",
            );
        }
        for pattern in patterns {
            let Some(found) = ports.select(pattern) else {
                reject(
                    plan,
                    &format!("{at}/select-ports"),
                    &json!(pattern),
                    "no such ports on this device",
                );
                continue;
            };
            for port in found {
                if poe.port(&port).is_none() {
                    // A wildcard also selects the ports without power: only named ones are refused.
                    if !pattern.ends_with('*') {
                        reject(
                            plan,
                            &format!("{at}/select-ports"),
                            &json!(pattern),
                            format!("{port} has no PoE"),
                        );
                    }
                    continue;
                }
                match wanted.get(&port) {
                    Some((was, first)) if *was != on => reject(
                        plan,
                        &format!("{at}/poe/admin-mode"),
                        &json!(on),
                        format!("{first} already turns {port}'s power {}", onoff(*was)),
                    ),
                    Some(_) => {}
                    None => {
                        wanted.insert(port, (on, at.clone()));
                    }
                }
            }
        }
    }
    let Some(poe) = poe else {
        return;
    };
    for (port, (on, _)) in wanted {
        let section = &poe.port(&port).expect("selected above").section;
        plan.device_options
            .push(("poe".into(), section.clone(), "enable".into()));
        plan.ops.push(Op::Set {
            config: "poe".into(),
            section: section.clone(),
            values: [("enable".to_string(), json!(if on { "1" } else { "0" }))]
                .into_iter()
                .collect(),
        });
    }
}

fn onoff(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}
