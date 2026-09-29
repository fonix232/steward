use serde_json::{Map, Value, json};
use steward_render::{Current, MARKER, Network, Op, Plan, Ports, Wireless, render};

/// Shaped like bifrost's `uci get network` under openUF: a VLAN-filtering br-lan over wan and
/// lan1-4, VLAN 1 untagged everywhere (the device's lan), VLAN 12 tagged, with its interface;
/// plus a VLAN the agent made before (10) and one the new configuration drops (30).
fn network() -> Network {
    let answer = json!({ "values": {
        "loopback": { ".type": "interface", "device": "lo", "proto": "static" },
        "openuf_br": { ".type": "device", "type": "bridge", "name": "br-lan",
                       "ports": ["wan", "lan1", "lan2", "lan3", "lan4"], "vlan_filtering": "1" },
        "openuf_bv1": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "1",
                        "ports": ["wan:u*", "lan1:u*", "lan2:u*", "lan3:u*"] },
        "openuf_bv12": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "12",
                         "ports": ["wan:t", "lan1:t"] },
        "lan": { ".type": "interface", "device": "br-lan.1", "proto": "dhcp" },
        "openuf_v12": { ".type": "interface", "device": "br-lan.12", "proto": "none" },
        "stw_bv10": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "10", "ports": ["wan:t"], MARKER: "1" },
        "stw_vlan10": { ".type": "interface", "device": "br-lan.10", "proto": "none", MARKER: "1" },
        "stw_bv30": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "30", "ports": ["wan:t"], MARKER: "1" },
        "stw_vlan30": { ".type": "interface", "device": "br-lan.30", "proto": "none", MARKER: "1" },
    }});
    Network::from_uci(answer.as_object().unwrap())
}

/// Like bifrost's board.json.
fn ports() -> Ports {
    Ports::from_board(&json!({ "network": {
        "lan": { "ports": ["lan1", "lan2", "lan3", "lan4"], "protocol": "static" },
        "wan": { "device": "wan", "protocol": "dhcp" }
    }}))
}

fn wireless() -> Wireless {
    let answer = json!({ "values": {
        "radio0": { ".type": "wifi-device", "band": "2g" },
        "radio1": { ".type": "wifi-device", "band": "5g" },
    }});
    Wireless::from_uci(answer.as_object().unwrap())
}

fn plan_for(config: &Value, network: &Network) -> Plan {
    render(
        config,
        &Current {
            wireless: &wireless(),
            network,
            ports: &ports(),
            poe: None,
        },
    )
}

fn ssid(name: &str) -> Value {
    json!({ "name": name, "wifi-bands": ["5G"], "encryption": { "proto": "psk2", "key": "correct horse battery" } })
}

fn find<'a>(plan: &'a Plan, section: &str) -> Option<&'a Op> {
    plan.ops.iter().find(|op| match op {
        Op::Add { name, .. } => name == section,
        Op::Set { section: s, .. }
        | Op::Unset { section: s, .. }
        | Op::Delete { section: s, .. } => s == section,
    })
}

fn values(op: &Op) -> &Map<String, Value> {
    match op {
        Op::Add { values, .. } | Op::Set { values, .. } => values,
        Op::Unset { .. } | Op::Delete { .. } => panic!("no values"),
    }
}

fn reasons(plan: &Plan) -> Vec<&str> {
    plan.rejected.iter().map(|r| r.reason.as_str()).collect()
}

#[test]
fn an_interface_without_a_vlan_is_the_devices_lan() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "name": "LAN", "ssids": [ssid("Home")] }] }),
        &network(),
    );
    assert_eq!(values(find(&plan, "stw_0_0_5g").unwrap())["network"], "lan");
    assert!(
        plan.ops.iter().all(|op| !matches!(op, Op::Add { config, .. } | Op::Set { config, .. } if config == "network")),
        "{:?}",
        plan.ops
    );
}

