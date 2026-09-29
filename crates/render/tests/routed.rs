use serde_json::{Map, Value, json};
use steward_render::{
    Current, MARKER, Network, Op, Plan, Ports, Sections, Wireless, dnsmasq_running,
    firewall_active, render,
};

/// A router's network config: br-lan filtering VLANs, and a VLAN (30) Steward made before.
fn network() -> Network {
    let answer = json!({ "values": {
        "br": { ".type": "device", "type": "bridge", "name": "br-lan", "ports": ["lan1", "lan2"],
                "vlan_filtering": "1" },
        "lan": { ".type": "interface", "device": "br-lan.1", "proto": "static" },
        "stw_bv30": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "30", "ports": ["lan1:t"], MARKER: "1" },
        "stw_vlan30": { ".type": "interface", "device": "br-lan.30", "proto": "none", MARKER: "1" },
    }});
    Network::from_uci(answer.as_object().unwrap())
}

/// A config whose service runs (dnsmasq, fw4): what the agent finds out from procd.
fn running(s: Sections) -> Sections {
    Sections { running: true, ..s }
}

/// dnsmasq's config: OpenWrt's own sections, and a reservation Steward made before. dnsmasq
/// runs.
fn dhcp() -> Sections {
    running(dhcp_config())
}

fn dhcp_config() -> Sections {
    let answer = json!({ "values": {
        "cfg01": { ".type": "dnsmasq", "domain": "lan" },
        "lan": { ".type": "dhcp", "interface": "lan", "start": "100", "limit": "150", "dhcpv4": "server" },
        "stw_host30_0": { ".type": "host", "mac": "00:00:5e:00:53:99", "ip": "198.51.100.9", MARKER: "1" },
        "stw_not_mine": { ".type": "host", "mac": "00:00:5e:00:53:98" },
    }});
    Sections::from_uci(answer.as_object().unwrap(), true)
}

/// fw4's config: lan and wan zones, and a zone Steward made before (no marker in firewall).
/// fw4 is active.
fn firewall() -> Sections {
    running(firewall_config())
}

fn firewall_config() -> Sections {
    let answer = json!({ "values": {
        "cfg01": { ".type": "zone", "name": "lan", "network": ["lan"] },
        "cfg02": { ".type": "zone", "name": "wan", "network": ["wan"] },
        "stw_zone30": { ".type": "zone", "name": "stw30", "network": ["stw_vlan30"] },
    }});
    Sections::from_uci(answer.as_object().unwrap(), false)
}

fn ports() -> Ports {
    Ports {
        lan: vec!["lan1".into(), "lan2".into()],
        wan: vec!["wan".into()],
    }
}

fn plan(config: Value, dhcp: Option<&Sections>, firewall: Option<&Sections>) -> Plan {
    plan_on(&network(), config, dhcp, firewall)
}

fn plan_on(
    network: &Network,
    config: Value,
    dhcp: Option<&Sections>,
    firewall: Option<&Sections>,
) -> Plan {
    render(
        &config,
        &Current {
            wireless: &Wireless::default(),
            network,
            ports: &ports(),
            poe: None,
            dhcp,
            firewall,
        },
    )
}

fn routed(ipv4: Value) -> Value {
    json!({ "interfaces": [routed_at(40, ipv4)] })
}

fn routed_at(vid: u64, ipv4: Value) -> Value {
    json!({ "name": format!("N{vid}"), "role": "downstream", "vlan": { "id": vid },
            "ethernet": [{ "select-ports": ["LAN2"] }], "ipv4": ipv4 })
}

fn find<'a>(plan: &'a Plan, config: &str, section: &str) -> Option<&'a Map<String, Value>> {
    plan.ops.iter().find_map(|op| match op {
        Op::Add {
            config: c,
            name,
            values,
            ..
        } if c == config && name == section => Some(values),
        Op::Set {
            config: c,
            section: s,
            values,
        } if c == config && s == section => Some(values),
        _ => None,
    })
}

fn deleted(plan: &Plan) -> Vec<(String, String)> {
    plan.ops
        .iter()
        .filter_map(|op| match op {
            Op::Delete { config, section } => Some((config.clone(), section.clone())),
            _ => None,
        })
        .collect()
}

fn reasons(plan: &Plan) -> String {
    plan.rejected
        .iter()
        .map(|r| format!("{} {} ", r.parameter, r.reason))
        .collect()
}

fn office() -> Value {
    routed(json!({
        "addressing": "static", "subnet": "198.51.100.1/24", "use-dns": ["198.51.100.1"],
        "dhcp": { "lease-first": 100, "lease-count": 50, "lease-time": "12h",
                  "use-dns": ["198.51.100.1", "192.0.2.53"] },
        "dhcp-leases": [
            { "macaddr": "00:00:5E:00:53:01", "static-lease-offset": 10, "lease-time": "1d" },
            { "macaddr": "00:00:5e:00:53:02", "static-lease-offset": 11 }
        ]
    }))
}

