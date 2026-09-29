//! uCentral interfaces → the device's networks.
//!
//! On an access point or switch an interface is a layer-2 network. The untagged one is the
//! device's own `lan`, left as it is (its addressing included). One with a VLAN id lives on
//! the device's VLAN-filtering bridge as `<bridge>.<vid>`:
//! - a VLAN already on the bridge (a `bridge-vlan` for that id, whoever made it) is reused
//!   as it is, never edited;
//! - a new one gets an owned `bridge-vlan` (`stw_bv<vid>`), tagged on the ports the interface
//!   selects (all bridge ports by default), and an owned interface `stw_vlan<vid>`
//!   (proto none) unless one already sits on `<bridge>.<vid>`.

use crate::{MARKER, PREFIX, Plan, options_of, reject, unsupported};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The board's ports by role, from board.json's `network` section.
#[derive(Debug, Default, Clone)]
pub struct Ports {
    pub lan: Vec<String>,
    pub wan: Vec<String>,
}

impl Ports {
    pub fn from_board(board: &Value) -> Ports {
        let list = |role: &str| -> Vec<String> {
            let n = &board["network"][role];
            let mut v: Vec<String> = n["ports"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(d) = n["device"].as_str() {
                v.push(d.to_owned());
            }
            v
        };
        Ports {
            lan: list("lan"),
            wan: list("wan"),
        }
    }

    /// uCentral's `select-ports` patterns (`LAN*`, `LAN2`, `WAN*`, `WAN1`) → port names.
    fn select(&self, pattern: &str) -> Option<Vec<String>> {
        let p = pattern.to_ascii_uppercase();
        let (role, rest) = match p.strip_prefix("LAN") {
            Some(r) => (&self.lan, r),
            None => (&self.wan, p.strip_prefix("WAN")?),
        };
        match rest {
            "*" => Some(role.clone()),
            n => n
                .parse::<usize>()
                .ok()
                .filter(|&n| n >= 1)
                .and_then(|n| role.get(n - 1))
                .map(|p| vec![p.clone()]),
        }
    }
}

/// A `bridge-vlan` on the device.
#[derive(Debug, Clone)]
pub struct Vlan {
    pub section: String,
    pub owned: bool,
    /// Its ports as UCI lists them (`lan1:t`, `wan:u*`).
    pub ports: Vec<String>,
}

/// What the device's network config holds now.
#[derive(Debug, Default)]
pub struct Network {
    /// The VLAN-filtering bridge: its device name and ports.
    pub bridge: Option<(String, Vec<String>)>,
    pub vlans: BTreeMap<u16, Vlan>,
    /// interface section → its device
    pub interfaces: BTreeMap<String, String>,
    /// (section, type) the agent owns.
    pub owned: Vec<(String, String)>,
    /// The options each owned section has now.
    pub options: BTreeMap<String, Vec<String>>,
}

fn list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::String(s) => s.split_whitespace().map(str::to_owned).collect(),
        _ => vec![],
    }
}

/// Whether a `bridge-vlan` port entry (`lan1`, `lan1:u*`, `lan1:t`) carries `port` untagged.
fn untagged(entry: &str, port: &str) -> bool {
    membership(entry) == (port, false)
}

impl Network {
    /// From `uci get {"config": "network"}` over ubus.
    pub fn from_uci(answer: &Map<String, Value>) -> Network {
        let mut n = Network::default();
        let Some(Value::Object(sections)) = answer.get("values") else {
            return n;
        };
        let kind = |s: &Value| s.get(".type").and_then(Value::as_str).map(str::to_owned);
        // Both the name and the marker: a user's section that carries the marker isn't the
        // agent's to edit or delete.
        let owned = |name: &str, s: &Value| {
            name.starts_with(PREFIX) && s.get(MARKER).and_then(Value::as_str) == Some("1")
        };
        let vlan_devices: Vec<&str> = sections
            .values()
            .filter(|s| kind(s).as_deref() == Some("bridge-vlan"))
            .filter_map(|s| s["device"].as_str())
            .collect();
        let mut bridges = vec![];
        for (name, s) in sections {
            match kind(s).as_deref() {
                // netifd filters VLANs on a bridge that has bridge-vlan sections, or is told to.
                Some("device") if s["type"] == "bridge" => {
                    if let Some(dev) = s["name"].as_str()
                        && (s["vlan_filtering"] == "1" || vlan_devices.contains(&dev))
                    {
                        bridges.push((dev.to_owned(), list(&s["ports"])));
                    }
                }
                Some("interface") => {
                    if let Some(dev) = s["device"].as_str() {
                        n.interfaces.insert(name.clone(), dev.to_owned());
                    }
                }
                _ => {}
            }
            if owned(name, s)
                && let Some(t) = kind(s)
            {
                n.owned.push((name.clone(), t));
                n.options.insert(name.clone(), options_of(s));
            }
        }
        // The LAN bridge when there are several.
        bridges.sort_by_key(|(dev, _)| dev != "br-lan");
        n.bridge = bridges.into_iter().next();
        if let Some((dev, _)) = &n.bridge {
            for (name, s) in sections {
                if kind(s).as_deref() == Some("bridge-vlan")
                    && s["device"].as_str() == Some(dev)
                    && let Some(vid) = s["vlan"].as_str().and_then(|v| v.parse::<u16>().ok())
                {
                    n.vlans.insert(
                        vid,
                        Vlan {
                            section: name.clone(),
                            owned: owned(name, s),
                            ports: list(&s["ports"]),
                        },
                    );
                }
            }
        }
        n
    }