#[test]
fn an_existing_vlan_is_joined_never_edited() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "name": "IoT", "vlan": { "id": 12 }, "ssids": [ssid("IoT")] }] }),
        &network(),
    );
    assert_eq!(
        values(find(&plan, "stw_0_0_5g").unwrap())["network"],
        "openuf_v12"
    );
    assert!(find(&plan, "openuf_bv12").is_none() && find(&plan, "openuf_v12").is_none());
    assert!(find(&plan, "stw_bv12").is_none() && find(&plan, "stw_vlan12").is_none());
    assert!(plan.rejected.is_empty(), "{:?}", reasons(&plan));
}

#[test]
fn a_new_vlan_gets_owned_sections_tagged_on_every_bridge_port() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "name": "Guest", "vlan": { "id": 20 }, "ssids": [ssid("Guest")] }] }),
        &network(),
    );
    let bv = find(&plan, "stw_bv20").expect("bridge-vlan");
    assert!(
        matches!(bv, Op::Add { config, kind, .. } if config == "network" && kind == "bridge-vlan")
    );
    let v = values(bv);
    assert_eq!(v["device"], "br-lan");
    assert_eq!(v["vlan"], "20");
    assert_eq!(
        v["ports"],
        json!(["lan1:t", "lan2:t", "lan3:t", "lan4:t", "wan:t"])
    );
    assert_eq!(v[MARKER], "1");
    let iface = find(&plan, "stw_vlan20").expect("interface");
    assert!(matches!(iface, Op::Add { kind, .. } if kind == "interface"));
    assert_eq!(values(iface)["device"], "br-lan.20");
    assert_eq!(values(iface)["proto"], "none");
    assert_eq!(
        values(find(&plan, "stw_0_0_5g").unwrap())["network"],
        "stw_vlan20"
    );
    // The network changes come before the SSID that joins the new network.
    let pos = |s: &str| {
        plan.ops
            .iter()
            .position(|op| {
                find(
                    &Plan {
                        ops: vec![op.clone()],
                        ..Default::default()
                    },
                    s,
                )
                .is_some()
            })
            .unwrap()
    };
    assert!(pos("stw_vlan20") < pos("stw_0_0_5g"));
}

#[test]
fn owned_vlans_are_updated_and_dropped_ones_deleted() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "name": "Cameras", "vlan": { "id": 10 },
                                  "ethernet": [{ "select-ports": ["LAN1", "LAN2"] }] }] }),
        &network(),
    );
    let bv = find(&plan, "stw_bv10").unwrap();
    assert!(matches!(bv, Op::Set { .. }));
    assert_eq!(values(bv)["ports"], json!(["lan1:t", "lan2:t"]));
    assert!(matches!(find(&plan, "stw_vlan10"), Some(Op::Set { .. })));
    assert!(
        matches!(find(&plan, "stw_bv30"), Some(Op::Delete { config, .. }) if config == "network")
    );
    assert!(matches!(find(&plan, "stw_vlan30"), Some(Op::Delete { .. })));
    for op in &plan.ops {
        if let Op::Delete { section, .. } = op {
            assert!(section.starts_with("stw_"), "deleted {section}");
        }
    }
}

