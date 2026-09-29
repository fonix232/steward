//! Who's connected to the device, for the state document:
//! - the stations on its SSIDs (`iwinfo assoclist`);
//! - the hosts on its networks: the MACs its bridges learned (`brforward`), its IPv4
//!   neighbours (`/proc/net/arp`) and host hints (`luci-rpc getHostHints`) for IPv6;
//! - where it serves DHCP, its leases.
//!
//! Left out: the device's own MACs, and the hosts behind its uplink (the bridge port its
//! default gateway is learned on), which are someone else's to report. On a device with an
//! uplink, so are neighbours no port or SSID places: the bridge hasn't learned them here.
//! Where the default route isn't on a bridge (a router's WAN), everything on its interface
//! is upstream: the ISP's gateway isn't a client (`state::interfaces`).

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A MAC a bridge learned (`/sys/class/net/<bridge>/brforward`).
#[derive(Debug, Clone, PartialEq)]
pub struct Fdb {
    pub bridge: String,
    pub mac: String,
    pub port: String,
    /// One of the bridge's own addresses.
    pub local: bool,
}

/// An IPv4 neighbour (`/proc/net/arp`).
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub ip: String,
    pub mac: String,
    pub device: String,
}

/// A DHCPv4 lease.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub ip: String,
    pub mac: String,
    pub hostname: Option<String>,
}

fn mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A bridge's `brforward`: the kernel's `struct __fdb_entry`, 16 bytes each (MAC, port
/// number low byte, is_local, ageing timer, port number high byte, padding). `ports` maps
/// port numbers to names (`brif/<port>/port_no`).
pub fn brforward(bridge: &str, data: &[u8], ports: &BTreeMap<u16, String>) -> Vec<Fdb> {
    data.chunks_exact(16)
        .filter_map(|e| {
            let number = u16::from(e[6]) | (u16::from(e[12]) << 8);
            Some(Fdb {
                bridge: bridge.to_owned(),
                mac: mac(&e[..6]),
                port: ports.get(&number)?.clone(),
                local: e[7] != 0,
            })
        })
        .collect()
}

/// `/proc/net/arp`: the complete entries.
pub fn arp(text: &str) -> Vec<Neighbour> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (ip, flags, mac, device) = (f.first()?, f.get(2)?, f.get(3)?, f.get(5)?);
            let complete = u32::from_str_radix(flags.trim_start_matches("0x"), 16).ok()? & 0x2 != 0;
            (complete && *mac != "00:00:00:00:00:00").then(|| Neighbour {
                ip: ip.to_string(),
                mac: mac.to_lowercase(),
                device: device.to_string(),
            })
        })
        .collect()
}

fn hostname(h: Option<&str>) -> Option<String> {
    h.filter(|h| !h.is_empty() && *h != "*").map(str::to_owned)
}

/// dnsmasq's lease file: `<expiry> <mac> <ip> <hostname or *> <client id>`.
pub fn dnsmasq_leases(text: &str) -> Vec<Lease> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some(Lease {
                mac: f.get(1)?.to_lowercase(),
                ip: f.get(2)?.to_string(),
                hostname: hostname(f.get(3).copied()),
            })
        })
        .filter(|l| l.ip.contains('.'))
        .collect()
}

/// `luci-rpc getDHCPLeases`: dnsmasq's and odhcpd's DHCPv4 leases.
pub fn luci_leases(answer: &Value) -> Vec<Lease> {
    answer["dhcp_leases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(Lease {
                ip: l["ipaddr"].as_str()?.to_owned(),
                mac: l["macaddr"].as_str()?.to_lowercase(),
                hostname: hostname(l["hostname"].as_str()),
            })
        })
        .collect()
}

/// One direction's rate, from iwinfo's `rx` or `tx`.
fn rate(r: &Value) -> Value {
    json!({
        "bitrate": r["rate"], "mcs": r["mcs"], "nss": r["nss"], "chwidth": r["mhz"],
        "sgi": r["short_gi"], "vht": r["vht"], "he": r["he"],
    })
}

/// The stations on an SSID (TIP's `interface.ssid.association`), from `iwinfo assoclist`.
pub fn associations(results: &Value, bssid: &Value) -> Vec<Value> {
    results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(json!({
                "station": s["mac"].as_str()?.to_lowercase(),
                "bssid": bssid,
                "rssi": s["signal"],
                "connected": s["connected_time"],
                "inactive": s["inactive"],
                "rx_bytes": s["rx"]["bytes"], "rx_packets": s["rx"]["packets"],
                "tx_bytes": s["tx"]["bytes"], "tx_packets": s["tx"]["packets"],
                "tx_retries": s["tx"]["retries"], "tx_failed": s["tx"]["failed"],
                "rx_rate": rate(&s["rx"]), "tx_rate": rate(&s["tx"]),
            }))
        })
        .collect()
}

