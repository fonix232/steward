//! The state document the agent reports every minute: uCentral's `state` (TIP's `state/*.yml`
//! in wlan-ucentral-schema). It holds the unit, its radios, its logical interfaces with the
//! SSIDs on them, and its ports' links.
//!
//! [`gather`] reads the sources: ubus, board.json, sysfs and /proc/stat. [`document`] builds
//! the document from them, and is pure, so it's tested against answers shaped like a real
//! device's. Every field is picked by name, because a wifi-iface's config holds its key:
//! nothing from a source is copied whole. Rates (CPU load, channel utilisation) cover the
//! time since the previous report, which [`Previous`] carries. A source that's missing
//! leaves its part out. Who's connected (stations, clients, leases) is `clients`'.

use crate::clients::{self, Fdb, Lease, Neighbour, Seen};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use steward_render::{PREFIX, Ports};
use steward_ubus::Ubus;

/// A CPU's time counters from /proc/stat, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// A radio's live state.
#[derive(Debug, Default)]
pub struct Phy {
    /// Its name (`wl0`, `phy0`).
    pub name: String,
    /// `iwinfo info` for it.
    pub info: Value,
    /// `iwinfo survey` results.
    pub survey: Value,
    /// Its hwmon sensor, in °C.
    pub temperature: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Sources {
    /// Unix time.
    pub now: u64,
    /// `system info`.
    pub info: Value,
    /// /proc/stat: all CPUs, then each core.
    pub cpu: Vec<CpuTimes>,
    /// The CPU's thermal zones, in °C.
    pub thermal: Vec<f64>,
    /// `network.wireless status`.
    pub wireless: Value,
    /// By radio section (`radio0`).
    pub phys: BTreeMap<String, Phy>,
    /// `network.device status`: every device, by name.
    pub devices: Value,
    /// `network.interface dump`.
    pub interfaces: Value,
    pub ports: Ports,
    /// `iwinfo assoclist` results, by wireless interface.
    pub assoc: BTreeMap<String, Value>,
    /// What the bridges learned.
    pub fdb: Vec<Fdb>,
    /// IPv4 neighbours.
    pub arp: Vec<Neighbour>,
    /// `luci-rpc getHostHints`.
    pub hints: Value,
    /// DHCPv4 leases, where the device serves DHCP.
    pub leases: Vec<Lease>,
    /// The default gateway.
    pub gateway: Option<String>,
}

/// What the last report's rates start from.
#[derive(Debug, Default)]
pub struct Previous {
    cpu: Vec<CpuTimes>,
    /// Radio section → (frequency, active, busy): its channel, and the time on it.
    survey: BTreeMap<String, (u64, u64, u64)>,
}

/// Parts of a percentage, rounded; none when no time passed.
fn percent(part: u64, whole: u64) -> Option<u64> {
    (whole > 0).then(|| (part * 100 + whole / 2) / whole)
}

/// Rounded to a tenth.
fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn unit(s: &Sources, prev: &mut Previous) -> Value {
    let info = &s.info;
    let load: Option<Vec<f64>> = info["load"].as_array().map(|l| {
        l.iter()
            .map(|v| v.as_f64().unwrap_or(0.0) / 65536.0)
            .collect()
    });
    let m = &info["memory"];
    let mut u = json!({
        "load": load,
        // Not system info's localtime: that one is shifted by the timezone.
        "localtime": s.now,
        "uptime": info["uptime"],
        "boottime": info["uptime"].as_u64().map(|up| s.now.saturating_sub(up)),
        "memory": {
            "total": m["total"], "free": m["free"], "cached": m["cached"], "buffered": m["buffered"],
        },
    });
    if prev.cpu.len() == s.cpu.len() && !s.cpu.is_empty() {
        let load: Option<Vec<u64>> = s
            .cpu
            .iter()
            .zip(&prev.cpu)
            .map(|(now, was)| {
                percent(
                    now.busy.saturating_sub(was.busy),
                    now.total.saturating_sub(was.total),
                )
            })
            .collect();
        u["cpu_load"] = json!(load);
    }
    prev.cpu = s.cpu.clone();
    if !s.thermal.is_empty() {
        let avg = s.thermal.iter().sum::<f64>() / s.thermal.len() as f64;
        let max = s.thermal.iter().cloned().fold(f64::MIN, f64::max);
        u["temperature"] = json!([tenth(avg), tenth(max)]);
    }
    u
}

/// A channel's centre frequency, in MHz.
fn frequency(band: &str, channel: u64) -> Option<u64> {
    match (band, channel) {
        ("2G", 14) => Some(2484),
        ("2G", c) => Some(2407 + 5 * c),
        ("5G", c) => Some(5000 + 5 * c),
        ("6G", 2) => Some(5935),
        ("6G", c) => Some(5950 + 5 * c),
        _ => None,
    }
}

/// The width in an htmode (`HE80` → 80); 20 without one.
fn width(htmode: &str) -> u64 {
    htmode
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .unwrap_or(20)
}

/// Every 20 MHz channel a channel of `width` centred on `center` spans.
fn spanned(center: u64, width: u64) -> Vec<u64> {
    let n = (width / 20).max(1);
    let first = center.saturating_sub(2 * (n - 1));
    (0..n).map(|k| first + 4 * k).collect()
}

struct Radio {
    section: String,
    phy: String,
    band: Option<String>,
    frequencies: Vec<u64>,
    doc: Value,
}

fn radios(s: &Sources, prev: &mut Previous) -> Vec<Radio> {
    let mut out = vec![];
    let Some(wireless) = s.wireless.as_object() else {
        return out;
    };
    for (section, r) in wireless {
        let band = r["config"]["band"].as_str().map(str::to_ascii_uppercase);
        let empty = Phy::default();
        let phy = s.phys.get(section).unwrap_or(&empty);
        let info = &phy.info;
        let mut doc = json!({
            "phy": phy.name, "band": band.as_ref().map(|b| [b]),
            "tx_power": info["txpower"], "temperature": phy.temperature,
        });
        let mut frequencies = vec![];
        if let Some(channel) = info["channel"].as_u64() {
            let w = width(info["htmode"].as_str().unwrap_or(""));
            let center = info["center_chan1"].as_u64().unwrap_or(channel);
            let channels = if w <= 20 {
                vec![channel]
            } else {
                spanned(center, w)
            };
            frequencies = channels
                .iter()
                .filter_map(|c| frequency(band.as_deref().unwrap_or(""), *c))
                .collect();
            doc["channel"] = json!(channel);
            doc["channel_width"] = json!(w);
            doc["channels"] = json!(channels);
            doc["frequency"] = json!(frequencies);
        }
        // Channel utilisation: busy over active time on its channel, since the last report.
        // The survey counts per channel, so after a channel change the rate starts over.
        let mhz = info["frequency"].as_u64();
        let on_channel = mhz.and_then(|mhz| {
            let results = phy.survey.as_array()?;
            results.iter().find(|r| r["mhz"] == mhz)
        });
        if let (Some(mhz), Some(r)) = (mhz, on_channel)
            && let (Some(active), Some(busy)) = (r["active_time"].as_u64(), r["busy_time"].as_u64())
        {
            if let Some((f, a, b)) = prev.survey.get(section)
                && *f == mhz
                && active >= *a
                && busy >= *b
            {
                doc["chanUtil"] = json!(percent(busy - b, active - a));
            }
            prev.survey.insert(section.clone(), (mhz, active, busy));
        }
        out.push(Radio {
            section: section.clone(),
            phy: phy.name.clone(),
            band,
            frequencies,
            doc,
        });
    }
    out
}

/// The traffic counters uCentral reports, from a device's `statistics`.
fn counters(device: &Value) -> Value {
    let st = &device["statistics"];
    if !st.is_object() {
        return Value::Null;
    }
    let names = [
        "collisions",
        "multicast",
        "rx_bytes",
        "rx_packets",
        "rx_errors",
        "rx_dropped",
        "tx_bytes",
        "tx_packets",
        "tx_errors",
        "tx_dropped",
    ];
    Value::Object(
        names
            .iter()
            .filter(|n| !st[**n].is_null())
            .map(|n| (n.to_string(), st[*n].clone()))
            .collect(),
    )
}

/// `/interfaces/<i>/ssids/<s>` for the agent's own SSID sections (`stw_<i>_<s>_<band>`).
fn location(section: &str) -> Option<String> {
    let mut parts = section.strip_prefix(PREFIX)?.split('_');
    let (i, s) = (parts.next()?, parts.next()?);
    (i.parse::<u64>().is_ok() && s.parse::<u64>().is_ok())
        .then(|| format!("/interfaces/{i}/ssids/{s}"))
}

/// The SSIDs on the logical interface `network`, with their stations.
fn ssids(network: &str, s: &Sources, radios: &[Radio]) -> Vec<Value> {
    let mut out = vec![];
    for (n, radio) in radios.iter().enumerate() {
        let ifaces = s.wireless[&radio.section]["interfaces"].as_array();
        for iface in ifaces.into_iter().flatten() {
            let config = &iface["config"];
            let on = config["network"]
                .as_array()
                .is_some_and(|nets| nets.iter().any(|x| x == network));
            let Some(ifname) = iface["ifname"].as_str().filter(|_| on) else {
                continue;
            };
            let device = &s.devices[ifname];
            out.push(json!({
                "bssid": device["macaddr"],
                "ssid": config["ssid"],
                "mode": config["mode"],
                "band": radio.band.as_ref().map(|b| [b]),
                "phy": radio.phy,
                "iface": ifname,
                "frequency": radio.frequencies,
                "radio": { "$ref": format!("#/radios/{n}") },
                "counters": counters(device),
                "location": iface["section"].as_str().and_then(location),
                "associations": s.assoc.get(ifname).map(|r| clients::associations(r, &device["macaddr"])),
            }));
        }
    }
    out
}

/// Every wireless interface's name.
fn wireless_ifnames(s: &Sources) -> BTreeSet<String> {
    let radios = s.wireless.as_object().into_iter().flatten();
    radios
        .flat_map(|(_, r)| r["interfaces"].as_array().into_iter().flatten())
        .filter_map(|i| i["ifname"].as_str().map(str::to_owned))
        .collect()
}

/// The logical interface that gets a bridge's MACs no IP places: `lan` when it's on the
/// bridge, else the first on it. The kernel's brforward has no VLANs.
fn fdb_home<'a>(s: &'a Sources, seen: &Seen) -> BTreeMap<String, &'a str> {
    let mut home = BTreeMap::new();
    for iface in s.interfaces["interface"].as_array().into_iter().flatten() {
        let (Some(name), Some(l3)) = (iface["interface"].as_str(), iface["l3_device"].as_str())
        else {
            continue;
        };
        if let Some(bridge) = seen.bridge_of(l3) {
            let e = home.entry(bridge.to_owned()).or_insert(name);
            if name == "lan" {
                *e = name;
            }
        }
    }
    home
}

