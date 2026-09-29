//! One client list from every device's latest state: each MAC once, with its name (from a
//! DHCP lease), its addresses, its network, and where it's connected.
//! - **Wireless** wins: a station associated to an SSID is there. If two APs list it while
//!   it roams, the one it was last active on: each state's arrival less the association's
//!   `inactive`, since the states arrive at different times.
//! - **Wired**: the sighting on the port with the fewest clients on it. A client behind an
//!   AP is seen on the AP's port and on the router's port towards the AP, and the AP's port
//!   is the one it's plugged into. Topology (LLDP) will make this exact.
//! - The managed devices' own MACs (their capabilities' `macaddr`, their BSSIDs) aren't
//!   clients.
//! - Only connected devices' states count: an offline AP's last state says who was on it,
//!   not who is. Its own MACs are still left out.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A device's latest state, and what it reported on connecting.
pub struct Report<'a> {
    pub serial: &'a str,
    pub capabilities: &'a Value,
    pub state: &'a Value,
    /// When the state arrived.
    pub received: u64,
    /// Whether the device is connected.
    pub connected: bool,
}

#[derive(Default)]
struct Client {
    name: Option<String>,
    ipv4: BTreeSet<String>,
    ipv6: BTreeSet<String>,
    network: Option<String>,
    /// (when it was last active, in ms since the epoch; where)
    wireless: Option<(u64, Value)>,
    /// (clients on that port, where)
    wired: Option<(usize, Value)>,
    last_seen: u64,
}

fn arr(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn strings(v: &Value) -> impl Iterator<Item = String> + '_ {
    arr(v).filter_map(Value::as_str).map(str::to_lowercase)
}