#[test]
fn a_routed_network_gets_its_address_pool_and_reservations() {
    let (d, f) = (dhcp(), firewall());
    let plan = plan(office(), Some(&d), Some(&f));
    let iface = find(&plan, "network", "stw_vlan40").unwrap();
    assert_eq!(
        (
            &iface["proto"],
            &iface["ipaddr"],
            &iface["dns"],
            &iface["device"]
        ),
        (
            &json!("static"),
            &json!("198.51.100.1/24"),
            &json!(["198.51.100.1"]),
            &json!("br-lan.40")
        )
    );
    let pool = find(&plan, "dhcp", "stw_vlan40").unwrap();
    assert_eq!(pool["interface"], "stw_vlan40");
    assert_eq!(
        pool["dhcpv4"], "server",
        "current dnsmasq serves nothing without it"
    );
    assert_eq!(
        (&pool["start"], &pool["limit"], &pool["leasetime"]),
        (&json!("100"), &json!("50"), &json!("12h"))
    );
    assert_eq!(pool["dhcp_option"], json!(["6,198.51.100.1,192.0.2.53"]));
    assert_eq!(pool[MARKER], "1");
    let host = find(&plan, "dhcp", "stw_host40_0").unwrap();
    assert_eq!(
        (&host["mac"], &host["ip"], &host["leasetime"]),
        (
            &json!("00:00:5e:00:53:01"),
            &json!("198.51.100.10"),
            &json!("1d")
        )
    );
    assert_eq!(
        find(&plan, "dhcp", "stw_host40_1").unwrap()["ip"],
        "198.51.100.11"
    );
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

#[test]
fn a_routed_network_gets_a_zone_that_lets_dhcp_and_dns_in() {
    let (d, f) = (dhcp(), firewall());
    let plan = plan(office(), Some(&d), Some(&f));
    let zone = find(&plan, "firewall", "stw_zone40").unwrap();
    assert_eq!(
        (
            &zone["name"],
            &zone["network"],
            &zone["input"],
            &zone["output"],
            &zone["forward"]
        ),
        (
            &json!("stw40"),
            &json!(["stw_vlan40"]),
            &json!("REJECT"),
            &json!("ACCEPT"),
            &json!("REJECT")
        )
    );
    assert!(
        !zone.contains_key(MARKER),
        "fw4 warns about options it doesn't know"
    );
    let dhcp_rule = find(&plan, "firewall", "stw_dhcp40").unwrap();
    assert_eq!(
        (
            &dhcp_rule["src"],
            &dhcp_rule["dest_port"],
            &dhcp_rule["target"]
        ),
        (&json!("stw40"), &json!("67"), &json!("ACCEPT"))
    );
    assert_eq!(
        find(&plan, "firewall", "stw_dns40").unwrap()["proto"],
        json!(["tcp", "udp"])
    );
    let fwd = find(&plan, "firewall", "stw_fwd40").unwrap();
    assert_eq!(
        (&fwd["src"], &fwd["dest"]),
        (&json!("stw40"), &json!("wan"))
    );
    // No wan zone (an AP): no forwarding.
    let no_wan = running(Sections::default());
    let plan = plan_with(office(), &d, &no_wan);
    assert!(find(&plan, "firewall", "stw_fwd40").is_none());
    assert!(find(&plan, "firewall", "stw_zone40").is_some());
}

fn plan_with(config: Value, d: &Sections, f: &Sections) -> Plan {
    plan(config, Some(d), Some(f))
}

#[test]
fn what_the_configuration_no_longer_wants_goes() {
    let (d, f) = (dhcp(), firewall());
    // VLAN 30's network, zone and reservation are Steward's; the configuration drops them.
    let plan = plan(office(), Some(&d), Some(&f));
    let gone = deleted(&plan);
    for want in [
        ("network", "stw_vlan30"),
        ("dhcp", "stw_host30_0"),
        ("firewall", "stw_zone30"),
    ] {
        assert!(
            gone.contains(&(want.0.into(), want.1.into())),
            "{want:?} in {gone:?}"
        );
    }
    // A section named like Steward's without the marker isn't Steward's in dhcp.
    assert!(!gone.iter().any(|(_, s)| s == "stw_not_mine"));
    assert!(!gone.iter().any(|(_, s)| s == "lan" || s == "cfg01"));
}

#[test]
fn dynamic_and_no_addressing() {
    let (d, f) = (dhcp(), firewall());
    let p = plan(
        routed(json!({ "addressing": "dynamic", "dhcp": {} })),
        Some(&d),
        Some(&f),
    );
    assert_eq!(find(&p, "network", "stw_vlan40").unwrap()["proto"], "dhcp");
    assert!(reasons(&p).contains("DHCP serving needs static addressing"));
    assert!(
        find(&p, "firewall", "stw_zone40").is_none(),
        "only routed networks get zones"
    );
    let p = plan(routed(json!({ "addressing": "none" })), Some(&d), Some(&f));
    assert_eq!(find(&p, "network", "stw_vlan40").unwrap()["proto"], "none");
}

#[test]
fn what_doesnt_fit_is_refused() {
    let (d, f) = (dhcp(), firewall());
    let cases = [
        (
            json!({ "addressing": "static", "subnet": "auto/24" }),
            "automatic subnets",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.0/24" }),
            "a subnet is the device's address",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": { "lease-first": 200, "lease-count": 100 } }),
            "doesn't fit",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.120/24", "dhcp": {} }),
            "the device's own address",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": { "lease-time": "soon" } }),
            "a lease time",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                 "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 1 }] }),
            "the device's own address",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                 "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 300 }] }),
            "lands inside the subnet",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                 "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 },
                                 { "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 11 }] }),
            "another reservation",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                 "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10, "publish-hostname": false }] }),
            "keeping one out",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "port-forward": [{}] }),
            "port forwards",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "disallow-upstream-subnet": ["203.0.113.0/24"] }),
            "the firewall's",
        ),
    ];
    for (ipv4, want) in cases {
        let plan = plan(routed(ipv4), Some(&d), Some(&f));
        assert!(reasons(&plan).contains(want), "{want}: {}", reasons(&plan));
    }
    // An upstream network isn't addressed; the device's own lan keeps its addressing.
    let wan = json!({ "interfaces": [{ "name": "WAN2", "role": "upstream", "vlan": { "id": 40 },
                                       "ipv4": { "addressing": "dynamic" } }] });
    assert!(reasons(&plan(wan, Some(&d), Some(&f))).contains("upstream (WAN)"));
    let lan = json!({ "interfaces": [{ "name": "LAN", "ipv4": { "addressing": "static", "subnet": "192.0.2.1/24" } }] });
    assert!(reasons(&plan(lan, Some(&d), Some(&f))).contains("lan is the device's own network"));
    // No dnsmasq: no DHCP, and no DNS rule.
    let plan = plan(office(), None, Some(&f));
    assert!(reasons(&plan).contains("dnsmasq isn't running on this device"));
    assert!(find(&plan, "firewall", "stw_dns40").is_none());
}