/// The L3 devices of the interfaces that carry a default route, where they aren't on a
/// bridge: a router's WAN. Everything on them is upstream (the ISP's gateway, a modem), not
/// a client. On a bridge (an AP's lan), the uplink port tells what's upstream.
fn upstream<'a>(s: &'a Sources, seen: &Seen) -> BTreeSet<&'a str> {
    let default = |r: &Value| r["mask"] == 0 && (r["target"] == "0.0.0.0" || r["target"] == "::");
    s.interfaces["interface"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["route"].as_array().is_some_and(|r| r.iter().any(default)))
        .filter_map(|i| i["l3_device"].as_str())
        .filter(|l3| seen.bridge_of(l3).is_none())
        .collect()
}

fn interfaces(s: &Sources, radios: &[Radio]) -> Vec<Value> {
    let mut out = vec![];
    let mut own: BTreeSet<String> = s
        .devices
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, d)| d["macaddr"].as_str().map(str::to_lowercase))
        .collect();
    own.remove("00:00:00:00:00:00");
    let seen = Seen::new(&s.fdb, &s.arp, &s.hints, s.gateway.as_deref(), own);
    let home = fdb_home(s, &seen);
    let upstream = upstream(s, &seen);
    let wireless = wireless_ifnames(s);
    let list = s.interfaces["interface"].as_array();
    for iface in list.into_iter().flatten() {
        let Some(name) = iface["interface"].as_str() else {
            continue;
        };
        if name == "loopback" || iface["l3_device"] == "lo" {
            continue;
        }
        let addresses = |key: &str| -> Vec<String> {
            iface[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| Some(format!("{}/{}", a["address"].as_str()?, a["mask"])))
                .collect()
        };
        let ipv4 = addresses("ipv4-address");
        let ipv6: Vec<Value> = addresses("ipv6-address")
            .into_iter()
            .map(|a| json!({ "address": a }))
            .collect();
        let up = iface["up"] == true;
        let l3 = iface["l3_device"].as_str().unwrap_or("");
        let ssids = ssids(name, s, radios);
        let stations: Vec<(String, String)> = ssids
            .iter()
            .flat_map(|ssid| {
                let ifname = ssid["iface"].as_str().unwrap_or_default().to_owned();
                let stations = ssid["associations"].as_array().cloned().unwrap_or_default();
                stations
                    .into_iter()
                    .filter_map(move |a| Some((a["station"].as_str()?.to_owned(), ifname.clone())))
            })
            .collect();
        let fdb_only = seen
            .bridge_of(l3)
            .is_some_and(|b| home.get(b) == Some(&name));
        let clients = if l3.is_empty() || upstream.contains(l3) {
            vec![]
        } else {
            seen.clients(l3, &stations, fdb_only, &wireless)
        };
        // Leases in this interface's subnets.
        let subnets: Vec<(&str, u32)> = iface["ipv4-address"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| Some((a["address"].as_str()?, a["mask"].as_u64()? as u32)))
            .collect();
        let leases: Vec<Value> = s
            .leases
            .iter()
            .filter(|l| {
                subnets
                    .iter()
                    .any(|(net, p)| clients::in_subnet(&l.ip, net, *p))
            })
            .map(|l| json!({ "address": l.ip, "mac": l.mac, "hostname": l.hostname }))
            .collect();
        out.push(json!({
            "name": name,
            "uptime": if up { iface["uptime"].clone() } else { Value::Null },
            "ipv4": if ipv4.is_empty() { Value::Null } else {
                json!({ "addresses": ipv4, "leases": (!leases.is_empty()).then_some(leases) })
            },
            "clients": (!clients.is_empty()).then_some(clients),
            "ipv6_addresses": if ipv6.is_empty() { Value::Null } else { json!(ipv6) },
            "dns_servers": iface["dns-server"].as_array().filter(|d| !d.is_empty()),
            "counters": if l3.is_empty() { Value::Null } else { counters(&s.devices[l3]) },
            "ssids": if ssids.is_empty() { Value::Null } else { json!(ssids) },
        }));
    }
    out
}