#[test]
fn untagged_ports_must_be_free() {
    let config = json!({ "interfaces": [{ "name": "Printers", "vlan": { "id": 40 }, "ethernet": [
        { "select-ports": ["LAN*"], "vlan-tag": "untagged" },
        { "select-ports": ["WAN1"], "vlan-tag": "tagged" },
        { "select-ports": ["LAN9"] }
    ]}]});
    let plan = plan_for(&config, &network());
    // lan1-3 are untagged in VLAN 1; lan4 isn't in it.
    assert_eq!(
        values(find(&plan, "stw_bv40").unwrap())["ports"],
        json!(["lan4:u*", "wan:t"])
    );
    let r = reasons(&plan);
    assert_eq!(
        r.iter().filter(|r| r.contains("already untagged")).count(),
        3,
        "{r:?}"
    );
    assert!(r.iter().any(|r| r.contains("no such ports")), "{r:?}");
    // Nor twice within one configuration.
    let twice = json!({ "interfaces": [
        { "name": "A", "vlan": { "id": 41 }, "ethernet": [{ "select-ports": ["LAN4"], "vlan-tag": "untagged" }] },
        { "name": "B", "vlan": { "id": 42 }, "ethernet": [{ "select-ports": ["LAN4"], "vlan-tag": "untagged" }] }
    ]});
    let plan = plan_for(&twice, &network());
    assert_eq!(
        values(find(&plan, "stw_bv41").unwrap())["ports"],
        json!(["lan4:u*"])
    );
    assert!(
        find(&plan, "stw_bv42").is_none(),
        "no ports left for VLAN 42"
    );
}

/// Nothing in `ipv4` is dropped: at this stage every interface keeps the device's own
/// addressing, so anything `ipv4` asks for is refused, and only what asks nothing passes.
#[test]
fn any_addressing_asked_for_is_refused() {
    for ipv4 in [
        json!({ "port-forward": [] }),
        json!({ "subnet": "198.51.100.1/24" }),
        json!({ "addressing": "Static" }),
        json!({ "addressing": 5 }),
        json!({ "use-dns": ["192.0.2.53"] }),
        json!({ "gateway": "192.0.2.1" }),
        json!({ "disallow-upstream-subnet": true }),
        json!("static"),
        json!({ "anything": 1 }),
        json!({ "addressing": "none", "send-hostname": false }),
    ] {
        for vlan in [None, Some(20)] {
            let mut iface = json!({ "name": "N", "ipv4": ipv4 });
            if let Some(id) = vlan {
                iface["vlan"] = json!({ "id": id });
            }
            let plan = plan_for(&json!({ "interfaces": [iface] }), &network());
            let r = reasons(&plan);
            assert!(
                r.iter().any(|r| r.contains("routed interfaces")),
                "{ipv4} on {vlan:?}: {r:?}"
            );
        }
    }
    for ipv4 in [
        json!({}),
        json!({ "addressing": "none" }),
        json!({ "addressing": "none", "send-hostname": true }),
    ] {
        let plan = plan_for(
            &json!({ "interfaces": [{ "name": "N", "ipv4": ipv4 }] }),
            &network(),
        );
        assert!(plan.rejected.is_empty(), "{ipv4}: {:?}", reasons(&plan));
    }
}

#[test]
fn what_it_cant_do_comes_back_as_rejections() {
    let config = json!({ "interfaces": [
        { "name": "Routed", "vlan": { "id": 50 },
          "ipv4": { "addressing": "static", "subnet": "198.51.100.1/24", "dhcp": { "lease-count": 10 } },
          "ssids": [ssid("Routed")] },
        { "name": "Again", "vlan": { "id": 50 }, "ssids": [ssid("Again")] },
        { "name": "Bad", "vlan": { "id": 5000 }, "ssids": [ssid("Bad")] }
    ]});
    let plan = plan_for(&config, &network());
    let r = reasons(&plan);
    // Routed: the addressing is refused, the layer-2 network still made.
    assert!(r.iter().any(|r| r.contains("routed interfaces")), "{r:?}");
    // A DHCP client or reservations on a VLAN are addressing too, not supported in this era.
    for ipv4 in [
        json!({ "addressing": "dynamic" }),
        json!({ "dhcp-leases": [{ "macaddr": "00:00:5e:00:53:01", "static-lease-offset": 10 }] }),
    ] {
        let config = json!({ "interfaces": [{ "name": "V", "vlan": { "id": 60 }, "ipv4": ipv4 }] });
        let plan = plan_for(&config, &network());
        let r = reasons(&plan);
        assert!(r.iter().any(|r| r.contains("routed interfaces")), "{r:?}");
    }
    assert_eq!(
        values(find(&plan, "stw_0_0_5g").unwrap())["network"],
        "stw_vlan50"
    );
    assert!(
        r.iter()
            .any(|r| r.contains("another interface has this VLAN")),
        "{r:?}"
    );
    assert!(r.iter().any(|r| r.contains("1 to 4094")), "{r:?}");
    // A refused interface's SSIDs aren't put on some other network.
    assert!(find(&plan, "stw_1_0_5g").is_none() && find(&plan, "stw_2_0_5g").is_none());
    assert_eq!(
        r.iter().filter(|r| r.contains("SSIDs are too")).count(),
        2,
        "{r:?}"
    );

    // A bridge that doesn't filter VLANs isn't converted.
    let plain = Network::from_uci(
        json!({ "values": {
            "br": { ".type": "device", "type": "bridge", "name": "br-lan", "ports": ["lan1"] },
            "lan": { ".type": "interface", "device": "br-lan", "proto": "static" }
        }})
        .as_object()
        .unwrap(),
    );
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 10 }, "ssids": [ssid("X")] }] }),
        &plain,
    );
    assert!(
        reasons(&plan)
            .iter()
            .any(|r| r.contains("doesn't filter VLANs"))
    );
    assert!(plan.ops.is_empty(), "{:?}", plan.ops);
}