#[test]
fn dns_records_become_dnsmasq_sections() {
    let d = dhcp();
    let config = json!({ "dns-records": [
        { "name": "nas.home.arpa", "type": "A", "value": "198.51.100.20" },
        { "name": "nas.home.arpa", "type": "AAAA", "value": "2001:db8::20" },
        { "name": "files.home.arpa", "type": "CNAME", "value": "nas.home.arpa" },
        { "name": "bad name", "type": "A", "value": "198.51.100.21" },
        { "name": "x.home.arpa", "type": "A", "value": "not-an-address" },
        { "name": "y.home.arpa", "type": "MX", "value": "mail.home.arpa" }
    ]});
    let plan = plan(config.clone(), Some(&d), None);
    assert_eq!(
        (
            &find(&plan, "dhcp", "stw_dns0").unwrap()["name"],
            &find(&plan, "dhcp", "stw_dns0").unwrap()["ip"]
        ),
        (&json!("nas.home.arpa"), &json!("198.51.100.20"))
    );
    assert_eq!(
        find(&plan, "dhcp", "stw_dns1").unwrap()["ip"],
        "2001:db8::20"
    );
    let cname = find(&plan, "dhcp", "stw_cname0").unwrap();
    assert_eq!(
        (&cname["cname"], &cname["target"]),
        (&json!("files.home.arpa"), &json!("nas.home.arpa"))
    );
    let r = reasons(&plan);
    for want in [
        "dns-records/3",
        "not a value for a A record",
        "A, AAAA or CNAME",
    ] {
        assert!(r.contains(want), "{want}: {r}");
    }
    assert!(reasons(&plan_nodhcp(config)).contains("dnsmasq isn't running on this device"));
}

fn plan_nodhcp(config: Value) -> Plan {
    plan(config, None, None)
}

/// Every option written exists where it's read: netifd's interface and static-protocol
/// options, its dhcp protocol's, dnsmasq's init script, fw4 (tests/routed-schema.json).
#[test]
fn every_routed_option_exists_where_its_read() {
    let schema: Value = serde_json::from_str(include_str!("routed-schema.json")).unwrap();
    let (d, f) = (dhcp(), firewall());
    let mut config = office();
    config["interfaces"].as_array_mut().unwrap().push(routed_at(
        41,
        json!({ "addressing": "dynamic", "send-hostname": false }),
    ));
    config["dns-records"] = json!([
        { "name": "nas.home.arpa", "type": "A", "value": "198.51.100.20" },
        { "name": "files.home.arpa", "type": "CNAME", "value": "nas.home.arpa" }
    ]);
    let plan = plan(config, Some(&d), Some(&f));
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
    assert_eq!(
        find(&plan, "network", "stw_vlan41").unwrap()["hostname"],
        "*"
    );
    for op in &plan.ops {
        let (config, kind, values) = match op {
            Op::Add {
                config,
                kind,
                values,
                ..
            } => (config.as_str(), kind.clone(), values),
            Op::Set {
                config,
                section,
                values,
            } => {
                let kind = match (config.as_str(), section.as_str()) {
                    ("network", _) => "interface",
                    ("dhcp", s) if s.starts_with("stw_host") => "host",
                    ("dhcp", _) => "dhcp",
                    _ => continue,
                };
                (config.as_str(), kind.to_string(), values)
            }
            _ => continue,
        };
        if config == "network" && kind != "interface" {
            continue;
        }
        let mut known = schema[config][&kind]
            .as_array()
            .unwrap_or_else(|| panic!("{config}.{kind}"))
            .clone();
        // A DHCP client's interface takes the dhcp protocol's options too.
        if config == "network" && values["proto"] == "dhcp" {
            known.extend(schema["network"]["proto-dhcp"].as_array().unwrap().clone());
        }
        for option in values.keys().filter(|k| k.as_str() != MARKER) {
            assert!(
                known.iter().any(|o| o == option),
                "{config}.{kind}.{option}"
            );
        }
    }
}

fn cname(name: &str, value: &str) -> Value {
    json!({ "name": name, "type": "CNAME", "value": value })
}

fn cnames_written(plan: &Plan) -> Vec<(String, String)> {
    plan.ops
        .iter()
        .filter_map(|op| match op {
            Op::Add { kind, values, .. } if kind == "cname" => Some((
                values["cname"].as_str()?.to_owned(),
                values["target"].as_str()?.to_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// dnsmasq won't start with two CNAMEs for one alias ("duplicate CNAME", compared
/// case-insensitively) or with a loop ("CNAME loop involving …"): DHCP and DNS would go down
/// with it. `dnsmasq --test` on bifrost showed both.
#[test]
fn cnames_dnsmasq_would_die_on_are_refused() {
    let d = dhcp();
    let records = |r: Value| plan(json!({ "dns-records": r }), Some(&d), None);
    for r in [
        json!([
            cname("www.home.arpa", "a.home.arpa"),
            cname("www.home.arpa", "b.home.arpa")
        ]),
        json!([
            cname("www.home.arpa", "a.home.arpa"),
            cname("WWW.Home.arpa", "b.home.arpa")
        ]),
        json!([
            cname("a.home.arpa", "b.home.arpa"),
            cname("b.home.arpa", "a.home.arpa")
        ]),
        json!([cname("a.home.arpa", "A.home.arpa")]),
        json!([
            cname("a.home.arpa", "b.home.arpa"),
            cname("b.home.arpa", "c.home.arpa"),
            cname("C.home.arpa", "a.HOME.arpa")
        ]),
    ] {
        let p = records(r.clone());
        assert_eq!(p.rejected.len(), 1, "{r}: {}", reasons(&p));
        // What's written has no duplicate alias and no loop.
        let written = cnames_written(&p);
        let mut aliases: Vec<String> = written.iter().map(|(a, _)| a.to_lowercase()).collect();
        aliases.sort();
        aliases.dedup();
        assert_eq!(aliases.len(), written.len(), "{r}: {written:?}");
        assert!(written.len() < r.as_array().unwrap().len(), "{r}");
    }
    // The first stands; the one that repeats the alias or closes the loop is refused.
    let p = records(json!([
        cname("www.home.arpa", "a.home.arpa"),
        cname("WWW.home.arpa", "b.home.arpa")
    ]));
    assert_eq!(
        cnames_written(&p),
        [("www.home.arpa".into(), "a.home.arpa".into())]
    );
    assert!(reasons(&p).contains("dns-records/1"), "{}", reasons(&p));
    assert!(
        reasons(&p).contains("already has a CNAME"),
        "{}",
        reasons(&p)
    );
    let p = records(json!([
        cname("a.home.arpa", "b.home.arpa"),
        cname("b.home.arpa", "a.home.arpa")
    ]));
    assert!(reasons(&p).contains("dns-records/1"), "{}", reasons(&p));
    assert!(reasons(&p).contains("closes a loop"), "{}", reasons(&p));
    // A chain isn't a loop, and neither are two aliases for one target.
    let p = records(json!([
        cname("a.home.arpa", "b.home.arpa"),
        cname("b.home.arpa", "c.home.arpa"),
        cname("d.home.arpa", "c.home.arpa")
    ]));
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    assert_eq!(cnames_written(&p).len(), 3);
    // The device's own cname sections are dnsmasq's too.
    let own = running(Sections::from_uci(
        json!({ "values": {
            "cfg09": { ".type": "cname", "cname": "Printer.home.arpa", "target": "p1.home.arpa" },
        }})
        .as_object()
        .unwrap(),
        true,
    ));
    let p = plan(
        json!({ "dns-records": [cname("printer.home.arpa", "p2.home.arpa")] }),
        Some(&own),
        None,
    );
    assert!(
        reasons(&p).contains("already has a CNAME"),
        "{}",
        reasons(&p)
    );
    let p = plan(
        json!({ "dns-records": [cname("p1.home.arpa", "printer.home.arpa")] }),
        Some(&own),
        None,
    );
    assert!(reasons(&p).contains("closes a loop"), "{}", reasons(&p));
    assert!(cnames_written(&p).is_empty());
}

#[test]
fn reservations_without_a_pool_are_refused() {
    let (d, f) = (dhcp(), firewall());
    let lease = json!([{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 }]);
    let cases = [
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp-leases": lease }),
            "need a DHCP pool",
        ),
        (
            json!({ "addressing": "none", "dhcp-leases": lease }),
            "needs static addressing",
        ),
        (json!({ "dhcp-leases": lease }), "needs static addressing"),
        (
            json!({ "addressing": "auto", "dhcp-leases": lease }),
            "needs static addressing",
        ),
        (
            json!({ "addressing": "static", "subnet": "auto/24", "dhcp": {}, "dhcp-leases": lease }),
            "needs the network's subnet",
        ),
        (
            json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                    "dhcp": { "lease-first": 200, "lease-count": 100 }, "dhcp-leases": lease }),
            "the DHCP pool, which was refused",
        ),
    ];
    for (ipv4, want) in cases {
        let p = plan(routed(ipv4.clone()), Some(&d), Some(&f));
        let refused: Vec<String> = p
            .rejected
            .iter()
            .filter(|r| r.parameter.get("/interfaces/0/ipv4/dhcp-leases").is_some())
            .map(|r| r.reason.clone())
            .collect();
        assert!(
            refused.iter().any(|r| r.contains(want)),
            "{ipv4}: {}",
            reasons(&p)
        );
        assert!(find(&p, "dhcp", "stw_host40_0").is_none(), "{ipv4}");
    }
    // No dnsmasq: the pool and the reservations, each with the reason.
    let p = plan(
        routed(
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                       "dhcp-leases": lease }),
        ),
        None,
        Some(&f),
    );
    assert_eq!(p.rejected.len(), 2, "{}", reasons(&p));
    assert!(reasons(&p).contains("dhcp-leases"), "{}", reasons(&p));
}