/// A port's link: carrier, and speed and duplex from netifd's `1000F`.
fn link(device: &Value) -> Option<Value> {
    if !device.is_object() {
        return None;
    }
    let speed = device["speed"].as_str().unwrap_or("");
    let (digits, duplex) = speed.split_at(speed.trim_end_matches(['F', 'H']).len());
    Some(json!({
        "carrier": device["carrier"] == true,
        "speed": digits.parse::<u64>().ok(),
        "duplex": match duplex { "F" => Some("full"), "H" => Some("half"), _ => None },
        "counters": counters(device),
    }))
}

/// Drops nulls (and objects and arrays left empty by that) from objects: a missing source
/// leaves its part out rather than reporting it as null.
fn prune(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k, prune(v)))
                .filter(|(_, v)| match v {
                    Value::Null => false,
                    Value::Object(o) => !o.is_empty(),
                    _ => true,
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(prune).collect()),
        v => v,
    }
}

/// The state document.
pub fn document(s: &Sources, prev: &mut Previous) -> Value {
    let unit = unit(s, prev);
    let radios = radios(s, prev);
    let interfaces = interfaces(s, &radios);
    let role = |ports: &[String]| -> Map<String, Value> {
        ports
            .iter()
            .filter_map(|p| Some((p.clone(), link(&s.devices[p])?)))
            .collect()
    };
    let doc = json!({
        "unit": unit,
        "radios": if radios.is_empty() { Value::Null } else { json!(radios.into_iter().map(|r| r.doc).collect::<Vec<_>>()) },
        "interfaces": if interfaces.is_empty() { Value::Null } else { json!(interfaces) },
        "link-state": { "upstream": role(&s.ports.wan), "downstream": role(&s.ports.lan) },
    });
    prune(doc)
}