pub fn merge(reports: &[Report]) -> Vec<Value> {
    let mut managed: BTreeSet<String> = BTreeSet::new();
    let mut names: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
    for r in reports {
        if let Some(macs) = r.capabilities["macaddr"].as_object() {
            managed.extend(
                macs.values()
                    .filter_map(Value::as_str)
                    .map(str::to_lowercase),
            );
        }
        for iface in arr(&r.state["interfaces"]) {
            managed.extend(
                arr(&iface["ssids"])
                    .filter_map(|s| s["bssid"].as_str())
                    .map(str::to_lowercase),
            );
            for l in arr(&iface["ipv4"]["leases"]).filter(|_| r.connected) {
                if let (Some(mac), Some(ip)) = (l["mac"].as_str(), l["address"].as_str()) {
                    let name = l["hostname"].as_str().map(str::to_owned);
                    names.insert(mac.to_lowercase(), (name, ip.to_owned()));
                }
            }
        }
    }

    let mut clients: BTreeMap<String, Client> = BTreeMap::new();
    for r in reports.iter().filter(|r| r.connected) {
        let interfaces: Vec<&Value> = arr(&r.state["interfaces"]).collect();
        let wifi: BTreeSet<&str> = interfaces
            .iter()
            .flat_map(|i| arr(&i["ssids"]))
            .filter_map(|s| s["iface"].as_str())
            .collect();
        // How many clients each of this device's ports has.
        let mut on_port: BTreeMap<&str, usize> = BTreeMap::new();
        for c in interfaces.iter().flat_map(|i| arr(&i["clients"])) {
            for p in arr(&c["ports"]).filter_map(Value::as_str) {
                *on_port.entry(p).or_default() += 1;
            }
        }
        for iface in &interfaces {
            let network = iface["name"].as_str().map(str::to_owned);
            for ssid in arr(&iface["ssids"]) {
                for a in arr(&ssid["associations"]) {
                    let Some(mac) = a["station"].as_str().map(str::to_lowercase) else {
                        continue;
                    };
                    // iwinfo's `inactive` is in ms, up to when the state was gathered.
                    let active = a["inactive"]
                        .as_u64()
                        .map_or(0, |i| (r.received * 1000).saturating_sub(i));
                    let c = clients.entry(mac).or_default();
                    if c.wireless.as_ref().is_none_or(|(t, _)| active > *t) {
                        c.network = network.clone();
                        c.wireless = Some((
                            active,
                            json!({
                                "type": "wireless", "device": r.serial, "ssid": ssid["ssid"],
                                "band": ssid["band"][0], "iface": ssid["iface"], "signal": a["rssi"],
                                "connected": a["connected"], "inactive": a["inactive"],
                                "rx_rate": a["rx_rate"]["bitrate"], "tx_rate": a["tx_rate"]["bitrate"],
                            }),
                        ));
                    }
                    c.last_seen = c.last_seen.max(r.received);
                }
            }
            for seen in arr(&iface["clients"]) {
                let Some(mac) = seen["mac"].as_str().map(str::to_lowercase) else {
                    continue;
                };
                let c = clients.entry(mac).or_default();
                c.ipv4.extend(strings(&seen["ipv4_addresses"]));
                c.ipv6.extend(strings(&seen["ipv6_addresses"]));
                c.last_seen = c.last_seen.max(r.received);
                let wired = arr(&seen["ports"])
                    .filter_map(Value::as_str)
                    .find(|p| !wifi.contains(p));
                if let Some(port) = wired {
                    let count = on_port.get(port).copied().unwrap_or(usize::MAX);
                    if c.wired.as_ref().is_none_or(|(n, _)| count < *n) {
                        c.wired = Some((
                            count,
                            json!({ "type": "wired", "device": r.serial, "port": port }),
                        ));
                        if c.wireless.is_none() {
                            c.network = network.clone();
                        }
                    }
                }
                if c.network.is_none() {
                    c.network = network.clone();
                }
            }
        }
    }

    let mut out = vec![];
    for (mac, mut c) in clients {
        if managed.contains(&mac) {
            continue;
        }
        if let Some((name, ip)) = names.get(&mac) {
            c.name = name.clone();
            c.ipv4.insert(ip.clone());
        }
        let connection = c.wireless.map(|(_, w)| w).or(c.wired.map(|(_, w)| w));
        out.push(json!({
            "mac": mac,
            "name": c.name,
            "ipv4": c.ipv4,
            "ipv6": c.ipv6,
            "network": c.network,
            "connection": connection,
            "last_seen": c.last_seen,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An AP (its wired client on lan1, two stations) behind a router (which sees everything
    /// behind the AP on its lan2, and serves the leases).
    fn reports() -> (Value, Value, Value, Value) {
        let ap_caps = json!({ "macaddr": { "lan": "00:00:5e:00:53:a0" } });
        let ap = json!({ "interfaces": [{
            "name": "lan",
            "ssids": [{ "ssid": "Home", "band": ["5G"], "iface": "wl1-ap0", "bssid": "00:00:5e:00:53:a1",
                "associations": [
                    { "station": "00:00:5e:00:53:21", "rssi": -40, "inactive": 1, "connected": 60,
                      "rx_rate": { "bitrate": 6000 }, "tx_rate": { "bitrate": 144400 } },
                    { "station": "00:00:5e:00:53:22", "rssi": -70, "inactive": 50 }
                ] }],
            "clients": [
                { "mac": "00:00:5e:00:53:21", "ports": ["wl1-ap0"] },
                { "mac": "00:00:5e:00:53:31", "ports": ["lan1"] }
            ]
        }]});
        let router_caps =
            json!({ "macaddr": { "lan": "00:00:5e:00:53:b0", "wan": "00:00:5e:00:53:b1" } });
        let router = json!({ "interfaces": [{
            "name": "lan",
            "ipv4": { "addresses": ["192.0.2.1/24"], "leases": [
                { "address": "192.0.2.21", "mac": "00:00:5e:00:53:21", "hostname": "phone" },
                { "address": "192.0.2.31", "mac": "00:00:5e:00:53:31", "hostname": "printer" }
            ]},
            "clients": [
                { "mac": "00:00:5e:00:53:21", "ipv4_addresses": ["192.0.2.21"], "ports": ["lan2"] },
                { "mac": "00:00:5e:00:53:31", "ipv4_addresses": ["192.0.2.31"], "ports": ["lan2"] },
                { "mac": "00:00:5e:00:53:a0", "ipv4_addresses": ["192.0.2.4"], "ports": ["lan2"] },
                { "mac": "00:00:5e:00:53:41", "ipv4_addresses": ["192.0.2.41"], "ports": ["lan3"] }
            ]
        }]});
        (ap_caps, ap, router_caps, router)
    }

    #[test]
    fn each_client_once_where_it_is() {
        let (ap_caps, ap, router_caps, router) = reports();
        let list = merge(&[
            Report {
                serial: "00005e0053a0",
                capabilities: &ap_caps,
                state: &ap,
                received: 100,
                connected: true,
            },
            Report {
                serial: "00005e0053b0",
                capabilities: &router_caps,
                state: &router,
                received: 90,
                connected: true,
            },
        ]);
        let by: BTreeMap<&str, &Value> = list
            .iter()
            .map(|c| (c["mac"].as_str().unwrap(), c))
            .collect();
        assert!(
            !by.contains_key("00:00:5e:00:53:a0"),
            "the AP isn't a client"
        );
        assert_eq!(by.len(), 4);

        let phone = by["00:00:5e:00:53:21"];
        assert_eq!(phone["name"], "phone");
        assert_eq!(phone["ipv4"], json!(["192.0.2.21"]));
        assert_eq!(phone["connection"]["type"], "wireless");
        assert_eq!(phone["connection"]["device"], "00005e0053a0");
        assert_eq!(phone["connection"]["ssid"], "Home");
        assert_eq!(phone["connection"]["band"], "5G");
        assert_eq!(phone["connection"]["signal"], -40);
        assert_eq!(phone["connection"]["tx_rate"], 144400);
        assert_eq!(phone["last_seen"], 100);

        // Seen on the AP's lan1 (one client) and the router's lan2 (three): the AP's.
        let printer = by["00:00:5e:00:53:31"];
        assert_eq!(printer["name"], "printer");
        assert_eq!(
            printer["connection"],
            json!({ "type": "wired", "device": "00005e0053a0", "port": "lan1" })
        );
        assert_eq!(printer["network"], "lan");

        // Only the router sees it: on its lan3. No lease: no name.
        let other = by["00:00:5e:00:53:41"];
        assert_eq!(other["connection"]["port"], "lan3");
        assert_eq!(other["name"], Value::Null);
        // A station with no clients entry is still listed.
        assert_eq!(by["00:00:5e:00:53:22"]["connection"]["signal"], -70);
    }

    /// The device a station two APs list is placed on, from each one's association inactive
    /// (ms) and when its state arrived.
    fn roamed(a: (u64, u64), c: (u64, u64)) -> Value {
        let caps = json!({});
        let assoc = |inactive: u64| {
            json!({ "interfaces": [{ "name": "lan", "ssids": [{ "ssid": "Home", "iface": "wl0-ap0",
            "associations": [{ "station": "00:00:5e:00:53:21", "inactive": inactive }] }] }] })
        };
        let (sa, sc) = (assoc(a.0), assoc(c.0));
        let list = merge(&[
            Report {
                serial: "00005e0053a0",
                capabilities: &caps,
                state: &sa,
                received: a.1,
                connected: true,
            },
            Report {
                serial: "00005e0053c0",
                capabilities: &caps,
                state: &sc,
                received: c.1,
                connected: true,
            },
        ]);
        list[0]["connection"]["device"].clone()
    }

    #[test]
    fn a_roaming_station_is_where_it_was_heard_last() {
        // Reported at the same time: the one inactive the shortest.
        assert_eq!(roamed((40, 1), (2, 1)), "00005e0053c0");
        // States arrive at different times. Active just now in a state of long ago loses to
        // active 5 ms before the latest state...
        assert_eq!(roamed((0, 1_000), (5, 10_000)), "00005e0053c0");
        // ...and a later state doesn't win by arriving later: active 100 ms before 9 990 s
        // is after inactive for 30 s at 10 000 s.
        assert_eq!(roamed((100, 9_990), (30_000, 10_000)), "00005e0053a0");
    }

    /// An offline device's last state lists who was on it, not who is: none of its clients,
    /// leases or associations count, while its own MACs still aren't clients.
    #[test]
    fn only_connected_devices_report_clients() {
        let (ap_caps, ap, router_caps, _) = reports();
        let router = json!({ "interfaces": [{ "name": "lan", "clients": [
            { "mac": "00:00:5e:00:53:a0", "ipv4_addresses": ["192.0.2.4"], "ports": ["lan2"] },
            { "mac": "00:00:5e:00:53:a1", "ports": ["lan2"] },
            { "mac": "00:00:5e:00:53:41", "ipv4_addresses": ["192.0.2.41"], "ports": ["lan3"] }
        ] }] });
        let offline = json!({ "interfaces": [{ "name": "lan", "ipv4": { "leases": [
            { "address": "192.0.2.41", "mac": "00:00:5e:00:53:41", "hostname": "stale" }
        ] } }] });
        let no_caps = json!({});
        let list = merge(&[
            Report {
                serial: "00005e0053a0",
                capabilities: &ap_caps,
                state: &ap,
                received: 100,
                connected: false,
            },
            Report {
                serial: "00005e0053b0",
                capabilities: &router_caps,
                state: &router,
                received: 200,
                connected: true,
            },
            Report {
                serial: "00005e0053c0",
                capabilities: &no_caps,
                state: &offline,
                received: 50,
                connected: false,
            },
        ]);
        let macs: Vec<&str> = list.iter().map(|c| c["mac"].as_str().unwrap()).collect();
        assert_eq!(macs, ["00:00:5e:00:53:41"], "not the AP, nor its BSSID");
        assert_eq!(list[0]["name"], Value::Null, "an offline device's lease");
        assert_eq!(list[0]["connection"]["device"], "00005e0053b0");
    }
}