#[test]
fn pool_numbers_are_whole_numbers() {
    let (d, f) = (dhcp(), firewall());
    for pool in [
        json!({ "lease-first": -5 }),
        json!({ "lease-first": "10" }),
        json!({ "lease-count": 2.5 }),
        json!({ "lease-count": null }),
        json!({ "lease-first": 10.0 }),
    ] {
        let p = plan(
            routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": pool })),
            Some(&d),
            Some(&f),
        );
        assert!(
            reasons(&p).contains("is a whole number"),
            "{pool}: {}",
            reasons(&p)
        );
        assert!(
            find(&p, "dhcp", "stw_vlan40").is_none(),
            "{pool}: not the default pool"
        );
        assert!(find(&p, "firewall", "stw_dhcp40").is_none());
    }
    // dhcp that isn't an object isn't the default pool either.
    let p = plan(
        routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": true })),
        Some(&d),
        Some(&f),
    );
    assert!(reasons(&p).contains("dhcp is an object"), "{}", reasons(&p));
    assert!(find(&p, "dhcp", "stw_vlan40").is_none());
    // Only missing numbers take dnsmasq's defaults.
    let p = plan(
        routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {} })),
        Some(&d),
        Some(&f),
    );
    let pool = find(&p, "dhcp", "stw_vlan40").unwrap();
    assert_eq!(
        (&pool["start"], &pool["limit"]),
        (&json!("100"), &json!("150"))
    );
}

#[test]
fn huge_offsets_are_refused_not_overflowed() {
    let (d, f) = (dhcp(), firewall());
    // 2^64 - 198.51.100.0 + 100: in u64 it wraps to .100, inside the subnet.
    let offset: u64 = 18_446_744_070_384_295_012;
    let p = plan(
        routed(
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
            "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": offset }] }),
        ),
        Some(&d),
        Some(&f),
    );
    assert!(find(&p, "dhcp", "stw_host40_0").is_none());
    assert!(
        reasons(&p).contains("lands inside the subnet"),
        "{}",
        reasons(&p)
    );
    for (first, count) in [(offset, 10), (100, u64::MAX), (u64::MAX, 1)] {
        let p = plan(
            routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                "dhcp": { "lease-first": first, "lease-count": count } })),
            Some(&d),
            Some(&f),
        );
        assert!(
            reasons(&p).contains("doesn't fit"),
            "{first}+{count}: {}",
            reasons(&p)
        );
        assert!(find(&p, "dhcp", "stw_vlan40").is_none());
    }
}