/// /proc/stat's `cpu` lines: all CPUs, then each core.
pub fn cpu_times(stat: &str) -> Vec<CpuTimes> {
    stat.lines()
        .filter(|l| l.starts_with("cpu"))
        .map(|l| {
            // user nice system idle iowait irq softirq steal (guest time is inside user)
            let f: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .take(8)
                .map(|x| x.parse().unwrap_or(0))
                .collect();
            let total: u64 = f.iter().sum();
            let idle = f.get(3).unwrap_or(&0) + f.get(4).unwrap_or(&0);
            CpuTimes {
                busy: total - idle,
                total,
            }
        })
        .collect()
}

fn read_number(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The CPU's thermal zones, in °C: the zones whose type names the CPU, or all of them when
/// none does.
fn cpu_thermal() -> Vec<f64> {
    let zones: Vec<(String, f64)> = std::fs::read_dir("/sys/class/thermal")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("thermal_zone"))
        .filter_map(|e| {
            let kind = std::fs::read_to_string(e.path().join("type")).unwrap_or_default();
            Some((
                kind.to_lowercase(),
                read_number(&e.path().join("temp"))? / 1000.0,
            ))
        })
        .collect();
    let cpu: Vec<f64> = zones
        .iter()
        .filter(|(k, _)| k.contains("cpu"))
        .map(|(_, t)| *t)
        .collect();
    if cpu.is_empty() {
        zones.into_iter().map(|(_, t)| t).collect()
    } else {
        cpu
    }
}

/// A phy's hwmon sensor, in °C: the hwmon whose device is `.../ieee80211/<phy>`.
fn phy_temperature(phy: &str) -> Option<f64> {
    std::fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .find(|e| {
            std::fs::canonicalize(e.path().join("device"))
                .is_ok_and(|d| d.ends_with(Path::new("ieee80211").join(phy)))
        })
        .and_then(|e| read_number(&e.path().join("temp1_input")))
        .map(|t| t / 1000.0)
}