    fn is_owned(&self, section: &str) -> bool {
        self.owned.iter().any(|(s, _)| s == section)
    }

    /// Adds an owned section, or sets it again (dropping options it no longer has) when the
    /// agent made it before.
    fn put(&self, plan: &mut Plan, kind: &str, section: &str, values: Map<String, Value>) {
        let had = self
            .is_owned(section)
            .then(|| self.options.get(section))
            .flatten();
        crate::put(plan, "network", kind, section, values, had);
    }
}

fn section(values: &[(&str, Value)]) -> Map<String, Value> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .chain([(MARKER.to_string(), json!("1"))])
        .collect()
}

/// A port entry's port and whether it's tagged (`lan1:t` → (lan1, true); `lan1`, `lan1:u*` →
/// untagged).
fn membership(entry: &str) -> (&str, bool) {
    let (name, flags) = entry.split_once(':').unwrap_or((entry, ""));
    (name, flags.contains('t'))
}

/// A port an interface's `ethernet` selects: the entry that selects it, its name, and
/// whether it's tagged.
struct Selected {
    entry: usize,
    port: String,
    tagged: bool,
}

/// The board's ports an interface's `ethernet` entries select. What doesn't parse is
/// rejected, never guessed at: an entry that isn't an object, a `select-ports` that isn't a
/// list of strings, a pattern the board has no ports for, and a `vlan-tag` other than TIP's
/// `tagged`, `un-tagged` or `auto` (`untagged` is taken too). `auto`, TIP's default, is
/// tagged.
fn selection(at: &str, ethernet: &[Value], ports: &Ports, plan: &mut Plan) -> Vec<Selected> {
    let mut selected = vec![];
    for (e, entry) in ethernet.iter().enumerate() {
        let at = format!("{at}/ethernet/{e}");
        if !entry.is_object() {
            reject(plan, &at, entry, "an ethernet entry is an object");
            continue;
        }
        let tagged = match entry.get("vlan-tag") {
            None => true,
            Some(Value::String(t)) if t == "tagged" || t == "auto" => true,
            Some(Value::String(t)) if t == "un-tagged" || t == "untagged" => false,
            Some(other) => {
                reject(
                    plan,
                    &format!("{at}/vlan-tag"),
                    other,
                    "vlan-tag is tagged, un-tagged or auto",
                );
                continue;
            }
        };
        let patterns = match entry.get("select-ports") {
            None => continue,
            Some(Value::Array(a)) => a,
            Some(other) => {
                reject(
                    plan,
                    &format!("{at}/select-ports"),
                    other,
                    "select-ports is a list of port names",
                );
                continue;
            }
        };
        for pattern in patterns {
            let found = pattern.as_str().and_then(|p| ports.select(p));
            let Some(found) = found else {
                let reason = if pattern.is_string() {
                    "no such ports on this device"
                } else {
                    "a port name is a string"
                };
                reject(plan, &format!("{at}/select-ports"), pattern, reason);
                continue;
            };
            selected.extend(found.into_iter().map(|port| Selected {
                entry: e,
                port,
                tagged,
            }));
        }
    }
    selected
}