/// rpcd's `add` under a name that's taken merges into that section, so a section named like
/// Steward's that isn't marked would be taken over.
#[test]
fn an_unmarked_section_under_a_name_steward_wants_is_refused() {
    let answer = json!({ "values": {
        "stw_vlan40": { ".type": "dhcp", "interface": "stw_vlan40", "start": "10" },
        "stw_host41_0": { ".type": "host", "mac": "00:00:5e:00:53:77", "ip": "198.51.100.77" },
        "stw_dns0": { ".type": "domain", "name": "old.home.arpa", "ip": "198.51.100.9" },
        "stw_cname0": { ".type": "cname", "cname": "x.home.arpa", "target": "y.home.arpa" },
    }});
    let d = running(Sections::from_uci(answer.as_object().unwrap(), true));
    assert!(d.owned.is_empty());
    assert_eq!(d.sections.len(), 4);
    let f = firewall();
    // Whether the plan does anything to a dhcp section.
    let touched = |p: &Plan, name: &str| {
        p.ops.iter().any(|op| match op {
            Op::Add {
                config, name: n, ..
            }
            | Op::Set {
                config, section: n, ..
            }
            | Op::Unset {
                config, section: n, ..
            }
            | Op::Delete { config, section: n } => config == "dhcp" && n == name,
        })
    };
    // The pool's section is someone else's: no pool, no reservations, no DHCP rule.
    let p = plan(office(), Some(&d), Some(&f));
    assert!(
        reasons(&p).contains("already has a section stw_vlan40 that isn't Steward's"),
        "{}",
        reasons(&p)
    );
    assert!(!touched(&p, "stw_vlan40"));
    assert!(find(&p, "dhcp", "stw_host40_0").is_none());
    assert!(find(&p, "firewall", "stw_dhcp40").is_none());
    assert!(
        find(&p, "network", "stw_vlan40").is_some(),
        "the network stays"
    );
    // A reservation's section: that one refused, the next made.
    let vlan41 = json!({ "interfaces": [{ "name": "Lab", "vlan": { "id": 41 },
        "ethernet": [{ "select-ports": ["LAN2"] }],
        "ipv4": { "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                  "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 },
                                  { "macaddr": "00:00:5e:00:53:02", "static-lease-offset": 11 }] } }] });
    let p = plan(vlan41, Some(&d), Some(&f));
    assert_eq!(p.rejected.len(), 1, "{}", reasons(&p));
    assert!(reasons(&p).contains("dhcp-leases/0"), "{}", reasons(&p));
    assert!(!touched(&p, "stw_host41_0"));
    assert!(find(&p, "dhcp", "stw_host41_1").is_some());
    // DNS records: the A record's and the CNAME's sections are taken; the AAAA record's isn't.
    let p = plan(
        json!({ "dns-records": [
            { "name": "nas.home.arpa", "value": "198.51.100.20" },
            { "name": "nas.home.arpa", "type": "AAAA", "value": "2001:db8::20" },
            cname("files.home.arpa", "nas.home.arpa") ] }),
        Some(&d),
        None,
    );
    assert_eq!(p.rejected.len(), 2, "{}", reasons(&p));
    assert!(!touched(&p, "stw_dns0") && !touched(&p, "stw_cname0"));
    assert_eq!(find(&p, "dhcp", "stw_dns1").unwrap()["ip"], "2001:db8::20");
}

#[test]
fn keys_it_doesnt_handle_are_refused_not_dropped() {
    let (d, f) = (dhcp(), firewall());
    let cases = [
        // DHCP relaying and a DHCP client's options: nothing would do them.
        (
            routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                           "dhcp": { "relay-server": "192.0.2.1" } })),
            "relay-server isn't supported yet",
        ),
        (
            routed(json!({ "addressing": "dynamic", "vendor-class": "OpenLAN" })),
            "vendor-class isn't supported yet",
        ),
        (
            routed(json!({ "addressing": "dynamic", "request-options": [43] })),
            "request-options isn't supported yet",
        ),
        (
            routed(
                json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
                           "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01",
                                             "static-lease-offset": 10, "hostname": "printer" }] }),
            ),
            "hostname isn't supported yet",
        ),
        (
            json!({ "dns-records": [{ "name": "nas.lan", "value": "192.0.2.9", "ttl": 60 }] }),
            "ttl isn't supported yet",
        ),
        // Wrong kinds: never read as something else.
        (
            json!({ "dns-records": { "name": "nas.lan", "value": "192.0.2.9" } }),
            "dns-records is a list",
        ),
        (
            json!({ "dns-records": [{ "name": "nas.lan", "type": 1, "value": "192.0.2.9" }] }),
            "a record is A, AAAA or CNAME",
        ),
        (routed(json!("static")), "ipv4 is an object"),
        // Only static addressing takes a subnet, a gateway or DNS servers.
        (
            routed(json!({ "addressing": "dynamic", "subnet": "198.51.100.1/24" })),
            "go with static addressing",
        ),
        (
            routed(json!({ "use-dns": ["192.0.2.53"] })),
            "go with static addressing",
        ),
        // The device's own lan keeps its addressing, whatever is asked of it.
        (
            json!({ "interfaces": [{ "name": "LAN", "ipv4": { "subnet": "192.0.2.1/24" } }] }),
            "lan is the device's own network and keeps its addressing",
        ),
    ];
    for (config, want) in cases {
        let p = plan(config, Some(&d), Some(&f));
        assert!(reasons(&p).contains(want), "{want}: {}", reasons(&p));
    }
    // Asking nothing of the lan is fine.
    let p = plan(
        json!({ "interfaces": [{ "name": "LAN", "ipv4": { "addressing": "none" } }] }),
        Some(&d),
        Some(&f),
    );
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    // The handled keys raise nothing.
    let p = plan(office(), Some(&d), Some(&f));
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
}

/// procd's `service list {"name": …}` answers, recorded on bifrost (OpenWrt SNAPSHOT r36162):
/// a stopped or disabled service isn't listed; a daemon's instance says whether it runs
/// (uhttpd's running, urandom_seed's exited); a service that runs a command once and registers
/// its triggers, as the firewall's init script does (packet_steering's there), is listed with
/// no instances.
#[test]
fn procd_says_what_runs() {
    let answer = |v: Value| v.as_object().unwrap().clone();
    let stopped = answer(json!({}));
    assert!(!dnsmasq_running(&stopped) && !firewall_active(&stopped));
    let running = answer(json!({ "dnsmasq": { "instances": {
        "cfg01411c": { "running": true, "pid": 2067, "command": ["/usr/sbin/dnsmasq", "-C",
            "/var/etc/dnsmasq.conf.cfg01411c", "-k"] } } } }));
    assert!(dnsmasq_running(&running));
    let exited = answer(json!({ "dnsmasq": { "instances": {
        "cfg01411c": { "running": false, "command": ["/usr/sbin/dnsmasq"] } } } }));
    assert!(!dnsmasq_running(&exited), "an instance that exited");
    let no_instances = answer(json!({ "dnsmasq": {} }));
    assert!(
        !dnsmasq_running(&no_instances),
        "no dnsmasq section: nothing runs"
    );
    assert!(firewall_active(&answer(json!({ "firewall": {} }))));
    assert!(firewall_active(&answer(
        json!({ "firewall": { "triggers": [["config.change", ["if", ["eq", "package", "firewall"],
            ["run_script", "/etc/init.d/firewall", "reload"]]]] } })
    )));
    // Another service's answer says nothing of these.
    let other = answer(json!({ "uhttpd": { "instances": { "instance1": { "running": true } } } }));
    assert!(!dnsmasq_running(&other) && !firewall_active(&other));
}

