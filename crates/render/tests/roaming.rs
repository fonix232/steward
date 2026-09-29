use serde_json::{Map, Value, json};
use steward_render::{MARKER, Op, Plan, Usteer, Wireless, wireless};

fn current(usteer: Option<Usteer>) -> Wireless {
    let answer = json!({ "values": {
        "radio0": { ".type": "wifi-device", "band": "2g" },
        "radio1": { ".type": "wifi-device", "band": "5g" },
    }});
    let mut w = Wireless::from_uci(answer.as_object().unwrap());
    w.usteer = usteer;
    w
}

fn steering_everything() -> Option<Usteer> {
    Some(Usteer {
        running: true,
        ssid_list: None,
    })
}

fn ssid(v: Value) -> Value {
    json!({ "interfaces": [{ "name": "LAN", "ssids": [v] }] })
}

fn psk(extra: Value) -> Value {
    let mut s = json!({ "name": "Home", "wifi-bands": ["2G", "5G"],
                        "encryption": { "proto": "psk2", "key": "correct horse battery" } });
    for (k, v) in extra.as_object().unwrap() {
        s[k] = v.clone();
    }
    s
}

fn find<'a>(plan: &'a Plan, section: &str) -> &'a Map<String, Value> {
    plan.ops
        .iter()
        .find_map(|op| match op {
            Op::Add { name, values, .. } if name == section => Some(values),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {section}"))
}

fn reasons(plan: &Plan) -> String {
    plan.rejected
        .iter()
        .map(|r| format!("{} {} ", r.parameter, r.reason))
        .collect()
}

#[test]
fn roaming_turns_on_802_11r_with_the_scripts_defaults() {
    let plan = wireless(&ssid(psk(json!({ "roaming": true }))), &current(None));
    for section in ["stw_0_0_2g", "stw_0_0_5g"] {
        let v = find(&plan, section);
        assert_eq!(v["ieee80211r"], "1");
        assert_eq!(v["ft_over_ds"], "0", "over the air by default");
        assert_eq!(v["ft_psk_generate_local"], "0", "uCentral's default");
        // No mobility domain or key holders: the scripts derive both, the same on every AP.
        assert!(!v.contains_key("mobility_domain") && !v.contains_key("r0kh"));
    }
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

#[test]
fn a_roaming_object_sets_what_it_says() {
    let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let r = json!({ "roaming": { "message-exchange": "ds", "domain-identifier": "ABCD",
                                 "generate-psk": true, "key-aes-256": key } });
    let plan = wireless(&ssid(psk(r)), &current(None));
    let v = find(&plan, "stw_0_0_5g");
    assert_eq!(
        (
            &v["ft_over_ds"],
            &v["mobility_domain"],
            &v["ft_psk_generate_local"]
        ),
        (&json!("1"), &json!("abcd"), &json!("1"))
    );
    assert_eq!(v["r0kh"], json!([format!("ff:ff:ff:ff:ff:ff,*,{key}")]));
    assert_eq!(
        v["r1kh"],
        json!([format!("00:00:00:00:00:00,00:00:00:00:00:00,{key}")])
    );
    let holders = json!({ "roaming": {
        "pmk-r0-key-holder": "00:00:5e:00:53:01,ap1,00112233445566778899aabbccddeeff",
        "pmk-r1-key-holder": "00:00:5e:00:53:01,00:00:5e:00:53:01,00112233445566778899aabbccddeeff" } });
    let plan = wireless(&ssid(psk(holders)), &current(None));
    assert_eq!(
        find(&plan, "stw_0_0_5g")["r0kh"],
        json!(["00:00:5e:00:53:01,ap1,00112233445566778899aabbccddeeff"])
    );
}

#[test]
fn an_enterprise_ssid_gets_key_holders_the_scripts_cant_derive() {
    let corp = |secret: &str| {
        json!({ "name": "Corp", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" }, "roaming": true,
                "radius": { "authentication": { "host": "192.0.2.10", "port": 1812, "secret": secret } } })
    };
    let a = wireless(&ssid(corp("s3cret-auth")), &current(None));
    let b = wireless(&ssid(corp("s3cret-auth")), &current(None));
    let other = wireless(&ssid(corp("another-secret")), &current(None));
    let (va, vb, vo) = (
        find(&a, "stw_0_0_5g"),
        find(&b, "stw_0_0_5g"),
        find(&other, "stw_0_0_5g"),
    );
    let r0 = va["r0kh"][0].as_str().expect("derived key holders");
    assert!(
        r0.starts_with("ff:ff:ff:ff:ff:ff,*,") && r0.len() == "ff:ff:ff:ff:ff:ff,*,".len() + 32
    );
    assert!(!r0.contains("s3cret"));
    assert_eq!(va["r0kh"], vb["r0kh"], "the same on every AP");
    assert_ne!(va["r0kh"], vo["r0kh"], "and bound to the secret");
    assert!(!va.contains_key("ft_psk_generate_local"));
}

#[test]
fn roaming_needs_wpa2_with_a_key_or_802_1x() {
    for (proto, key) in [
        ("none", None),
        ("owe", None),
        ("psk", Some("correct horse battery")),
    ] {
        let mut s = json!({ "name": "Open", "wifi-bands": ["5G"], "encryption": { "proto": proto },
                            "roaming": true });
        if let Some(k) = key {
            s["encryption"]["key"] = json!(k);
        }
        let plan = wireless(&ssid(s), &current(None));
        assert!(
            !find(&plan, "stw_0_0_5g").contains_key("ieee80211r"),
            "{proto}"
        );
        assert!(
            reasons(&plan).contains("fast roaming needs WPA2"),
            "{proto}"
        );
    }
    let sae = json!({ "name": "Home", "wifi-bands": ["5G"], "roaming": { "generate-psk": true },
                      "encryption": { "proto": "sae", "key": "correct horse battery" } });
    let plan = wireless(&ssid(sae), &current(None));
    assert_eq!(find(&plan, "stw_0_0_5g")["ieee80211r"], "1");
    assert!(
        reasons(&plan).contains("only WPA2-PSK networks"),
        "{}",
        reasons(&plan)
    );
}

#[test]
fn bad_roaming_values_are_refused_without_their_keys() {
    let r = json!({ "roaming": { "message-exchange": "wire", "domain-identifier": "xyz",
                                 "key-aes-256": "s3cret-short",
                                 "pmk-r0-key-holder": "s3cret-holder" } });
    let plan = wireless(&ssid(psk(r)), &current(None));
    let got = reasons(&plan);
    for part in ["message-exchange", "domain-identifier", "64 hex digits"] {
        assert!(got.contains(part), "{part}: {got}");
    }
    assert!(!got.contains("s3cret"), "{got}");
    let r = json!({ "roaming": { "pmk-r0-key-holder": "s3cret-holder" } });
    let got = reasons(&wireless(&ssid(psk(r)), &current(None)));
    assert!(got.contains("pairs") && !got.contains("s3cret"), "{got}");
}

#[test]
fn rrm_sets_802_11k_and_its_relatives() {
    let r = json!({ "rrm": { "neighbor-reporting": true, "reduced-neighbor-reporting": true,
                             "ftm-responder": true, "lci": "0102", "civic-location": "abcd",
                             "stationary-ap": true } });
    let plan = wireless(&ssid(psk(r)), &current(None));
    let v = find(&plan, "stw_0_0_5g");
    assert_eq!(
        (
            &v["ieee80211k"],
            &v["rnr"],
            &v["ftm_responder"],
            &v["lci"],
            &v["civic"]
        ),
        (
            &json!("1"),
            &json!("1"),
            &json!("1"),
            &json!("0102"),
            &json!("abcd")
        )
    );
    assert!(
        reasons(&plan).contains("stationary-ap"),
        "{}",
        reasons(&plan)
    );
}

#[test]
fn steering_turns_on_802_11k_and_v_for_usteer() {
    let steered = psk(json!({ "services": ["wifi-steering"] }));
    let plan = wireless(&ssid(steered.clone()), &current(steering_everything()));
    let v = find(&plan, "stw_0_0_2g");
    assert_eq!(
        (&v["bss_transition"], &v["ieee80211k"]),
        (&json!("1"), &json!("1"))
    );
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
    // 802.11k stays off when the configuration says so.
    let mut off = steered.clone();
    off["rrm"] = json!({ "neighbor-reporting": false });
    let plan = wireless(&ssid(off), &current(steering_everything()));
    assert_eq!(find(&plan, "stw_0_0_2g")["ieee80211k"], "0");
    // usteer steering its listed SSIDs, one of them this one.
    let listed = Some(Usteer {
        running: true,
        ssid_list: Some(vec!["Home".into()]),
    });
    assert!(
        wireless(&ssid(steered.clone()), &current(listed))
            .rejected
            .is_empty()
    );
}

#[test]
fn steering_usteer_cant_give_is_refused_but_keeps_802_11k_and_v() {
    let steered = psk(json!({ "services": ["wifi-steering"] }));
    let cases = [
        (None, "isn't installed"),
        (Some(Usteer::default()), "isn't running"),
        (
            Some(Usteer {
                running: true,
                ssid_list: Some(vec!["Other".into()]),
            }),
            "only the SSIDs in its ssid_list",
        ),
    ];
    for (usteer, reason) in cases {
        let plan = wireless(&ssid(steered.clone()), &current(usteer));
        assert!(
            reasons(&plan).contains(reason),
            "{reason}: {}",
            reasons(&plan)
        );
        assert!(reasons(&plan).contains("services/0"));
        assert_eq!(find(&plan, "stw_0_0_5g")["bss_transition"], "1");
    }
}

/// Every option written exists in the wifi scripts (tests/wireless-schema.json).
#[test]
fn every_roaming_option_exists_in_the_wifi_scripts_schema() {
    let schema: Value = serde_json::from_str(include_str!("wireless-schema.json")).unwrap();
    let known = schema["wifi-iface"].as_array().unwrap();
    let all = psk(json!({
        "services": ["wifi-steering"],
        "roaming": { "message-exchange": "ds", "domain-identifier": "abcd", "generate-psk": true,
                     "key-aes-256": "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff" },
        "rrm": { "neighbor-reporting": true, "reduced-neighbor-reporting": true,
                 "ftm-responder": true, "lci": "0102", "civic-location": "abcd" } }));
    let plan = wireless(&ssid(all), &current(steering_everything()));
    for op in &plan.ops {
        if let Op::Add { values, .. } = op {
            for option in values.keys().filter(|k| k.as_str() != MARKER) {
                assert!(known.iter().any(|o| o == option), "wifi-iface.{option}");
            }
        }
    }
}