#[test]
fn a_bridge_with_vlans_filters_them() {
    // netifd filters VLANs on a bridge with bridge-vlan sections even without vlan_filtering.
    let n = Network::from_uci(
        json!({ "values": {
            "br": { ".type": "device", "type": "bridge", "name": "br-lan", "ports": ["lan1", "lan2"] },
            "bv": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "1", "ports": "lan1:u* lan2:u*" }
        }})
        .as_object()
        .unwrap(),
    );
    assert_eq!(
        n.bridge,
        Some(("br-lan".into(), vec!["lan1".into(), "lan2".into()]))
    );
    assert_eq!(n.vlans[&1].ports, ["lan1:u*", "lan2:u*"]);
}

/// Every option written exists in netifd: it ignores unknown options silently. The lists are
/// netifd's own attribute tables (tests/network-schema.json).
#[test]
fn every_option_exists_in_netifd() {
    let schema: Value = serde_json::from_str(include_str!("network-schema.json")).unwrap();
    let config = json!({ "interfaces": [
        { "vlan": { "id": 10 } },
        { "vlan": { "id": 20 }, "ethernet": [{ "select-ports": ["LAN4"], "vlan-tag": "untagged" }] }
    ]});
    let plan = plan_for(&config, &network());
    let mut checked = 0;
    for op in &plan.ops {
        let (kind, vals) = match op {
            Op::Add {
                config,
                kind,
                values,
                ..
            } if config == "network" => (kind.as_str(), values),
            Op::Set {
                config,
                section,
                values,
            } if config == "network" => (
                if section.starts_with("stw_bv") {
                    "bridge-vlan"
                } else {
                    "interface"
                },
                values,
            ),
            _ => continue,
        };
        for option in vals.keys().filter(|k| k.as_str() != MARKER) {
            assert!(
                schema[kind].as_array().unwrap().iter().any(|o| o == option),
                "{kind}.{option} isn't read by netifd"
            );
            checked += 1;
        }
    }
    assert!(checked >= 10, "checked {checked}");
}

#[test]
fn a_joined_vlans_ports_stay_as_they_are() {
    // VLAN 12 is openUF's, tagged on wan and lan1.
    let same = json!({ "interfaces": [{ "vlan": { "id": 12 },
        "ethernet": [{ "select-ports": ["WAN*", "LAN1"], "vlan-tag": "tagged" }] }] });
    assert!(plan_for(&same, &network()).rejected.is_empty());
    let other = json!({ "interfaces": [{ "vlan": { "id": 12 },
        "ethernet": [{ "select-ports": ["LAN2"], "vlan-tag": "tagged" }] }] });
    let plan = plan_for(&other, &network());
    let r = reasons(&plan);
    assert!(
        r.iter().any(|r| r.contains("its ports stay wan:t lan1:t")),
        "{r:?}"
    );
    assert!(find(&plan, "openuf_bv12").is_none() && find(&plan, "stw_bv12").is_none());
}