/// What the device sees on its networks.
pub struct Seen<'a> {
    fdb: &'a [Fdb],
    arp: &'a [Neighbour],
    /// IPv6 addresses by MAC (lower case), from the host hints.
    ipv6: BTreeMap<String, Vec<String>>,
    /// The device's own MACs.
    own: BTreeSet<String>,
    /// (bridge, port) its default gateway is learned on.
    uplinks: BTreeSet<(String, String)>,
}

#[derive(Default)]
struct Client {
    ipv4: BTreeSet<String>,
    ipv6: BTreeSet<String>,
    ports: BTreeSet<String>,
}

impl<'a> Seen<'a> {
    pub fn new(
        fdb: &'a [Fdb],
        arp: &'a [Neighbour],
        hints: &Value,
        gateway: Option<&str>,
        mut own: BTreeSet<String>,
    ) -> Seen<'a> {
        own.extend(fdb.iter().filter(|f| f.local).map(|f| f.mac.clone()));
        // luci-rpc keys its host hints by upper-case MAC.
        let ipv6 = hints
            .as_object()
            .into_iter()
            .flatten()
            .map(|(mac, h)| {
                let v6 = h["ip6addrs"].as_array().into_iter().flatten();
                let v6 = v6.filter_map(Value::as_str).map(str::to_owned).collect();
                (mac.to_lowercase(), v6)
            })
            .collect();
        let gateway_macs: BTreeSet<&str> = arp
            .iter()
            .filter(|n| Some(n.ip.as_str()) == gateway)
            .map(|n| n.mac.as_str())
            .collect();
        let uplinks = fdb
            .iter()
            .filter(|f| !f.local && gateway_macs.contains(f.mac.as_str()))
            .map(|f| (f.bridge.clone(), f.port.clone()))
            .collect();
        Seen {
            fdb,
            arp,
            ipv6,
            own,
            uplinks,
        }
    }

    /// The bridge an L3 device is on (`br-lan`, `br-lan.12`).
    pub fn bridge_of(&self, l3: &str) -> Option<&str> {
        self.fdb
            .iter()
            .map(|f| f.bridge.as_str())
            .find(|b| l3 == *b || l3.strip_prefix(*b).is_some_and(|r| r.starts_with('.')))
    }

    /// The clients on the logical interface whose L3 device is `l3`: its IPv4 neighbours,
    /// the `stations` (MAC, interface) of its SSIDs, and with `fdb_only`, the MACs its
    /// bridge learned on wired ports (not in `wireless`) that nothing else places.
    pub fn clients(
        &self,
        l3: &str,
        stations: &[(String, String)],
        fdb_only: bool,
        wireless: &BTreeSet<String>,
    ) -> Vec<Value> {
        let bridge = self.bridge_of(l3);
        let mut found: BTreeMap<String, Client> = BTreeMap::new();
        for n in self.arp.iter().filter(|n| n.device == l3) {
            found
                .entry(n.mac.clone())
                .or_default()
                .ipv4
                .insert(n.ip.clone());
        }
        for (mac, ifname) in stations {
            found
                .entry(mac.clone())
                .or_default()
                .ports
                .insert(ifname.clone());
        }
        let learned = self
            .fdb
            .iter()
            .filter(|f| !f.local && Some(f.bridge.as_str()) == bridge);
        for f in learned.clone() {
            if fdb_only && !wireless.contains(&f.port) {
                found.entry(f.mac.clone()).or_default();
            }
        }
        let station: BTreeSet<&str> = stations.iter().map(|(m, _)| m.as_str()).collect();
        let mut out = vec![];
        for (mac, mut c) in found {
            if self.own.contains(&mac) {
                continue;
            }
            let ports: Vec<&Fdb> = learned.clone().filter(|f| f.mac == mac).collect();
            let behind_uplink = ports
                .iter()
                .any(|f| self.uplinks.contains(&(f.bridge.clone(), f.port.clone())));
            let is_station = station.contains(mac.as_str());
            if behind_uplink && !is_station {
                continue;
            }
            // A neighbour no port or SSID places, on a device with an uplink (an AP): a host
            // it talks to elsewhere on the network (the router, a monitoring server), not a
            // client. On the router, which has no uplink on its bridges, it's on its LAN.
            if ports.is_empty() && !is_station && !self.uplinks.is_empty() {
                continue;
            }
            c.ports.extend(ports.iter().map(|f| f.port.clone()));
            c.ipv6
                .extend(self.ipv6.get(&mac).into_iter().flatten().cloned());
            out.push(json!({
                "mac": mac,
                "ipv4_addresses": (!c.ipv4.is_empty()).then_some(&c.ipv4),
                "ipv6_addresses": (!c.ipv6.is_empty()).then_some(&c.ipv6),
                "ports": (!c.ports.is_empty()).then_some(&c.ports),
            }));
        }
        out
    }
}

