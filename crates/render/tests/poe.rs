use serde_json::{Value, json};
use steward_render::{Current, Network, Op, Plan, Poe, Ports, Wireless, render};

/// Shaped like realtek-poe's default config (as a switch's image makes it): lan1 to lan4
/// powered, lan3's power off.
fn poe() -> Poe {
    let answer = json!({ "values": {
        "cfg01": { ".type": "global", "budget": "170" },
        "cfg02": { ".type": "port", "id": "1", "name": "lan1", "enable": "1", "priority": "2", "poe_plus": "1" },
        "cfg03": { ".type": "port", "id": "2", "name": "lan2", "enable": "1" },
        "cfg04": { ".type": "port", "id": "3", "name": "lan3", "enable": "0" },
        "cfg05": { ".type": "port", "id": "4", "name": "lan4", "enable": "1" },
    }});
    Poe::from_uci(answer.as_object().unwrap())
}

fn ports() -> Ports {
    Ports {
        lan: ["lan1", "lan2", "lan3", "lan4", "lan5"]
            .map(String::from)
            .to_vec(),
        wan: vec!["wan".into()],
    }
}

fn plan(config: Value, poe: Option<&Poe>) -> Plan {
    render(
        &config,
        &Current {
            wireless: &Wireless::default(),
            network: &Network::default(),
            ports: &ports(),
            poe,
            dhcp: None,
            firewall: None,
        },
    )
}