#[test]
fn a_port_off_the_bridge_is_refused_not_dropped() {
    // A router: wan isn't on br-lan.
    let router = Network::from_uci(
        json!({ "values": {
            "br": { ".type": "device", "type": "bridge", "name": "br-lan",
                    "ports": ["lan1", "lan2"], "vlan_filtering": "1" },
        }})
        .as_object()
        .unwrap(),
    );
    let config = json!({ "interfaces": [{ "vlan": { "id": 20 },
        "ethernet": [{ "select-ports": ["WAN*", "LAN1"] }] }] });
    let plan = plan_for(&config, &router);
    assert_eq!(
        values(find(&plan, "stw_bv20").unwrap())["ports"],
        json!(["lan1:t"])
    );
    let r = reasons(&plan);
    assert!(
        r.iter().any(|r| r.contains("isn't on the device's bridge")),
        "{r:?}"
    );
}

/// Whether the SSIDs of this plan's only interface got a section, and on which network.
fn ssid_network(plan: &Plan) -> Option<String> {
    find(plan, "stw_0_0_5g").map(|op| values(op)["network"].as_str().unwrap().to_owned())
}

/// Interface fields this renderer doesn't handle are refused, not dropped: an answer of 0
/// would claim them applied. The SSIDs still run where the interface is.
#[test]
fn unhandled_interface_fields_are_refused() {
    for (extra, key) in [
        (json!({ "ipv6": { "addressing": "dynamic" } }), "ipv6"),
        (json!({ "isolate-hosts": true }), "isolate-hosts"),
        (json!({ "mtu": 1500 }), "mtu"),
        (json!({ "services": ["ssh"] }), "services"),
    ] {
        let mut iface = json!({ "name": "LAN", "ssids": [ssid("Home")] });
        iface
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let plan = plan_for(&json!({ "interfaces": [iface] }), &network());
        let r = reasons(&plan);
        assert_eq!(r, [format!("{key} isn't supported yet")], "{extra}");
        assert_eq!(ssid_network(&plan).as_deref(), Some("lan"), "{extra}");
    }
    // In a VLAN and an ethernet entry too.
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 21, "pvid": 3 },
            "ethernet": [{ "select-ports": ["LAN1"], "isolate": true }] }] }),
        &network(),
    );
    let r = reasons(&plan);
    assert!(r.contains(&"pvid isn't supported yet"), "{r:?}");
    assert!(r.contains(&"isolate isn't supported yet"), "{r:?}");
    assert_eq!(
        values(find(&plan, "stw_bv21").unwrap())["ports"],
        json!(["lan1:t"])
    );
    // What's refused is shown without its secrets.
    let plan = plan_for(
        &json!({ "interfaces": [{ "broad-band": { "protocol": "pppoe",
            "user-name": "u", "password": "leaked-ppp-password" } }] }),
        &network(),
    );
    let shown = serde_json::to_string(&plan.rejected).unwrap();
    assert!(
        shown.contains("pppoe") && !shown.contains("leaked"),
        "{shown}"
    );
}