/// A config file isn't a running service: with dnsmasq stopped nothing serves pools,
/// reservations or records, and with fw4 inactive nothing filters. They're refused with the
/// reason; the network is still addressed, and stale owned sections still go.
#[test]
fn what_a_stopped_service_would_serve_is_refused() {
    let (stopped_dhcp, f) = (dhcp_config(), firewall());
    let mut config = office();
    config["dns-records"] = json!([{ "name": "nas.home.arpa", "value": "198.51.100.20" }]);
    let p = plan(config.clone(), Some(&stopped_dhcp), Some(&f));
    let r = reasons(&p);
    for at in [
        "/interfaces/0/ipv4/dhcp\"",
        "/interfaces/0/ipv4/dhcp-leases\"",
        "/dns-records\"",
    ] {
        assert!(r.contains(at), "{at}: {r}");
    }
    assert_eq!(p.rejected.len(), 3, "{r}");
    assert!(
        p.rejected
            .iter()
            .all(|x| x.reason.starts_with("dnsmasq isn't running on this device")),
        "{r}"
    );
    assert!(!p.ops.iter().any(|op| matches!(op,
        Op::Add { config, .. } | Op::Set { config, .. } if config == "dhcp")));
    assert!(find(&p, "firewall", "stw_dhcp40").is_none());
    assert!(find(&p, "firewall", "stw_dns40").is_none());
    assert!(
        find(&p, "firewall", "stw_zone40").is_some(),
        "fw4 is active"
    );
    assert!(deleted(&p).contains(&("dhcp".into(), "stw_host30_0".into())));
    // fw4 inactive, or no firewall at all: addressed, served, but no zone, and the answer
    // says the network isn't filtered.
    let d = dhcp();
    for fw in [Some(firewall_config()), None] {
        let p = plan(office(), Some(&d), fw.as_ref());
        assert_eq!(p.rejected.len(), 1, "{}", reasons(&p));
        let refusal = &p.rejected[0];
        assert!(
            refusal
                .reason
                .contains("the firewall (fw4) isn't active on this device")
                && refusal.reason.contains("isn't filtered"),
            "{}",
            reasons(&p)
        );
        assert_eq!(
            refusal.parameter,
            json!({ "/interfaces/0/ipv4": "stw_zone40" })
        );
        let iface = find(&p, "network", "stw_vlan40").unwrap();
        assert_eq!(
            (&iface["proto"], &iface["ipaddr"]),
            (&json!("static"), &json!("198.51.100.1/24"))
        );
        assert!(find(&p, "dhcp", "stw_vlan40").is_some());
        assert!(!p.ops.iter().any(|op| matches!(op,
            Op::Add { config, .. } | Op::Set { config, .. } if config == "firewall")));
        if fw.is_some() {
            assert!(deleted(&p).contains(&("firewall".into(), "stw_zone30".into())));
        }
    }
}

/// dnsmasq refuses to start on `dhcp-option=6,` ("bad IP address"), taking DHCP and DNS down
/// on the whole device; netifd would get `dns` with nothing in it.
#[test]
fn empty_dns_server_lists_are_refused() {
    let (d, f) = (dhcp(), firewall());
    let p = plan(
        routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                       "dhcp": { "use-dns": [] } })),
        Some(&d),
        Some(&f),
    );
    let pool = find(&p, "dhcp", "stw_vlan40").unwrap();
    assert!(!pool.contains_key("dhcp_option"), "{pool:?}");
    assert!(
        reasons(&p).contains("/interfaces/0/ipv4/dhcp/use-dns"),
        "{}",
        reasons(&p)
    );
    assert!(
        reasons(&p).contains("one or more IPv4 addresses"),
        "{}",
        reasons(&p)
    );
    let p = plan(
        routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24", "use-dns": [] })),
        Some(&d),
        Some(&f),
    );
    assert!(
        !find(&p, "network", "stw_vlan40")
            .unwrap()
            .contains_key("dns")
    );
    assert!(
        reasons(&p).contains("/interfaces/0/ipv4/use-dns"),
        "{}",
        reasons(&p)
    );
}

#[test]
fn addressing_of_the_wrong_kind_is_refused() {
    let (d, f) = (dhcp(), firewall());
    for a in [
        json!(5),
        json!(true),
        json!(null),
        json!("Static"),
        json!(["static"]),
    ] {
        let p = plan(
            routed(json!({ "addressing": a.clone(), "subnet": "198.51.100.1/24", "dhcp": {} })),
            Some(&d),
            Some(&f),
        );
        let r = reasons(&p);
        assert!(
            r.contains("addressing is static, dynamic or none"),
            "{a}: {r}"
        );
        assert!(r.contains("go with static addressing"), "{a}: {r}");
        assert!(
            r.contains("DHCP serving needs static addressing"),
            "{a}: {r}"
        );
        assert_eq!(find(&p, "network", "stw_vlan40").unwrap()["proto"], "none");
        assert!(find(&p, "dhcp", "stw_vlan40").is_none());
    }
    // Missing is none, and asks nothing.
    let p = plan(routed(json!({})), Some(&d), Some(&f));
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    assert_eq!(find(&p, "network", "stw_vlan40").unwrap()["proto"], "none");
}

/// A network with the device's static addressing, as its config gives it: lan (as given) and
/// loopback (ipaddr with a netmask), guest (ipaddr in CIDR, a list) in 198.51.100.192/26, wan
/// (a DHCP client asking for .70), and VLAN 30, Steward's, with an address from before in
/// 198.51.100.128/26.
fn addressed(lan: Value) -> Network {
    let mut answer = json!({ "values": {
        "br": { ".type": "device", "type": "bridge", "name": "br-lan", "ports": ["lan1", "lan2"],
                "vlan_filtering": "1" },
        "loopback": { ".type": "interface", "device": "lo", "proto": "static",
                      "ipaddr": "127.0.0.1", "netmask": "255.0.0.0" },
        "lan": { ".type": "interface", "device": "br-lan.1", "proto": "static" },
        "guest": { ".type": "interface", "device": "br-lan.50", "proto": "static",
                   "ipaddr": ["198.51.100.193/26"] },
        "wan": { ".type": "interface", "device": "wan", "proto": "dhcp", "ipaddr": "198.51.100.70" },
        "stw_bv30": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "30", "ports": ["lan1:t"], MARKER: "1" },
        "stw_vlan30": { ".type": "interface", "device": "br-lan.30", "proto": "static",
                        "ipaddr": "198.51.100.129/26", MARKER: "1" },
    }});
    for (k, v) in lan.as_object().unwrap() {
        answer["values"]["lan"][k] = v.clone();
    }
    Network::from_uci(answer.as_object().unwrap())
}