/// The `bridge-vlan` port entries for a new VLAN `vid` from what its interface selects:
/// every bridge port tagged when it has no `ethernet`. Ports that can't carry it are
/// rejected, and so is a port selected again with the other tag: the first selection stands.
/// `untagged_here` collects the ports this configuration makes untagged.
fn members(
    at: &str,
    selected: Option<&[Selected]>,
    vid: u16,
    bridge_ports: &[String],
    current: &Network,
    untagged_here: &mut Vec<(u16, String)>,
    plan: &mut Plan,
) -> Vec<String> {
    let Some(selected) = selected else {
        let mut all: Vec<String> = bridge_ports.iter().map(|p| format!("{p}:t")).collect();
        all.sort();
        return all;
    };
    let tag = |tagged: bool| if tagged { "tagged" } else { "un-tagged" };
    let mut members = vec![];
    // Each port's first selection: whether it's tagged, and the entry that selects it.
    let mut first: Vec<(&str, bool, String)> = vec![];
    for Selected {
        entry,
        port: p,
        tagged,
    } in selected
    {
        let at = format!("{at}/ethernet/{entry}");
        if !bridge_ports.contains(p) {
            reject(
                plan,
                &format!("{at}/select-ports"),
                &json!(p),
                "the port isn't on the device's bridge",
            );
            continue;
        }
        // Both ways, the bridge-vlan would list the port twice, tagged and untagged.
        match first.iter().find(|(q, ..)| q == p) {
            Some((_, was, by)) if was != tagged => {
                reject(
                    plan,
                    &format!("{at}/vlan-tag"),
                    &json!({ "port": p, "vlan-tag": tag(*tagged) }),
                    format!("{by} already selects {p} {}", tag(*was)),
                );
                continue;
            }
            Some(_) => {}
            None => first.push((p, *tagged, at.clone())),
        }
        if *tagged {
            members.push(format!("{p}:t"));
        } else if current
            .vlans
            .iter()
            .any(|(id, v)| *id != vid && v.ports.iter().any(|s| untagged(s, p)))
            || untagged_here.iter().any(|(id, q)| *id != vid && q == p)
        {
            reject(
                plan,
                &format!("{at}/vlan-tag"),
                &json!({ "port": p, "vlan-tag": "un-tagged" }),
                "the port is already untagged in another VLAN",
            );
        } else {
            untagged_here.push((vid, p.clone()));
            members.push(format!("{p}:u*"));
        }
    }
    members.sort();
    members.dedup();
    members
}

/// The keys of an interface this renderer handles; any other is rejected.
const INTERFACE_KEYS: [&str; 6] = ["name", "role", "vlan", "ethernet", "ssids", "ipv4"];
/// The keys of an interface's `vlan`, and of each of its `ethernet` entries.
const VLAN_KEYS: [&str; 2] = ["id", "proto"];
const ETHERNET_KEYS: [&str; 2] = ["select-ports", "vlan-tag"];