/// A VLAN this renderer can't make as asked is refused with its SSIDs, never turned into
/// something else: an 802.1ad VLAN isn't made as 802.1q, and one without an id isn't the
/// untagged lan.
#[test]
fn a_vlan_it_cant_make_as_asked_is_refused_with_its_ssids() {
    for vlan in [
        json!({ "id": 21, "proto": "802.1ad" }),
        json!({ "proto": "802.1q" }),
        json!({}),
        json!(21),
    ] {
        let plan = plan_for(
            &json!({ "interfaces": [{ "vlan": vlan.clone(), "ssids": [ssid("V")] }] }),
            &network(),
        );
        assert_eq!(ssid_network(&plan), None, "{vlan}");
        assert!(find(&plan, "stw_bv21").is_none(), "{vlan}");
        let r = reasons(&plan);
        assert!(
            r.iter()
                .any(|r| r.contains("802.1ad VLANs") || r.contains("a VLAN without an id")),
            "{vlan}: {r:?}"
        );
        assert!(
            r.iter().any(|r| r.contains("SSIDs are too")),
            "{vlan}: {r:?}"
        );
    }
    // 802.1q, the default, said out loud.
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 21, "proto": "802.1q" }, "ssids": [ssid("V")] }] }),
        &network(),
    );
    assert!(plan.rejected.is_empty(), "{:?}", reasons(&plan));
    assert_eq!(ssid_network(&plan).as_deref(), Some("stw_vlan21"));
}

/// Ports asked of an interface without a VLAN select nothing (the device's lan keeps its
/// own), so they're refused; its SSIDs stay on lan. An `ethernet` that isn't a list would read
/// as no selection, tagging a VLAN on every port: refused, with the VLAN's SSIDs.
#[test]
fn ethernet_that_selects_nothing_is_refused() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "role": "upstream",
            "ethernet": [{ "select-ports": ["WAN*"] }], "ssids": [ssid("Up")] }] }),
        &network(),
    );
    let r = reasons(&plan);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].contains("the device's own lan"), "{r:?}");
    assert_eq!(ssid_network(&plan).as_deref(), Some("lan"));
    assert!(
        plan.ops
            .iter()
            .all(|op| !matches!(op, Op::Add { config, .. } if config == "network"))
    );

    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 21 },
            "ethernet": { "select-ports": ["LAN1"] }, "ssids": [ssid("V")] }] }),
        &network(),
    );
    assert!(
        reasons(&plan).contains(&"ethernet is a list"),
        "{:?}",
        reasons(&plan)
    );
    assert!(find(&plan, "stw_bv21").is_none());
    assert_eq!(ssid_network(&plan), None);
}

/// A new VLAN 20 on lan4 (free) with one ethernet entry: its ports, and what was refused.
fn vlan20(entry: Value) -> (Option<Value>, Vec<String>) {
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 20 }, "ethernet": [entry] }] }),
        &network(),
    );
    let ports = find(&plan, "stw_bv20").map(|op| values(op)["ports"].clone());
    let r = reasons(&plan).iter().map(|r| r.to_string()).collect();
    (ports, r)
}

/// vlan-tag is TIP's `tagged`, `un-tagged` or `auto` (its default, taken as tagged), with
/// `untagged` taken too. Anything else is refused with its entry's ports, never read as
/// tagged.
#[test]
fn vlan_tag_is_read_as_tip_spells_it() {
    for (tag, want) in [
        (json!("un-tagged"), "lan4:u*"),
        (json!("untagged"), "lan4:u*"),
        (json!("tagged"), "lan4:t"),
        (json!("auto"), "lan4:t"),
    ] {
        let (ports, r) = vlan20(json!({ "select-ports": ["LAN4"], "vlan-tag": tag.clone() }));
        assert_eq!(ports, Some(json!([want])), "{tag}");
        assert!(r.is_empty(), "{tag}: {r:?}");
    }
    let (ports, r) = vlan20(json!({ "select-ports": ["LAN4"] }));
    assert_eq!((ports, r.len()), (Some(json!(["lan4:t"])), 0));
    for tag in [json!("bogus"), json!("Untagged"), json!(true), json!(0)] {
        let (ports, r) = vlan20(json!({ "select-ports": ["LAN4"], "vlan-tag": tag.clone() }));
        assert_eq!(ports, None, "{tag}: no port left for the VLAN");
        assert!(
            r.iter()
                .any(|r| r == "vlan-tag is tagged, un-tagged or auto"),
            "{tag}: {r:?}"
        );
    }
    // Another entry still gives the VLAN its ports; the refused one is listed.
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 20 }, "ethernet": [
            { "select-ports": ["LAN4"], "vlan-tag": "bogus" },
            { "select-ports": ["LAN2"], "vlan-tag": "tagged" }
        ] }] }),
        &network(),
    );
    assert_eq!(
        values(find(&plan, "stw_bv20").unwrap())["ports"],
        json!(["lan2:t"])
    );
    assert!(
        plan.rejected[0]
            .parameter
            .get("/interfaces/0/ethernet/0/vlan-tag")
            .is_some()
    );
}