#[test]
fn overlapping_subnets_are_refused() {
    let (d, f) = (dhcp(), firewall());
    let static_at = |vid: u64, subnet: &str| {
        routed_at(
            vid,
            json!({ "addressing": "static", "subnet": subnet, "dhcp": {},
                    "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 }] }),
        )
    };
    // Two of this configuration's: the first stands, the second isn't addressed or served.
    for second in ["198.51.100.2/24", "198.51.100.130/25", "198.51.100.1/16"] {
        let p = plan(
            json!({ "interfaces": [static_at(40, "198.51.100.1/24"), static_at(41, second)] }),
            Some(&d),
            Some(&f),
        );
        let r = reasons(&p);
        assert!(
            r.contains(&format!(
                r#"{{"/interfaces/1/ipv4/subnet":"{second}"}} it overlaps 198.51.100.1/24 on stw_vlan40"#
            )),
            "{second}: {r}"
        );
        assert!(r.contains("/interfaces/1/ipv4/dhcp\""), "{second}: {r}");
        assert!(
            r.contains("/interfaces/1/ipv4/dhcp-leases\""),
            "{second}: {r}"
        );
        assert_eq!(p.rejected.len(), 3, "{second}: {r}");
        assert_eq!(find(&p, "network", "stw_vlan41").unwrap()["proto"], "none");
        assert!(find(&p, "dhcp", "stw_vlan41").is_none());
        assert!(find(&p, "dhcp", "stw_host41_0").is_none());
        assert!(find(&p, "firewall", "stw_zone41").is_none());
        assert!(find(&p, "dhcp", "stw_host40_0").is_some());
    }
    // Next to each other isn't overlapping.
    let p = plan(
        json!({ "interfaces": [
            routed_at(40, json!({ "addressing": "static", "subnet": "198.51.100.1/25" })),
            routed_at(41, json!({ "addressing": "static", "subnet": "198.51.100.129/25" })) ] }),
        Some(&d),
        Some(&f),
    );
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    assert_eq!(
        find(&p, "network", "stw_vlan41").unwrap()["proto"],
        "static"
    );

    // The device's own addresses: lan with a netmask (dotted or a prefix), in CIDR, or alone
    // (/32), against 198.51.100.0/26; the loopback; guest's list.
    let lan_forms = [
        json!({ "ipaddr": "198.51.100.1", "netmask": "255.255.255.0" }),
        json!({ "ipaddr": "198.51.100.1", "netmask": "24" }),
        json!({ "ipaddr": "198.51.100.1/24" }),
        json!({ "ipaddr": ["192.0.2.1/24", "198.51.100.20"] }),
        json!({ "ipaddr": "192.0.2.1 198.51.100.20/30" }),
    ];
    for lan in lan_forms {
        let p = plan_on(
            &addressed(lan.clone()),
            routed(json!({ "addressing": "static", "subnet": "198.51.100.5/26" })),
            Some(&d),
            Some(&f),
        );
        let r = reasons(&p);
        assert!(
            r.contains("it overlaps") && r.contains("on lan"),
            "{lan}: {r}"
        );
        assert_eq!(
            find(&p, "network", "stw_vlan40").unwrap()["proto"],
            "none",
            "{lan}"
        );
    }
    for (subnet, whose) in [
        ("127.1.0.1/24", "127.0.0.1 (netmask 255.0.0.0) on loopback"),
        ("198.51.100.201/26", "198.51.100.193/26 on guest"),
    ] {
        let p = plan_on(
            &addressed(json!({})),
            routed(json!({ "addressing": "static", "subnet": subnet })),
            Some(&d),
            Some(&f),
        );
        assert!(reasons(&p).contains(whose), "{subnet}: {}", reasons(&p));
    }
    // A DHCP client's requested address isn't its address; Steward's own VLAN 30 is addressed
    // by this configuration (here: not at all), so its old address doesn't count.
    let p = plan_on(
        &addressed(json!({ "ipaddr": "192.0.2.1", "netmask": "255.255.255.0" })),
        json!({ "interfaces": [
            routed_at(40, json!({ "addressing": "static", "subnet": "198.51.100.65/26" })),
            routed_at(41, json!({ "addressing": "static", "subnet": "198.51.100.130/26" })) ] }),
        Some(&d),
        Some(&f),
    );
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    // An address netifd may read otherwise: refused, not guessed at. A netmask in octal (030
    // is 24 to strtoul), one that isn't contiguous, an address that doesn't parse.
    for lan in [
        json!({ "ipaddr": "198.51.100.1", "netmask": "030" }),
        json!({ "ipaddr": "198.51.100.1", "netmask": "0x18" }),
        json!({ "ipaddr": "198.51.100.1", "netmask": "255.0.255.0" }),
        json!({ "ipaddr": "198.51.100.1/24", "netmask": "garbage" }),
        json!({ "ipaddr": "198.51.100.01" }),
        json!({ "ipaddr": "198.51.100.1/33" }),
    ] {
        let p = plan_on(
            &addressed(lan.clone()),
            routed(json!({ "addressing": "static", "subnet": "192.0.2.1/24" })),
            Some(&d),
            Some(&f),
        );
        let r = reasons(&p);
        assert!(
            r.contains("lan's address") && r.contains("an overlap can't be ruled out"),
            "{lan}: {r}"
        );
        assert_eq!(find(&p, "network", "stw_vlan40").unwrap()["proto"], "none");
    }
}

#[test]
fn lease_times_dnsmasq_would_change_or_misread_are_refused() {
    let (d, f) = (dhcp(), firewall());
    let lease = json!([{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 }]);
    // A bad pool lease time refuses the pool, and so its reservations: it isn't served for
    // the default 6 hours instead.
    for t in [
        json!("soon"),
        json!("0"),
        json!("119"),
        json!("1m"),
        json!("30s"),
        json!("53w"),
        json!("366d"),
        json!("31536001"),
        json!("99999999999999999999h"),
        json!("6H"),
        json!(-5),
        json!(2.5),
    ] {
        let p = plan(
            routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                           "dhcp": { "lease-time": t.clone() }, "dhcp-leases": lease })),
            Some(&d),
            Some(&f),
        );
        let r = reasons(&p);
        assert!(r.contains("/interfaces/0/ipv4/dhcp/lease-time"), "{t}: {r}");
        assert!(r.contains("the DHCP pool, which was refused"), "{t}: {r}");
        assert!(find(&p, "dhcp", "stw_vlan40").is_none(), "{t}");
        assert!(find(&p, "dhcp", "stw_host40_0").is_none(), "{t}");
        assert!(find(&p, "firewall", "stw_dhcp40").is_none(), "{t}");
    }
    for (t, written) in [
        (json!("2m"), "2m"),
        (json!("120"), "120"),
        (json!("365d"), "365d"),
        (json!("52w"), "52w"),
        (json!("8760h"), "8760h"),
        (json!("infinite"), "infinite"),
        (json!(3600), "3600"),
    ] {
        let p = plan(
            routed(json!({ "addressing": "static", "subnet": "198.51.100.1/24",
                           "dhcp": { "lease-time": t.clone() } })),
            Some(&d),
            Some(&f),
        );
        assert!(p.rejected.is_empty(), "{t}: {}", reasons(&p));
        assert_eq!(
            find(&p, "dhcp", "stw_vlan40").unwrap()["leasetime"],
            written
        );
    }
    // A bad reservation lease time refuses that reservation: it would take the pool's.
    let p = plan(
        routed(
            json!({ "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": {},
            "dhcp-leases": [
                { "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10, "lease-time": "30s" },
                { "macaddr": "00:00:5e:00:53:02", "static-lease-offset": 11, "lease-time": "12x" },
                { "macaddr": "00:00:5e:00:53:03", "static-lease-offset": 12, "lease-time": "1d" }] }),
        ),
        Some(&d),
        Some(&f),
    );
    let r = reasons(&p);
    assert_eq!(p.rejected.len(), 2, "{r}");
    assert!(
        r.contains("/interfaces/0/ipv4/dhcp-leases/0/lease-time"),
        "{r}"
    );
    assert!(r.contains("2 minutes to a year"), "{r}");
    assert!(
        r.contains("/interfaces/0/ipv4/dhcp-leases/1/lease-time"),
        "{r}"
    );
    assert!(find(&p, "dhcp", "stw_host40_0").is_none());
    assert!(find(&p, "dhcp", "stw_host40_1").is_none());
    assert_eq!(find(&p, "dhcp", "stw_host40_2").unwrap()["leasetime"], "1d");
    assert!(find(&p, "dhcp", "stw_vlan40").is_some(), "the pool stands");
}