fn enables(plan: &Plan) -> Vec<(String, String)> {
    plan.ops
        .iter()
        .filter_map(|op| match op {
            Op::Set {
                config,
                section,
                values,
            } if config == "poe" => Some((section.clone(), values["enable"].as_str()?.to_owned())),
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

#[test]
fn realtek_poes_ports_are_read_in_id_order() {
    let p = poe();
    let names: Vec<(&str, bool)> = p
        .ports
        .iter()
        .map(|p| (p.name.as_str(), p.enable))
        .collect();
    assert_eq!(
        names,
        [
            ("lan1", true),
            ("lan2", true),
            ("lan3", false),
            ("lan4", true)
        ]
    );
    assert_eq!(p.port("lan3").unwrap().section, "cfg04");
}

#[test]
fn admin_mode_sets_the_ports_power() {
    let config = json!({ "ethernet": [
        { "select-ports": ["LAN1", "LAN3"], "poe": { "admin-mode": true } },
        { "select-ports": ["LAN2"], "poe": { "admin-mode": false } },
    ]});
    let plan = plan(config, Some(&poe()));
    assert_eq!(
        enables(&plan),
        [
            ("cfg02".into(), "1".into()),
            ("cfg03".into(), "0".into()),
            ("cfg04".into(), "1".into())
        ]
    );
    // Each is the device's own section: listed for the agent to record what it replaces.
    assert!(
        plan.device_options
            .contains(&("poe".into(), "cfg04".into(), "enable".into()))
    );
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

#[test]
fn a_wildcard_takes_the_powered_ports_it_selects() {
    let plan = plan(
        json!({ "ethernet": [{ "select-ports": ["LAN*"], "poe": { "admin-mode": false } }] }),
        Some(&poe()),
    );
    assert_eq!(enables(&plan).len(), 4, "lan5 has no PoE and is skipped");
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
    let plan = plan_all();
    assert_eq!(enables(&plan).len(), 4);
}

fn plan_all() -> Plan {
    plan(
        json!({ "ethernet": [{ "select-ports": ["*"], "poe": { "admin-mode": true } }] }),
        Some(&poe()),
    )
}

#[test]
fn what_it_cant_do_is_refused() {
    let cases = [
        (
            json!({ "ethernet": [{ "select-ports": ["LAN1"], "poe": { "admin-mode": true } }] }),
            None,
            "no PoE controller",
        ),
        (
            json!({ "ethernet": [{ "select-ports": ["LAN5"], "poe": { "admin-mode": true } }] }),
            Some(poe()),
            "lan5 has no PoE",
        ),
        (
            json!({ "ethernet": [{ "select-ports": ["LAN9"], "poe": {} }] }),
            Some(poe()),
            "no such ports",
        ),
        (
            json!({ "ethernet": [
                { "select-ports": ["LAN1"], "poe": { "admin-mode": true } },
                { "select-ports": ["LAN*"], "poe": { "admin-mode": false } } ] }),
            Some(poe()),
            "/ethernet/0 already turns lan1's power on",
        ),
        (
            json!({ "ethernet": [{ "select-ports": ["LAN1"], "speed": 1000, "duplex": "full",
                                   "enabled": false }] }),
            Some(poe()),
            "speed isn't supported yet",
        ),
        (
            json!({ "ethernet": [{ "select-ports": ["LAN1"], "lldp": true }] }),
            Some(poe()),
            "lldp isn't supported yet",
        ),
        (
            json!({ "ethernet": { "select-ports": ["LAN1"], "poe": {} } }),
            Some(poe()),
            "ethernet is a list",
        ),
        (
            json!({ "ethernet": ["LAN1"] }),
            Some(poe()),
            "an ethernet entry is an object",
        ),
    ];
    for (config, poe, want) in cases {
        let plan = plan(config, poe.as_ref());
        assert!(reasons(&plan).contains(want), "{want}: {}", reasons(&plan));
    }
    // The contradiction keeps the first entry's mode.
    let plan = plan(
        json!({ "ethernet": [
            { "select-ports": ["LAN1"], "poe": { "admin-mode": true } },
            { "select-ports": ["LAN1"], "poe": { "admin-mode": false } } ] }),
        Some(&poe()),
    );
    assert_eq!(enables(&plan), [("cfg02".into(), "1".into())]);
}

#[test]
fn admin_mode_is_a_boolean_and_poe_an_object() {
    // Only a missing admin-mode takes the default (on): "false" isn't false, and nothing
    // else is taken for either.
    for mode in [
        json!("false"),
        json!("true"),
        json!(0),
        json!(1),
        json!(null),
    ] {
        let got = plan(
            json!({ "ethernet": [{ "select-ports": ["LAN1"], "poe": { "admin-mode": mode } }] }),
            Some(&poe()),
        );
        assert!(
            reasons(&got).contains("admin-mode is true or false"),
            "{mode}: {}",
            reasons(&got)
        );
        assert!(enables(&got).is_empty(), "{mode}: {:?}", enables(&got));
        assert!(got.device_options.is_empty());
    }
    for p in [json!(true), json!("on"), json!([]), json!(null)] {
        let got = plan(
            json!({ "ethernet": [{ "select-ports": ["LAN1"], "poe": p }] }),
            Some(&poe()),
        );
        assert!(
            reasons(&got).contains("poe is an object"),
            "{p}: {}",
            reasons(&got)
        );
        assert!(enables(&got).is_empty(), "{p}");
    }
    // A bad entry doesn't take the others with it.
    let two = plan(
        json!({ "ethernet": [
            { "select-ports": ["LAN1"], "poe": { "admin-mode": "false" } },
            { "select-ports": ["LAN2"], "poe": { "admin-mode": false } } ] }),
        Some(&poe()),
    );
    assert_eq!(two.rejected.len(), 1, "{}", reasons(&two));
    assert_eq!(enables(&two), [("cfg03".into(), "0".into())]);
    // Missing: on.
    let missing = plan(
        json!({ "ethernet": [{ "select-ports": ["LAN3"], "poe": {} }] }),
        Some(&poe()),
    );
    assert!(missing.rejected.is_empty(), "{}", reasons(&missing));
    assert_eq!(enables(&missing), [("cfg04".into(), "1".into())]);
}

#[test]
fn port_ids_are_read_as_realtek_poe_reads_them() {
    // realtek-poe reads `id` with strtoul(id, NULL, 0) and drops 0 and anything past 48.
    let read = |id: &str| {
        let answer = json!({ "values": {
            "cfg01": { ".type": "port", "id": id, "name": "lan1", "enable": "1" },
        }});
        Poe::from_uci(answer.as_object().unwrap()).ports.len() == 1
    };
    for good in [
        "1", "0x1", "0X1", "01", "010", "0x30", "48", " 2", "\t3", "+4", "5abc", "0x1g",
    ] {
        assert!(read(good), "{good:?} is a port");
    }
    for bad in [
        "0",
        "49",
        "0x31",
        "061",
        "-1",
        "-0",
        "08",
        "0x",
        "0xg",
        "",
        "abc",
        " ",
        "+",
        "- 1",
        "99999999999999999999999",
    ] {
        assert!(!read(bad), "{bad:?} isn't a port");
    }
    // Hex and octal ids sort by their value: 0x2 (2) before 010 (8) before 9.
    let answer = json!({ "values": {
        "a": { ".type": "port", "id": "9", "name": "lan9", "enable": "1" },
        "b": { ".type": "port", "id": "010", "name": "lan8", "enable": "1" },
        "c": { ".type": "port", "id": "0x2", "name": "lan2", "enable": "1" },
    }});
    let names: Vec<String> = Poe::from_uci(answer.as_object().unwrap())
        .ports
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["lan2", "lan8", "lan9"]);
}

#[test]
fn port_patterns_are_strings() {
    // A pattern of the wrong kind is refused, never dropped: the entry would be answered 0.
    let got = plan(
        json!({ "ethernet": [{ "select-ports": ["LAN1", 5, null], "poe": { "admin-mode": false } }] }),
        Some(&poe()),
    );
    assert_eq!(got.rejected.len(), 2, "{}", reasons(&got));
    let r = reasons(&got);
    assert!(r.contains("/ethernet/0/select-ports/1"), "{r}");
    assert!(r.contains("/ethernet/0/select-ports/2"), "{r}");
    assert!(r.contains("a port pattern is a string"), "{r}");
    // The strings in it still count.
    assert_eq!(enables(&got), [("cfg02".into(), "0".into())]);
    // A select-ports that isn't a list names no port, and the answer shows what was sent.
    let got = plan(
        json!({ "ethernet": [{ "select-ports": "LAN1", "poe": { "admin-mode": false } }] }),
        Some(&poe()),
    );
    assert!(
        reasons(&got)
            .contains(r#"{"/ethernet/0/select-ports":"LAN1"} select-ports names the ports"#),
        "{}",
        reasons(&got)
    );
    assert!(enables(&got).is_empty());
}