/// A port selected both tagged and un-tagged in one new VLAN would be listed twice in its
/// bridge-vlan: the first selection stands, the later one is refused, naming the port.
#[test]
fn a_port_selected_both_ways_keeps_its_first_selection() {
    let ports_and_reasons = |ethernet: Value| {
        let plan = plan_for(
            &json!({ "interfaces": [{ "vlan": { "id": 20 }, "ethernet": ethernet }] }),
            &network(),
        );
        let ports = values(find(&plan, "stw_bv20").unwrap())["ports"].clone();
        let r: Vec<String> = reasons(&plan).iter().map(|r| r.to_string()).collect();
        (ports, r, plan.rejected)
    };
    let (ports, r, rejected) = ports_and_reasons(json!([
        { "select-ports": ["LAN4"], "vlan-tag": "un-tagged" },
        { "select-ports": ["LAN*"], "vlan-tag": "tagged" }
    ]));
    assert_eq!(ports, json!(["lan1:t", "lan2:t", "lan3:t", "lan4:u*"]));
    assert_eq!(
        r,
        ["/interfaces/0/ethernet/0 already selects lan4 un-tagged"]
    );
    assert_eq!(
        rejected[0].parameter["/interfaces/0/ethernet/1/vlan-tag"],
        json!({ "port": "lan4", "vlan-tag": "tagged" })
    );
    let (ports, r, _) = ports_and_reasons(json!([
        { "select-ports": ["LAN4"] },
        { "select-ports": ["LAN4"], "vlan-tag": "untagged" }
    ]));
    assert_eq!(ports, json!(["lan4:t"]));
    assert_eq!(r, ["/interfaces/0/ethernet/0 already selects lan4 tagged"]);
    // The same port the same way twice is one member, nothing refused.
    let (ports, r, _) = ports_and_reasons(json!([
        { "select-ports": ["LAN4"], "vlan-tag": "un-tagged" },
        { "select-ports": ["LAN4"], "vlan-tag": "untagged" }
    ]));
    assert_eq!((ports, r.len()), (json!(["lan4:u*"]), 0));
}

/// ethernet entries that aren't objects, a select-ports that isn't a list, and port names
/// that aren't strings are refused, not dropped: the rest of the selection still applies.
#[test]
fn ethernet_that_doesnt_parse_is_refused() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 20 }, "ethernet": [
            { "select-ports": ["LAN2", 3] },
            "LAN3",
            { "select-ports": "LAN4" }
        ] }] }),
        &network(),
    );
    assert_eq!(
        values(find(&plan, "stw_bv20").unwrap())["ports"],
        json!(["lan2:t"])
    );
    let r = reasons(&plan);
    assert_eq!(
        r,
        [
            "a port name is a string",
            "an ethernet entry is an object",
            "select-ports is a list of port names"
        ],
        "{r:?}"
    );
    assert!(
        plan.rejected[1]
            .parameter
            .get("/interfaces/0/ethernet/1")
            .is_some()
    );
}