/// The networks: for each uCentral interface, the network its SSIDs join (`None` when it was
/// refused), and the owned network sections the configuration needs.
pub(crate) fn networks(
    config: &Value,
    current: &Network,
    ports: &Ports,
    plan: &mut Plan,
) -> (Vec<Option<String>>, Vec<String>) {
    let mut by_interface = vec![];
    let mut wanted = vec![];
    let mut vids = vec![];
    // Ports this configuration makes untagged, by VLAN.
    let mut untagged_here: Vec<(u16, String)> = vec![];
    let Some(interfaces) = config.get("interfaces").and_then(Value::as_array) else {
        return (by_interface, wanted);
    };
    for (i, iface) in interfaces.iter().enumerate() {
        let at = format!("/interfaces/{i}");
        // Read as one without fields, it would be the device's lan: refused instead.
        if !iface.is_object() {
            reject(plan, &at, iface, "an interface is an object");
            by_interface.push(None);
            continue;
        }
        unsupported(plan, &at, iface, &INTERFACE_KEYS);
        // TIP's roles. Both are rendered alike here, so a wrong one is refused on its own.
        if let Some(role) = iface
            .get("role")
            .filter(|r| r.as_str() != Some("upstream") && r.as_str() != Some("downstream"))
        {
            reject(
                plan,
                &format!("{at}/role"),
                role,
                "a role is upstream or downstream",
            );
        }
        let ethernet = match iface.get("ethernet") {
            None => None,
            Some(Value::Array(entries)) => {
                for (e, entry) in entries.iter().enumerate() {
                    unsupported(plan, &format!("{at}/ethernet/{e}"), entry, &ETHERNET_KEYS);
                }
                Some(Ok(entries))
            }
            Some(other) => {
                reject(plan, &format!("{at}/ethernet"), other, "ethernet is a list");
                Some(Err(()))
            }
        };
        // The device's own addressing stays: routing and DHCP serving are the gateway's tasks.
        // Only `addressing: none` asks nothing, with TIP's default `send-hostname: true`
        // (netifd's own default too); anything else in `ipv4` would be dropped, answered 0.
        let asks_nothing = |(k, v): (&String, &Value)| {
            (k == "addressing" && v == "none") || (k == "send-hostname" && *v == true)
        };
        if let Some(ipv4) = iface.get("ipv4")
            && ipv4.as_object().is_none_or(|o| !o.iter().all(asks_nothing))
        {
            reject(
                plan,
                &format!("{at}/ipv4"),
                ipv4,
                "routed interfaces and DHCP serving aren't supported yet; the device keeps its own addressing",
            );
        }
        let Some(vlan) = iface.get("vlan") else {
            // The device's own lan keeps its ports: ports asked of it select nothing.
            if let Some(Ok(_)) = ethernet {
                reject(
                    plan,
                    &format!("{at}/ethernet"),
                    &iface["ethernet"],
                    "an interface without a VLAN is the device's own lan, whose ports stay as they are",
                );
            }
            by_interface.push(Some("lan".to_string()));
            continue;
        };
        // An ethernet that isn't a list would read as no selection: every port tagged.
        if let Some(Err(())) = ethernet {
            by_interface.push(None);
            continue;
        }
        unsupported(plan, &format!("{at}/vlan"), vlan, &VLAN_KEYS);
        // Anything but 802.1q (the default) would be a different network: refused, SSIDs too.
        if let Some(proto) = vlan.get("proto").filter(|p| *p != "802.1q") {
            let shown = proto.as_str().map_or(proto.to_string(), str::to_owned);
            reject(
                plan,
                &format!("{at}/vlan/proto"),
                proto,
                format!("{shown} VLANs aren't supported yet; only 802.1q"),
            );
            by_interface.push(None);
            continue;
        }
        // Without an id it isn't the untagged lan either: refused, not moved there.
        let Some(vid) = vlan.get("id") else {
            reject(plan, &format!("{at}/vlan"), vlan, "a VLAN without an id");
            by_interface.push(None);
            continue;
        };
        let Some(vid) = vid
            .as_u64()
            .filter(|v| (1..=4094).contains(v))
            .map(|v| v as u16)
        else {
            reject(
                plan,
                &format!("{at}/vlan/id"),
                vid,
                "a VLAN id is 1 to 4094",
            );
            by_interface.push(None);
            continue;
        };
        if vids.contains(&vid) {
            reject(
                plan,
                &format!("{at}/vlan/id"),
                &json!(vid),
                "another interface has this VLAN",
            );
            by_interface.push(None);
            continue;
        }
        vids.push(vid);
        let Some((bridge, bridge_ports)) = &current.bridge else {
            reject(
                plan,
                &format!("{at}/vlan"),
                &json!(vid),
                "the device's bridge doesn't filter VLANs; converting it isn't supported",
            );
            by_interface.push(None);
            continue;
        };
        // What its ethernet selects (none: every bridge port), with what doesn't parse or
        // isn't on the board rejected, for a joined VLAN as for a new one.
        let selected = match ethernet {
            Some(Ok(entries)) => Some(selection(&at, entries, ports, plan)),
            _ => None,
        };
        match current.vlans.get(&vid) {
            // Someone else's VLAN: joined as it is. Ports asked for that it doesn't carry
            // that way are the one thing refused.
            Some(v) if !v.owned => {
                if let Some(selected) = &selected {
                    let mut asked: Vec<_> = selected
                        .iter()
                        .map(|s| (s.port.as_str(), s.tagged))
                        .collect();
                    let mut has: Vec<_> = v.ports.iter().map(|m| membership(m)).collect();
                    asked.sort();
                    asked.dedup();
                    has.sort();
                    if asked != has {
                        reject(
                            plan,
                            &format!("{at}/ethernet"),
                            &iface["ethernet"],
                            format!(
                                "VLAN {vid} is the device's own ({}); its ports stay {}",
                                v.section,
                                v.ports.join(" ")
                            ),
                        );
                    }
                }
            }
            _ => {
                let members = members(
                    &at,
                    selected.as_deref(),
                    vid,
                    bridge_ports,
                    current,
                    &mut untagged_here,
                    plan,
                );
                if members.is_empty() {
                    reject(
                        plan,
                        &format!("{at}/ethernet"),
                        &iface["ethernet"],
                        "none of the device's bridge ports can carry this VLAN",
                    );
                    by_interface.push(None);
                    continue;
                }
                let bv = format!("{PREFIX}bv{vid}");
                current.put(
                    plan,
                    "bridge-vlan",
                    &bv,
                    section(&[
                        ("device", json!(bridge)),
                        ("vlan", json!(vid.to_string())),
                        ("ports", json!(members)),
                    ]),
                );
                wanted.push(bv);
            }
        }
        let device = format!("{bridge}.{vid}");
        let network = match current
            .interfaces
            .iter()
            .find(|(n, d)| **d == device && !current.is_owned(n))
        {
            // Someone else's interface on the VLAN: joined as it is.
            Some((n, _)) => n.clone(),
            None => {
                let name = format!("{PREFIX}vlan{vid}");
                current.put(
                    plan,
                    "interface",
                    &name,
                    section(&[("device", json!(device)), ("proto", json!("none"))]),
                );
                wanted.push(name.clone());
                name
            }
        };
        by_interface.push(Some(network));
    }
    (by_interface, wanted)
}