/// Whether `ip` is in `address/prefix`.
pub fn in_subnet(ip: &str, address: &str, prefix: u32) -> bool {
    let parse = |s: &str| s.parse::<std::net::Ipv4Addr>().ok().map(u32::from);
    let (Some(ip), Some(net)) = (parse(ip), parse(address)) else {
        return false;
    };
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.min(32))
    };
    ip & mask == net & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mac: [u8; 6], port: u16, local: bool) -> Vec<u8> {
        let mut e = mac.to_vec();
        e.push(port as u8);
        e.push(local as u8);
        e.extend([0, 0, 0, 0]); // ageing timer
        e.push((port >> 8) as u8);
        e.extend([0, 0, 0]);
        e
    }

    #[test]
    fn brforward_entries_name_their_ports() {
        let ports = BTreeMap::from([(5, "lan4".to_string()), (0x10c, "wl1-ap0".to_string())]);
        let mut data = entry([0, 0, 0x5e, 0, 0x53, 1], 5, false);
        data.extend(entry([0, 0, 0x5e, 0, 0x53, 2], 0x10c, true));
        data.extend(entry([0, 0, 0x5e, 0, 0x53, 3], 7, false)); // no such port
        let fdb = brforward("br-lan", &data, &ports);
        assert_eq!(fdb.len(), 2);
        assert_eq!(fdb[0].mac, "00:00:5e:00:53:01");
        assert_eq!((fdb[0].port.as_str(), fdb[0].local), ("lan4", false));
        assert_eq!((fdb[1].port.as_str(), fdb[1].local), ("wl1-ap0", true));
    }

    #[test]
    fn arp_keeps_complete_entries() {
        let text = "IP address       HW type     Flags       HW address            Mask     Device
192.0.2.1        0x1         0x2         00:00:5E:00:53:01     *        br-lan.1
192.0.2.9        0x1         0x0         00:00:00:00:00:00     *        br-lan.1
198.51.100.7     0x1         0x6         00:00:5e:00:53:07     *        br-lan.12
";
        let n = arp(text);
        assert_eq!(n.len(), 2);
        assert_eq!(
            n[0],
            Neighbour {
                ip: "192.0.2.1".into(),
                mac: "00:00:5e:00:53:01".into(),
                device: "br-lan.1".into()
            }
        );
        assert_eq!(n[1].device, "br-lan.12");
    }

    #[test]
    fn leases_from_either_source() {
        let file = "1790000000 00:00:5e:00:53:0a 192.0.2.50 laptop 01:00:00:5e:00:53:0a\n1790000000 00:00:5e:00:53:0b 192.0.2.51 * *\n1790000000 00:00:5e:00:53:0c 2001:db8::5 phone *\n";
        let l = dnsmasq_leases(file);
        assert_eq!(l.len(), 2, "IPv6 left out");
        assert_eq!(
            (l[0].hostname.as_deref(), l[1].hostname.as_deref()),
            (Some("laptop"), None)
        );
        let answer = json!({ "dhcp_leases": [
            { "expires": 3000, "hostname": "printer", "macaddr": "00:00:5E:00:53:0D", "ipaddr": "192.0.2.52" },
            { "expires": 3000, "macaddr": "00:00:5e:00:53:0e", "ipaddr": "192.0.2.53" }
        ], "dhcp6_leases": [] });
        let l = luci_leases(&answer);
        assert_eq!(
            l[0],
            Lease {
                ip: "192.0.2.52".into(),
                mac: "00:00:5e:00:53:0d".into(),
                hostname: Some("printer".into())
            }
        );
        assert_eq!(l[1].hostname, None);
    }

    #[test]
    fn subnets() {
        assert!(in_subnet("192.0.2.50", "192.0.2.1", 24));
        assert!(!in_subnet("198.51.100.5", "192.0.2.1", 24));
        assert!(in_subnet("192.0.3.9", "192.0.2.1", 23));
        assert!(in_subnet("203.0.113.1", "192.0.2.1", 0));
    }

    #[test]
    fn stations_carry_signal_and_rates() {
        let results = json!([{ "mac": "00:00:5E:00:53:21", "signal": -39, "connected_time": 220844, "inactive": 10,
            "rx": { "bytes": 5, "packets": 1, "rate": 6000, "mhz": 20, "ht": false, "vht": false, "he": false },
            "tx": { "bytes": 9, "packets": 2, "retries": 3, "failed": 1, "rate": 144400, "mcs": 15, "mhz": 20, "short_gi": true, "he": false } }]);
        let a = associations(&results, &json!("00:00:5e:00:53:02"));
        assert_eq!(a[0]["station"], "00:00:5e:00:53:21");
        assert_eq!(a[0]["rssi"], -39);
        assert_eq!(a[0]["tx_rate"]["bitrate"], 144400);
        assert_eq!(a[0]["tx_rate"]["mcs"], 15);
        assert_eq!(a[0]["tx_retries"], 3);
        assert_eq!(a[0]["bssid"], "00:00:5e:00:53:02");
    }

    #[test]
    fn clients_behind_the_uplink_and_the_devices_own_are_left_out() {
        let f = |mac: &str, port: &str, local: bool| Fdb {
            bridge: "br-lan".into(),
            mac: mac.into(),
            port: port.into(),
            local,
        };
        let fdb = vec![
            f("00:00:5e:00:53:01", "lan4", false), // the gateway, on the uplink
            f("00:00:5e:00:53:30", "lan4", false), // a host elsewhere on the LAN
            f("00:00:5e:00:53:31", "lan1", false), // wired, on this device
            f("00:00:5e:00:53:21", "wl1-ap0", false), // a station
            f("00:00:5e:00:53:ff", "lan1", true),  // the bridge's own
        ];
        let arp = vec![
            Neighbour {
                ip: "192.0.2.1".into(),
                mac: "00:00:5e:00:53:01".into(),
                device: "br-lan.1".into(),
            },
            Neighbour {
                ip: "192.0.2.31".into(),
                mac: "00:00:5e:00:53:31".into(),
                device: "br-lan.1".into(),
            },
            // Talked to, but not learned on any port: elsewhere.
            Neighbour {
                ip: "192.0.2.60".into(),
                mac: "00:00:5e:00:53:60".into(),
                device: "br-lan.1".into(),
            },
        ];
        // luci-rpc keys host hints in upper case.
        let hints = json!({ "00:00:5E:00:53:31": { "ipaddrs": ["192.0.2.31"], "ip6addrs": ["2001:db8::31"] } });
        let own = BTreeSet::from(["00:00:5e:00:53:fe".to_string()]);
        let seen = Seen::new(&fdb, &arp, &hints, Some("192.0.2.1"), own);
        assert_eq!(seen.bridge_of("br-lan.1"), Some("br-lan"));
        assert_eq!(seen.bridge_of("br-lanx"), None);
        let wireless = BTreeSet::from(["wl1-ap0".to_string()]);
        let stations = vec![("00:00:5e:00:53:21".to_string(), "wl1-ap0".to_string())];
        let c = seen.clients("br-lan.1", &stations, true, &wireless);
        let macs: Vec<&str> = c.iter().map(|c| c["mac"].as_str().unwrap()).collect();
        assert_eq!(macs, ["00:00:5e:00:53:21", "00:00:5e:00:53:31"]);
        assert_eq!(c[1]["ipv4_addresses"], json!(["192.0.2.31"]));
        assert_eq!(c[1]["ipv6_addresses"], json!(["2001:db8::31"]));
        assert_eq!(c[1]["ports"], json!(["lan1"]));
        assert_eq!(c[0]["ports"], json!(["wl1-ap0"]));
        // Another network on the same bridge: its own neighbours only, no FDB-only MACs.
        assert!(seen.clients("br-lan.12", &[], false, &wireless).is_empty());

        // The router: its gateway isn't on a bridge, so nothing is behind an uplink, and a
        // neighbour the bridge hasn't learned (yet) is still on its LAN.
        let router = Seen::new(
            &fdb[2..4],
            &arp[1..],
            &hints,
            Some("203.0.113.1"),
            BTreeSet::new(),
        );
        let macs: Vec<String> = router
            .clients("br-lan.1", &[], true, &wireless)
            .iter()
            .map(|c| c["mac"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(macs, ["00:00:5e:00:53:31", "00:00:5e:00:53:60"]);
    }
}