/// An interface that isn't an object would be read as the device's lan; one with a role
/// other than TIP's upstream or downstream isn't taken as either.
#[test]
fn an_interface_is_an_object_with_tips_roles() {
    let plan = plan_for(&json!({ "interfaces": ["guest"] }), &network());
    assert_eq!(reasons(&plan), ["an interface is an object"]);
    assert!(
        plan.ops.iter().all(|op| matches!(op, Op::Delete { .. })),
        "{:?}",
        plan.ops
    );
    for role in [json!(5), json!("sideways"), json!("Downstream")] {
        let plan = plan_for(
            &json!({ "interfaces": [{ "role": role.clone(), "ssids": [ssid("Home")] }] }),
            &network(),
        );
        assert_eq!(
            reasons(&plan),
            ["a role is upstream or downstream"],
            "{role}"
        );
        assert_eq!(ssid_network(&plan).as_deref(), Some("lan"), "{role}");
    }
    for role in ["upstream", "downstream"] {
        let plan = plan_for(
            &json!({ "interfaces": [{ "role": role, "ssids": [ssid("Home")] }] }),
            &network(),
        );
        assert!(plan.rejected.is_empty(), "{role}: {:?}", reasons(&plan));
    }
}

/// Ports asked of a joined VLAN that the board lacks are refused, as for a new VLAN: they
/// used to be dropped before the comparison, so the request matched and was answered 0.
#[test]
fn a_joined_vlans_port_the_board_lacks_is_refused() {
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 12 }, "ssids": [ssid("IoT")],
            "ethernet": [{ "select-ports": ["WAN*", "LAN1", "LAN9"] }] }] }),
        &network(),
    );
    let r = reasons(&plan);
    assert_eq!(r, ["no such ports on this device"], "{r:?}");
    assert_eq!(ssid_network(&plan).as_deref(), Some("openuf_v12"));
    assert!(find(&plan, "openuf_bv12").is_none() && find(&plan, "stw_bv12").is_none());
    // Its ports said as TIP spells them match: nothing refused.
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 1 },
            "ethernet": [{ "select-ports": ["WAN*", "LAN1", "LAN2", "LAN3"], "vlan-tag": "un-tagged" }] }] }),
        &network(),
    );
    assert!(plan.rejected.is_empty(), "{:?}", reasons(&plan));
}

/// The agent owns a network section only when it's named `stw_*` and marked: a user's VLAN
/// and interface that carry the marker are joined as anyone's, and never deleted.
#[test]
fn network_ownership_takes_the_name_and_the_marker() {
    let n = Network::from_uci(
        json!({ "values": {
            "br": { ".type": "device", "type": "bridge", "name": "br-lan",
                    "ports": ["lan1", "lan2"], "vlan_filtering": "1" },
            "mybv50": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "50", "ports": ["lan1:t"], MARKER: "1" },
            "myvlan": { ".type": "interface", "device": "br-lan.50", "proto": "none", MARKER: "1" },
            "stw_bv30": { ".type": "bridge-vlan", "device": "br-lan", "vlan": "30", "ports": ["lan1:t"], MARKER: "1" },
            "stw_vlan30": { ".type": "interface", "device": "br-lan.30", "proto": "none", MARKER: "1" },
        }})
        .as_object()
        .unwrap(),
    );
    assert_eq!(
        n.owned,
        [
            ("stw_bv30".to_string(), "bridge-vlan".to_string()),
            ("stw_vlan30".to_string(), "interface".to_string())
        ]
    );
    assert!(!n.vlans[&50].owned);
    let plan = plan_for(&json!({ "uuid": 1 }), &n);
    let deleted: Vec<&str> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Delete { section, .. } => Some(section.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, ["stw_bv30", "stw_vlan30"]);
    let plan = plan_for(
        &json!({ "interfaces": [{ "vlan": { "id": 50 }, "ssids": [ssid("X")] }] }),
        &n,
    );
    assert_eq!(ssid_network(&plan).as_deref(), Some("myvlan"));
    assert!(find(&plan, "mybv50").is_none() && find(&plan, "myvlan").is_none());
}