/// Reads every source. Only `system info` is required; anything else missing is left out.
pub fn gather() -> Result<Sources, steward_ubus::Error> {
    let mut ubus = Ubus::connect()?;
    let info = Value::Object(ubus.call("system", "info", &Map::new())?);
    let mut call = |path: &str, method: &str, args: Value| -> Value {
        let args = args.as_object().cloned().unwrap_or_default();
        ubus.call(path, method, &args)
            .map(Value::Object)
            .unwrap_or(Value::Null)
    };
    let wireless = call("network.wireless", "status", json!({}));
    let devices = call("network.device", "status", json!({}));
    let interfaces = call("network.interface", "dump", json!({}));
    let mut phys = BTreeMap::new();
    for (section, r) in wireless.as_object().into_iter().flatten() {
        let name = match r["config"]["phy"].as_str() {
            Some(p) => p.to_owned(),
            None => call("iwinfo", "phyname", json!({ "section": section }))["phyname"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        };
        // iwinfo's survey needs an interface on the radio, not the phy.
        let device = r["interfaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|i| i["ifname"].as_str())
            .unwrap_or(&name)
            .to_owned();
        let phy = Phy {
            info: call("iwinfo", "info", json!({ "device": device })),
            survey: call("iwinfo", "survey", json!({ "device": device }))["results"].clone(),
            temperature: phy_temperature(&name),
            name,
        };
        phys.insert(section.clone(), phy);
    }
    let assoc = wireless_ifnames(&Sources {
        wireless: wireless.clone(),
        ..Default::default()
    })
    .into_iter()
    .map(|ifname| {
        let results = call("iwinfo", "assoclist", json!({ "device": ifname }))["results"].clone();
        (ifname, results)
    })
    .collect();
    let hints = call("luci-rpc", "getHostHints", json!({}));
    // Leases from luci-rpc (dnsmasq's and odhcpd's), else dnsmasq's file.
    let leases = match call("luci-rpc", "getDHCPLeases", json!({ "family": 4 })) {
        Value::Null => clients::dnsmasq_leases(
            &std::fs::read_to_string("/tmp/dhcp.leases").unwrap_or_default(),
        ),
        answer => clients::luci_leases(&answer),
    };
    let board: Value = std::fs::read_to_string("/etc/board.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(Sources {
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        info,
        cpu: cpu_times(&std::fs::read_to_string("/proc/stat").unwrap_or_default()),
        thermal: cpu_thermal(),
        wireless,
        phys,
        devices,
        interfaces,
        ports: Ports::from_board(&board),
        assoc,
        fdb: bridges_fdb(),
        arp: clients::arp(&std::fs::read_to_string("/proc/net/arp").unwrap_or_default()),
        hints,
        leases,
        gateway: crate::device::default_gateway().map(|g| g.to_string()),
    })
}

/// Every bridge's learned MACs, from sysfs.
fn bridges_fdb() -> Vec<Fdb> {
    let mut out = vec![];
    for entry in std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
    {
        let (dir, bridge) = (
            entry.path(),
            entry.file_name().to_string_lossy().into_owned(),
        );
        let Ok(data) = std::fs::read(dir.join("brforward")) else {
            continue;
        };
        let ports: BTreeMap<u16, String> = std::fs::read_dir(dir.join("brif"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|p| {
                let n = std::fs::read_to_string(p.path().join("port_no")).ok()?;
                let n = u16::from_str_radix(n.trim().trim_start_matches("0x"), 16).ok()?;
                Some((n, p.file_name().to_string_lossy().into_owned()))
            })
            .collect();
        out.extend(clients::brforward(&bridge, &data, &ports));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    /// Shaped like bifrost's answers: two radios, the device's own SSID and two of the
    /// agent's (one on a VLAN it made), five switch ports with lan4 the uplink.
    fn sources() -> Sources {
        let phy = |name: &str, info: Value, survey: Value, t: f64| Phy {
            name: name.into(),
            info,
            survey,
            temperature: Some(t),
        };
        let stats = |rx: u64| json!({ "statistics": { "rx_bytes": rx, "tx_bytes": rx * 2, "rx_crc_errors": 0 } });
        let mut devices = json!({
            "lan4": { "carrier": true, "speed": "1000F" },
            "lan1": { "carrier": false },
            "wan": { "carrier": false },
            "br-lan.1": stats(100), "br-lan.10": stats(10),
            "wl0-ap0": { "macaddr": "00:00:5e:00:53:01" }, "wl1-ap0": { "macaddr": "00:00:5e:00:53:02" },
            "wl1-ap1": { "macaddr": "00:00:5e:00:53:03" },
        });
        for d in ["lan4", "lan1", "wan", "wl0-ap0", "wl1-ap0", "wl1-ap1"] {
            devices[d]["statistics"] = stats(7)["statistics"].clone();
        }
        Sources {
            now: NOW,
            info: json!({
                "localtime": NOW + 3600, "uptime": 1000, "load": [32768, 65536, 0],
                "memory": { "total": 512, "free": 200, "shared": 100, "buffered": 4, "available": 220, "cached": 2 },
            }),
            cpu: vec![
                CpuTimes {
                    busy: 100,
                    total: 1000,
                },
                CpuTimes {
                    busy: 50,
                    total: 500,
                },
                CpuTimes {
                    busy: 50,
                    total: 500,
                },
            ],
            thermal: vec![62.3, 58.0],
            wireless: json!({
                "radio0": { "up": true, "config": { "band": "2g", "phy": "wl0", "channel": "auto" }, "interfaces": [
                    { "section": "default_radio0", "ifname": "wl0-ap0",
                      "config": { "network": ["lan"], "ssid": "OpenWrt", "mode": "ap", "encryption": "psk2", "key": "secret-key-0" } }
                ]},
                "radio1": { "up": true, "config": { "band": "5g", "phy": "wl1" }, "interfaces": [
                    { "section": "stw_1_0_5g", "ifname": "wl1-ap0",
                      "config": { "network": ["stw_vlan10"], "ssid": "Guest", "mode": "ap", "key": "secret-key-1" } },
                    { "section": "stw_0_0_5g", "ifname": "wl1-ap1",
                      "config": { "network": ["lan"], "ssid": "Home", "mode": "ap", "key": "secret-key-2" } },
                    { "section": "stw_0_1_5g", "config": { "network": ["lan"], "ssid": "Down", "key": "secret-key-3" } }
                ]},
            }),
            phys: BTreeMap::from([
                (
                    "radio0".into(),
                    phy(
                        "wl0",
                        json!({ "channel": 6, "center_chan1": 8, "frequency": 2437, "txpower": 20, "htmode": "HT40" }),
                        json!([{ "mhz": 2437, "active_time": 1000, "busy_time": 100 }]),
                        55.0,
                    ),
                ),
                (
                    "radio1".into(),
                    phy(
                        "wl1",
                        json!({ "channel": 36, "center_chan1": 42, "frequency": 5180, "txpower": 23, "htmode": "HE80" }),
                        json!([{ "mhz": 5200, "active_time": 9, "busy_time": 9 }, { "mhz": 5180, "active_time": 2000, "busy_time": 500 }]),
                        60.0,
                    ),
                ),
            ]),
            devices,
            interfaces: json!({ "interface": [
                { "interface": "lan", "up": true, "uptime": 300, "l3_device": "br-lan.1",
                  "ipv4-address": [{ "address": "192.0.2.4", "mask": 24 }],
                  "ipv6-address": [{ "address": "2001:db8::4", "mask": 64 }], "dns-server": ["192.0.2.1"] },
                { "interface": "loopback", "up": true, "l3_device": "lo" },
                { "interface": "stw_vlan10", "up": true, "uptime": 20, "l3_device": "br-lan.10",
                  "ipv4-address": [], "ipv6-address": [], "dns-server": [] },
                { "interface": "wan6", "up": false },
            ]}),
            ports: Ports {
                lan: vec!["lan1".into(), "lan2".into(), "lan3".into(), "lan4".into()],
                wan: vec!["wan".into()],
            },
            ..Default::default()
        }
    }

    #[test]
    fn the_unit_reports_unix_time_and_rates_since_the_last_report() {
        let mut prev = Previous::default();
        let mut s = sources();
        let doc = document(&s, &mut prev);
        let u = &doc["unit"];
        assert_eq!(
            u["localtime"], NOW,
            "unix time, not the timezone-shifted one"
        );
        assert_eq!(u["boottime"], NOW - 1000);
        assert_eq!(u["load"], json!([0.5, 1.0, 0.0]));
        assert_eq!(
            u["memory"],
            json!({ "total": 512, "free": 200, "cached": 2, "buffered": 4 })
        );
        assert_eq!(u["temperature"], json!([60.2, 62.3]));
        assert!(u.get("cpu_load").is_none(), "no rate on the first report");
        // A minute later: the whole CPU 25% busy, core 0 40%, core 1 10%.
        s.cpu = vec![
            CpuTimes {
                busy: 150,
                total: 1200,
            },
            CpuTimes {
                busy: 90,
                total: 600,
            },
            CpuTimes {
                busy: 60,
                total: 600,
            },
        ];
        assert_eq!(
            document(&s, &mut prev)["unit"]["cpu_load"],
            json!([25, 40, 10])
        );
    }

    #[test]
    fn radios_report_what_they_span() {
        let mut prev = Previous::default();
        let mut s = sources();
        let doc = document(&s, &mut prev);
        let r0 = &doc["radios"][0];
        assert_eq!(r0["phy"], "wl0");
        assert_eq!(r0["band"], json!(["2G"]));
        assert_eq!(
            (r0["channel"].clone(), r0["channel_width"].clone()),
            (json!(6), json!(40))
        );
        assert_eq!(r0["channels"], json!([6, 10]));
        assert_eq!(r0["frequency"], json!([2437, 2457]));
        assert_eq!(
            (r0["tx_power"].clone(), r0["temperature"].clone()),
            (json!(20), json!(55.0))
        );
        let r1 = &doc["radios"][1];
        assert_eq!(r1["channels"], json!([36, 40, 44, 48]));
        assert_eq!(r1["frequency"], json!([5180, 5200, 5220, 5240]));
        assert!(r1.get("chanUtil").is_none(), "no rate on the first report");
        // A minute later on its channel: 1000 ms active, 400 busy.
        s.phys.get_mut("radio1").unwrap().survey =
            json!([{ "mhz": 5180, "active_time": 3000, "busy_time": 900 }]);
        assert_eq!(document(&s, &mut prev)["radios"][1]["chanUtil"], 40);
    }

    #[test]
    fn chan_util_starts_over_on_a_new_channel_or_a_reset_counter() {
        let mut prev = Previous::default();
        let mut util = |s: &Sources| document(s, &mut prev)["radios"][1].get("chanUtil").cloned();
        let mut s = sources();
        util(&s);
        // DFS moved the radio to channel 100, whose counters are larger: two channels'
        // counters give no rate (they gave 240%).
        let radio1 = s.phys.get_mut("radio1").unwrap();
        radio1.info = json!({ "channel": 100, "frequency": 5500, "htmode": "HE20" });
        radio1.survey = json!([
            { "mhz": 5180, "active_time": 2500, "busy_time": 600 },
            { "mhz": 5500, "active_time": 3000, "busy_time": 2900 },
        ]);
        assert_eq!(util(&s), None);
        // A minute later on channel 100: 1000 ms active, 250 busy.
        let survey = |a: u64, b: u64| json!([{ "mhz": 5500, "active_time": a, "busy_time": b }]);
        s.phys.get_mut("radio1").unwrap().survey = survey(4000, 3150);
        assert_eq!(util(&s), Some(json!(25)));
        // The driver reset its counters: no rate, then it resumes from the new ones.
        s.phys.get_mut("radio1").unwrap().survey = survey(100, 50);
        assert_eq!(util(&s), None);
        s.phys.get_mut("radio1").unwrap().survey = survey(1100, 150);
        assert_eq!(util(&s), Some(json!(10)));
    }

    #[test]
    fn interfaces_carry_their_ssids() {
        let doc = document(&sources(), &mut Previous::default());
        let ifaces = doc["interfaces"].as_array().unwrap();
        let names: Vec<&str> = ifaces.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["lan", "stw_vlan10", "wan6"], "loopback left out");
        let lan = &ifaces[0];
        assert_eq!(lan["uptime"], 300);
        assert_eq!(lan["ipv4"]["addresses"], json!(["192.0.2.4/24"]));
        assert_eq!(
            lan["ipv6_addresses"],
            json!([{ "address": "2001:db8::4/64" }])
        );
        assert_eq!(lan["dns_servers"], json!(["192.0.2.1"]));
        assert_eq!(lan["counters"], json!({ "rx_bytes": 100, "tx_bytes": 200 }));
        let ssids = lan["ssids"].as_array().unwrap();
        assert_eq!(
            ssids.len(),
            2,
            "the SSID that isn't up has no interface to report"
        );
        assert_eq!(ssids[0]["ssid"], "OpenWrt");
        assert_eq!(ssids[0]["bssid"], "00:00:5e:00:53:01");
        assert_eq!(ssids[0]["radio"], json!({ "$ref": "#/radios/0" }));
        assert!(ssids[0].get("location").is_none(), "the device's own SSID");
        assert_eq!(ssids[1]["ssid"], "Home");
        assert_eq!(ssids[1]["location"], "/interfaces/0/ssids/0");
        assert_eq!(ssids[1]["frequency"], json!([5180, 5200, 5220, 5240]));
        assert_eq!(ssids[1]["iface"], "wl1-ap1");
        let guest = &ifaces[1]["ssids"][0];
        assert_eq!(
            (guest["ssid"].clone(), guest["location"].clone()),
            (json!("Guest"), json!("/interfaces/1/ssids/0"))
        );
        // Down, with no addresses: only its name.
        assert_eq!(ifaces[2], json!({ "name": "wan6" }));
    }

    #[test]
    fn ports_report_their_links_by_role() {
        let doc = document(&sources(), &mut Previous::default());
        let down = &doc["link-state"]["downstream"];
        assert_eq!(down["lan4"]["carrier"], true);
        assert_eq!(
            (
                down["lan4"]["speed"].clone(),
                down["lan4"]["duplex"].clone()
            ),
            (json!(1000), json!("full"))
        );
        assert_eq!(down["lan4"]["counters"]["rx_bytes"], 7);
        assert_eq!(
            down["lan1"],
            json!({ "carrier": false, "counters": { "rx_bytes": 7, "tx_bytes": 14 } })
        );
        assert!(down.get("lan2").is_none(), "no such device: left out");
        assert_eq!(doc["link-state"]["upstream"]["wan"]["carrier"], false);
    }

    #[test]
    fn stations_clients_and_leases_are_reported() {
        let mut s = sources();
        s.assoc.insert(
            "wl1-ap1".into(),
            json!([{ "mac": "00:00:5e:00:53:21", "signal": -40, "connected_time": 60, "inactive": 1,
                     "rx": { "rate": 6000 }, "tx": { "rate": 144400, "mcs": 15 } }]),
        );
        let fdb = |mac: &str, port: &str| Fdb {
            bridge: "br-lan".into(),
            mac: mac.into(),
            port: port.into(),
            local: false,
        };
        s.fdb = vec![
            fdb("00:00:5e:00:53:40", "lan4"), // the gateway: lan4 is the uplink
            fdb("00:00:5e:00:53:41", "lan4"), // behind the uplink
            fdb("00:00:5e:00:53:31", "lan1"), // wired, here, no IP seen
            fdb("00:00:5e:00:53:21", "wl1-ap1"),
            fdb("00:00:5e:00:53:02", "wl1-ap0"), // a BSSID: the device's own
        ];
        s.arp = vec![Neighbour {
            ip: "192.0.2.1".into(),
            mac: "00:00:5e:00:53:40".into(),
            device: "br-lan.1".into(),
        }];
        s.gateway = Some("192.0.2.1".into());
        // An AP's default route is on its bridge: the uplink port tells what's upstream.
        s.interfaces["interface"][0]["route"] =
            json!([{ "target": "0.0.0.0", "mask": 0, "nexthop": "192.0.2.1" }]);
        s.leases = vec![
            Lease {
                ip: "192.0.2.50".into(),
                mac: "00:00:5e:00:53:31".into(),
                hostname: Some("printer".into()),
            },
            Lease {
                ip: "198.51.100.9".into(),
                mac: "00:00:5e:00:53:32".into(),
                hostname: None,
            },
        ];
        let doc = document(&s, &mut Previous::default());
        let lan = &doc["interfaces"][0];
        let home = &lan["ssids"][1];
        assert_eq!(home["associations"][0]["station"], "00:00:5e:00:53:21");
        assert_eq!(home["associations"][0]["bssid"], "00:00:5e:00:53:03");
        assert_eq!(home["associations"][0]["tx_rate"]["bitrate"], 144400);
        let macs: Vec<&str> = lan["clients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["mac"].as_str().unwrap())
            .collect();
        assert_eq!(
            macs,
            ["00:00:5e:00:53:21", "00:00:5e:00:53:31"],
            "not the gateway, nor what's behind it, nor its own"
        );
        assert_eq!(
            lan["ipv4"]["leases"],
            json!([{ "address": "192.0.2.50", "mac": "00:00:5e:00:53:31", "hostname": "printer" }])
        );
        // The VLAN's interface gets neither lan's FDB-only MACs nor its leases.
        assert!(doc["interfaces"][1].get("clients").is_none());
    }

    /// A router's default route is on its WAN, which isn't a bridge, so no uplink port is
    /// found: the WAN's neighbours (the ISP's gateway, a modem) are upstream, not clients.
    #[test]
    fn a_routers_wan_neighbours_are_not_clients() {
        let neighbour = |ip: &str, mac: &str, device: &str| Neighbour {
            ip: ip.into(),
            mac: mac.into(),
            device: device.into(),
        };
        let s = Sources {
            now: NOW,
            info: json!({ "uptime": 1 }),
            interfaces: json!({ "interface": [
                { "interface": "lan", "up": true, "l3_device": "br-lan",
                  "ipv4-address": [{ "address": "192.0.2.1", "mask": 24 }] },
                { "interface": "wan", "up": true, "l3_device": "wan",
                  "ipv4-address": [{ "address": "198.51.100.2", "mask": 24 }],
                  "route": [{ "target": "0.0.0.0", "mask": 0, "nexthop": "198.51.100.1" }] },
                { "interface": "wan6", "up": true, "l3_device": "wan",
                  "route": [{ "target": "::", "mask": 0, "nexthop": "fe80::1" }] },
            ]}),
            fdb: vec![Fdb {
                bridge: "br-lan".into(),
                mac: "00:00:5e:00:53:31".into(),
                port: "lan1".into(),
                local: false,
            }],
            arp: vec![
                neighbour("198.51.100.1", "00:00:5e:00:53:99", "wan"),
                neighbour("198.51.100.254", "00:00:5e:00:53:98", "wan"),
                neighbour("192.0.2.31", "00:00:5e:00:53:31", "br-lan"),
            ],
            gateway: Some("198.51.100.1".into()),
            ..Default::default()
        };
        let doc = document(&s, &mut Previous::default());
        let ifaces = doc["interfaces"].as_array().unwrap();
        assert_eq!(ifaces[0]["clients"][0]["mac"], "00:00:5e:00:53:31");
        assert_eq!(ifaces[0]["clients"][0]["ports"], json!(["lan1"]));
        for wan in &ifaces[1..] {
            assert!(wan.get("clients").is_none(), "{wan}");
        }
    }

    #[test]
    fn no_key_reaches_the_document() {
        let text = document(&sources(), &mut Previous::default()).to_string();
        assert!(!text.contains("secret-key"), "{text}");
        assert!(!text.contains("\"key\""));
    }

    #[test]
    fn missing_sources_leave_their_parts_out() {
        let s = Sources {
            now: NOW,
            info: json!({ "uptime": 5 }),
            ..Default::default()
        };
        let doc = document(&s, &mut Previous::default());
        assert_eq!(
            doc,
            json!({ "unit": { "localtime": NOW, "uptime": 5, "boottime": NOW - 5 } })
        );
    }

    #[test]
    fn proc_stat_gives_busy_and_total_time() {
        let stat = "cpu  4156443 0 25669653 269909235 55 6691453 9049007 0 0 0\ncpu0 2765639 0 8100010 139298565 31 3886897 3695963 0 0 0\nintr 1 2 3\n";
        let t = cpu_times(stat);
        assert_eq!(t.len(), 2);
        assert_eq!(
            t[0].total,
            4156443 + 25669653 + 269909235 + 55 + 6691453 + 9049007
        );
        assert_eq!(t[0].total - t[0].busy, 269909235 + 55);
    }

    #[test]
    fn channels_span_their_width() {
        assert_eq!(spanned(50, 160), [36, 40, 44, 48, 52, 56, 60, 64]);
        assert_eq!(width("EHT320"), 320);
        assert_eq!(width("NOHT"), 20);
        assert_eq!(frequency("6G", 5), Some(5975));
        assert_eq!(frequency("2G", 14), Some(2484));
    }
}