/// `send-hostname` (TIP's default: true): netifd's dhcp protocol sends the device's hostname
/// unless `hostname` is `*` (/lib/netifd/proto/dhcp.sh).
#[test]
fn send_hostname_is_the_dhcp_clients() {
    let (d, f) = (dhcp(), firewall());
    let iface = |ipv4: Value| {
        let p = plan(routed(ipv4.clone()), Some(&d), Some(&f));
        assert!(p.rejected.is_empty(), "{ipv4}: {}", reasons(&p));
        find(&p, "network", "stw_vlan40").unwrap().clone()
    };
    let off = iface(json!({ "addressing": "dynamic", "send-hostname": false }));
    assert_eq!(
        (&off["proto"], &off["hostname"]),
        (&json!("dhcp"), &json!("*"))
    );
    for ipv4 in [
        json!({ "addressing": "dynamic", "send-hostname": true }),
        json!({ "addressing": "dynamic" }),
    ] {
        assert!(!iface(ipv4).contains_key("hostname"));
    }
    // No DHCP client: nothing to change, and nothing refused.
    for ipv4 in [
        json!({ "addressing": "static", "subnet": "198.51.100.1/24", "send-hostname": false }),
        json!({ "addressing": "none", "send-hostname": false }),
        json!({ "send-hostname": true }),
    ] {
        assert!(!iface(ipv4).contains_key("hostname"));
    }
    // Not a boolean: refused, whatever the addressing.
    for ipv4 in [
        json!({ "addressing": "dynamic", "send-hostname": "no" }),
        json!({ "addressing": "static", "subnet": "198.51.100.1/24", "send-hostname": 0 }),
    ] {
        let p = plan(routed(ipv4.clone()), Some(&d), Some(&f));
        assert!(
            reasons(&p).contains("send-hostname is true or false"),
            "{ipv4}: {}",
            reasons(&p)
        );
        assert!(
            !find(&p, "network", "stw_vlan40")
                .unwrap()
                .contains_key("hostname")
        );
    }
    // A network that stopped hiding its name loses the option.
    let mut net = json!({ "values": {
        "br": { ".type": "device", "type": "bridge", "name": "br-lan", "ports": ["lan1", "lan2"],
                "vlan_filtering": "1" },
        "stw_bv30": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "30", "ports": ["lan2:t"], MARKER: "1" },
        "stw_vlan30": { ".type": "interface", "device": "br-lan.30", "proto": "dhcp",
                        "hostname": "*", MARKER: "1" },
    }});
    let net = Network::from_uci(net.as_object_mut().unwrap());
    let p = plan_on(
        &net,
        json!({ "interfaces": [routed_at(30, json!({ "addressing": "dynamic" }))] }),
        Some(&d),
        Some(&f),
    );
    assert!(p.ops.contains(&Op::Unset {
        config: "network".into(),
        section: "stw_vlan30".into(),
        options: vec!["hostname".into()],
    }));
    // The device's own lan: TIP's default asks nothing of it; false would.
    let lan = |ipv4: Value| {
        plan(
            json!({ "interfaces": [{ "name": "LAN", "ipv4": ipv4 }] }),
            Some(&d),
            Some(&f),
        )
    };
    let p = lan(json!({ "addressing": "none", "send-hostname": true }));
    assert!(p.rejected.is_empty(), "{}", reasons(&p));
    let p = lan(json!({ "addressing": "none", "send-hostname": false }));
    assert!(
        reasons(&p).contains("keeps its addressing"),
        "{}",
        reasons(&p)
    );
}
