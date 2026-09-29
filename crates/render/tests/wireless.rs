use serde_json::{Map, Value, json};
use steward_render::{MARKER, Op, Plan, Wireless, wireless};

/// Shaped like bifrost's `uci get wireless`: two radios, the device's own SSIDs, one SSID
/// the agent owns and keeps, and one it owns that the new configuration drops.
fn current() -> Wireless {
    let answer = json!({ "values": {
        "radio0": { ".type": "wifi-device", "band": "2g", "channel": "1" },
        "radio1": { ".type": "wifi-device", "band": "5g", "channel": "36" },
        "default_radio0": { ".type": "wifi-iface", "device": "radio0", "ssid": "OpenWrt" },
        "default_radio1": { ".type": "wifi-iface", "device": "radio1", "ssid": "OpenWrt" },
        "stw_0_0_2g": { ".type": "wifi-iface", "device": "radio0", MARKER: "1" },
        "stw_9_9_5g": { ".type": "wifi-iface", "device": "radio1", MARKER: "1" },
    }});
    Wireless::from_uci(answer.as_object().unwrap())
}

fn config() -> Value {
    json!({
        "uuid": 7,
        "radios": [
            { "band": "2G", "channel": 6, "channel-mode": "HE", "country": "GB", "tx-power": 20 },
            { "band": "5G", "channel": 36, "channel-mode": "HE", "channel-width": 80, "country": "GB" },
            { "band": "6G", "channel": 37 }
        ],
        "interfaces": [{
            "name": "LAN",
            "role": "downstream",
            "ssids": [
                { "name": "Home", "wifi-bands": ["2G", "5G"],
                  "encryption": { "proto": "psk2", "key": "correct horse battery", "ieee80211w": "optional" } },
                { "name": "Guest", "wifi-bands": ["2G"], "hidden-ssid": true, "isolate-clients": true,
                  "encryption": { "proto": "none" } },
                { "name": "Backhaul", "wifi-bands": ["5G"], "bss-mode": "mesh" },
                { "name": "Office", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2", "ieee80211w": "required" } },
                { "name": "Short", "wifi-bands": ["5G"], "encryption": { "proto": "sae", "key": "short" } }
            ]
        }]
    })
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

#[test]
fn radios_are_set_on_the_devices_own_sections() {
    let plan = wireless(&config(), &current());
    let r0 = values(find(&plan, "radio0").expect("radio0"));
    assert_eq!(r0["channel"], "6");
    // 2.4 GHz: the schema's default width (80) doesn't exist there, so 20.
    assert_eq!(r0["htmode"], "HE20");
    assert_eq!(r0["country"], "GB");
    assert_eq!(r0["txpower"], "20");
    assert_eq!(r0["disabled"], "0");
    assert_eq!(values(find(&plan, "radio1").unwrap())["htmode"], "HE80");
    // Every radio option it sets is listed, for the agent to record what it replaces.
    assert!(
        plan.radio_options
            .contains(&("radio0".into(), "txpower".into()))
    );
    assert!(
        plan.radio_options
            .iter()
            .all(|(s, _)| s == "radio0" || s == "radio1")
    );
}

#[test]
fn ssids_go_into_owned_sections_only() {
    let plan = wireless(&config(), &current());
    // Home on both bands: the existing owned 2.4 GHz section is updated, the 5 GHz one added.
    let home2 = find(&plan, "stw_0_0_2g").unwrap();
    assert!(matches!(home2, Op::Set { .. }));
    let home5 = find(&plan, "stw_0_0_5g").unwrap();
    assert!(matches!(home5, Op::Add { kind, .. } if kind == "wifi-iface"));
    let v = values(home5);
    assert_eq!(v["device"], "radio1");
    assert_eq!(v["ssid"], "Home");
    assert_eq!(v["encryption"], "psk2");
    assert_eq!(v["key"], "correct horse battery");
    assert_eq!(v["ieee80211w"], "1");
    assert_eq!(v["network"], "lan");
    assert_eq!(v[MARKER], "1");
    let guest = values(find(&plan, "stw_0_1_2g").unwrap());
    assert_eq!(
        (guest["hidden"].as_str(), guest["isolate"].as_str()),
        (Some("1"), Some("1"))
    );
    assert!(!guest.contains_key("key"));
    // The owned section the configuration dropped is deleted; the device's own never touched.
    assert!(matches!(find(&plan, "stw_9_9_5g"), Some(Op::Delete { .. })));
    assert!(find(&plan, "default_radio0").is_none() && find(&plan, "default_radio1").is_none());
    for op in &plan.ops {
        if let Op::Delete { section, .. } = op {
            assert!(section.starts_with("stw_"), "deleted {section}");
        }
    }
}

#[test]
fn what_it_cant_do_comes_back_as_rejections() {
    let plan = wireless(&config(), &current());
    let reasons: Vec<&str> = plan.rejected.iter().map(|r| r.reason.as_str()).collect();
    assert!(
        reasons.iter().any(|r| r.contains("no 6G radio")),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().any(|r| r.contains("mesh isn't supported")),
        "{reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("wpa2 needs a RADIUS authentication server")),
        "{reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("8 to 63 printable characters")),
        "{reasons:?}"
    );
    // Rejected SSIDs get no section.
    assert!(
        find(&plan, "stw_0_2_5g").is_none()
            && find(&plan, "stw_0_3_5g").is_none()
            && find(&plan, "stw_0_4_5g").is_none()
    );
    // A key never ends up in a rejection.
    assert!(
        plan.rejected
            .iter()
            .all(|r| !r.parameter.to_string().contains("short"))
    );
}

#[test]
fn channel_modes_map_onto_htmode() {
    let cases = [
        (
            json!({ "band": "2G", "channel-mode": "VHT", "channel-width": 40 }),
            Some("HT40"),
        ),
        (
            json!({ "band": "5G", "channel-mode": "VHT", "channel-width": 160 }),
            Some("VHT160"),
        ),
        (
            json!({ "band": "5G", "channel-mode": "HT", "channel-width": 80 }),
            None,
        ),
        (
            json!({ "band": "5G", "channel-mode": "HE", "channel-width": 8080 }),
            None,
        ),
    ];
    for (radio, want) in cases {
        let plan = wireless(&json!({ "radios": [radio.clone()] }), &current());
        let section = if radio["band"] == "2G" {
            "radio0"
        } else {
            "radio1"
        };
        let got = values(find(&plan, section).unwrap())
            .get("htmode")
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(got.as_deref(), want, "{radio}");
        assert_eq!(plan.rejected.is_empty(), want.is_some(), "{radio}");
    }
}

/// Every option written exists in the consumer: OpenWrt's wifi scripts ignore unknown options
/// silently. The list is bifrost's /usr/share/schema/wireless.*.json (tests/wireless-schema.json).
#[test]
fn every_option_exists_in_the_wifi_scripts_schema() {
    let schema: Value = serde_json::from_str(include_str!("wireless-schema.json")).unwrap();
    let known =
        |kind: &str, option: &str| schema[kind].as_array().unwrap().iter().any(|o| o == option);
    let plan = wireless(&config(), &current());
    for op in &plan.ops {
        let (kind, vals) = match op {
            Op::Add { kind, values, .. } => (kind.as_str(), values),
            Op::Set {
                section, values, ..
            } if section.starts_with("radio") => ("wifi-device", values),
            Op::Set { values, .. } => ("wifi-iface", values),
            Op::Unset { .. } | Op::Delete { .. } => continue,
        };
        for option in vals.keys().filter(|k| k.as_str() != MARKER) {
            assert!(
                known(kind, option),
                "{kind}.{option} isn't read by the wifi scripts"
            );
        }
        if let Some(h) = vals.get("htmode") {
            assert!(
                schema["htmode"].as_array().unwrap().contains(h),
                "htmode {h}"
            );
        }
    }
}

fn reasons(plan: &Plan) -> String {
    plan.rejected
        .iter()
        .map(|r| format!("[{} {}] ", r.parameter, r.reason))
        .collect()
}

/// Shaped like bifrost: radio0 (wl0) runs HT only, radio1 (wl1) HT, VHT and HE up to HE160
/// but no VHT160. Filled in from `network.wireless status` and `iwinfo info`, as the device
/// reports them.
fn bifrost() -> Wireless {
    let mut w = current();
    let status = json!({
        "radio0": { "config": { "phy": "wl0", "band": "2g" }, "interfaces": [{ "ifname": "wl0-ap0" }] },
        "radio1": { "config": { "phy": "wl1", "band": "5g" }, "interfaces": [{ "ifname": "wl1-ap0" }] }
    });
    w.read_htmodes(status.as_object().unwrap(), |device| {
        let modes = match device {
            "wl0" => json!(["HT20", "HT40"]),
            "wl1" => json!([
                "HT20", "HT40", "VHT20", "VHT40", "VHT80", "HE20", "HE40", "HE80", "HE160"
            ]),
            _ => return None,
        };
        json!({ "phy": device, "htmodes": modes })
            .as_object()
            .cloned()
    });
    w
}

/// Two radios, and a third on 6 GHz.
fn with_6g() -> Wireless {
    let answer = json!({ "values": {
        "radio0": { ".type": "wifi-device", "band": "2g" },
        "radio1": { ".type": "wifi-device", "band": "5g" },
        "radio2": { ".type": "wifi-device", "band": "6g" },
    }});
    Wireless::from_uci(answer.as_object().unwrap())
}

#[test]
fn htmodes_are_read_from_the_phy_or_an_interface() {
    let w = bifrost();
    assert_eq!(
        w.radios[0].htmodes,
        Some(vec!["HT20".to_string(), "HT40".to_string()])
    );
    assert_eq!(w.radios[1].htmodes.as_ref().map(Vec::len), Some(9));
    // A radio addressed by path= has no phy in its config: one of its interfaces answers.
    // One that iwinfo can't read, or that reports nothing, stays unchecked.
    let mut w = current();
    let status = json!({
        "radio0": { "config": { "path": "platform/18000000.wmac" }, "interfaces": [{ "ifname": "phy0-ap0" }] },
        "radio1": { "config": { "phy": "wl1" }, "interfaces": [] }
    });
    w.read_htmodes(status.as_object().unwrap(), |device| match device {
        "phy0-ap0" => json!({ "htmodes": ["HT20"] }).as_object().cloned(),
        _ => json!({ "htmodes": [] }).as_object().cloned(),
    });
    assert_eq!(w.radios[0].htmodes, Some(vec!["HT20".to_string()]));
    assert_eq!(w.radios[1].htmodes, None);
}

/// A mode or width the radio doesn't run is replaced by the best one it does (the same width
/// in a lower mode, then narrower), listed with the substitution: answered 1, not 0. With
/// nothing it runs, htmode is left alone and the request rejected.
#[test]
fn htmode_falls_back_to_what_the_radio_supports() {
    let cases = [
        // The schema's default mode (HE) on the 2.4 GHz radio, which has no HE.
        (
            json!({ "band": "2G", "channel": 6 }),
            "HT20",
            Some(("HT", 20)),
        ),
        (
            json!({ "band": "2G", "channel-width": 40 }),
            "HT40",
            Some(("HT", 40)),
        ),
        (
            json!({ "band": "2G", "channel-mode": "HT", "channel-width": 40 }),
            "HT40",
            None,
        ),
        (
            json!({ "band": "5G", "channel-mode": "VHT", "channel-width": 160 }),
            "VHT80",
            Some(("VHT", 80)),
        ),
        (
            json!({ "band": "5G", "channel-mode": "EHT", "channel-width": 80 }),
            "HE80",
            Some(("HE", 80)),
        ),
        (
            json!({ "band": "5G", "channel-mode": "EHT", "channel-width": 160 }),
            "HE160",
            Some(("HE", 160)),
        ),
        (
            json!({ "band": "5G", "channel-mode": "HE", "channel-width": 80 }),
            "HE80",
            None,
        ),
    ];
    for (radio, want, substituted) in cases {
        let plan = wireless(&json!({ "radios": [radio.clone()] }), &bifrost());
        let section = if radio["band"] == "2G" {
            "radio0"
        } else {
            "radio1"
        };
        assert_eq!(
            values(find(&plan, section).unwrap())["htmode"],
            want,
            "{radio}"
        );
        match substituted {
            None => assert!(plan.rejected.is_empty(), "{radio}: {}", reasons(&plan)),
            Some((mode, width)) => {
                assert_eq!(plan.rejected.len(), 1, "{radio}: {}", reasons(&plan));
                let r = &plan.rejected[0];
                assert!(r.reason.contains("doesn't support"), "{}", r.reason);
                let sub = r.substitution.as_ref().expect("a substitution");
                assert_eq!(
                    sub["/radios/0/channel-width"],
                    json!({ "channel-mode": mode, "channel-width": width }),
                    "{radio}"
                );
            }
        }
    }
    // Nothing the radio runs fits: rejected, and htmode isn't touched.
    let mut w = bifrost();
    w.radios[1].htmodes = Some(vec!["HE80".into()]);
    let plan = wireless(
        &json!({ "radios": [{ "band": "5G", "channel-mode": "HT", "channel-width": 20 }] }),
        &w,
    );
    assert!(!values(find(&plan, "radio1").unwrap()).contains_key("htmode"));
    assert!(
        reasons(&plan).contains("nor anything narrower"),
        "{}",
        reasons(&plan)
    );
    assert!(plan.rejected[0].substitution.is_none());
    // Unknown capabilities (no iwinfo answer): not checked.
    let plan = wireless(&json!({ "radios": [{ "band": "2G" }] }), &current());
    assert_eq!(values(find(&plan, "radio0").unwrap())["htmode"], "HE20");
    assert!(plan.rejected.is_empty());
}

/// Widths the band doesn't have (80 and 160 MHz on 2.4 GHz, 320 on 5 GHz) and HT or VHT on
/// 6 GHz are rejected, not written.
#[test]
fn widths_are_checked_against_the_band() {
    for (radio, section, want) in [
        (
            json!({ "band": "2G", "channel-mode": "HE", "channel-width": 80 }),
            "radio0",
            None,
        ),
        (
            json!({ "band": "2G", "channel-mode": "HE", "channel-width": 160 }),
            "radio0",
            None,
        ),
        (
            json!({ "band": "5G", "channel-mode": "EHT", "channel-width": 320 }),
            "radio1",
            None,
        ),
        (
            json!({ "band": "6G", "channel-mode": "HT", "channel-width": 20 }),
            "radio2",
            None,
        ),
        (
            json!({ "band": "6G", "channel-mode": "VHT", "channel-width": 80 }),
            "radio2",
            None,
        ),
        (
            json!({ "band": "6G", "channel-mode": "EHT", "channel-width": 320 }),
            "radio2",
            Some("EHT320"),
        ),
        (
            json!({ "band": "5G", "channel-mode": "EHT", "channel-width": 160 }),
            "radio1",
            Some("EHT160"),
        ),
        (
            json!({ "band": "2G", "channel-mode": "HE", "channel-width": 40 }),
            "radio0",
            Some("HE40"),
        ),
    ] {
        let plan = wireless(&json!({ "radios": [radio.clone()] }), &with_6g());
        let got = values(find(&plan, section).unwrap()).get("htmode");
        assert_eq!(got.and_then(Value::as_str), want, "{radio}");
        assert_eq!(
            plan.rejected.is_empty(),
            want.is_some(),
            "{radio}: {}",
            reasons(&plan)
        );
    }
}

/// A channel the band doesn't have is rejected, not written: 2.4 GHz is 1 to 14, 5 GHz 32 to
/// 144 and 149 to 177 in steps of 4, 6 GHz 1 to 233 in steps of 4.
#[test]
fn channels_are_checked_against_the_band() {
    let good = [
        ("2G", 1),
        ("2G", 14),
        ("5G", 36),
        ("5G", 144),
        ("5G", 149),
        ("5G", 177),
        ("6G", 1),
        ("6G", 37),
        ("6G", 233),
    ];
    let bad = [
        ("2G", 36),
        ("2G", 15),
        ("5G", 6),
        ("5G", 38),
        ("5G", 148),
        ("5G", 181),
        ("6G", 2),
        ("6G", 36),
    ];
    let all = good
        .iter()
        .map(|(b, c)| (*b, *c, true))
        .chain(bad.iter().map(|(b, c)| (*b, *c, false)));
    for (band, channel, ok) in all {
        let radio =
            json!({ "band": band, "channel": channel, "channel-mode": "HE", "channel-width": 20 });
        let plan = wireless(&json!({ "radios": [radio.clone()] }), &with_6g());
        let section = match band {
            "2G" => "radio0",
            "5G" => "radio1",
            _ => "radio2",
        };
        let set = values(find(&plan, section).unwrap())
            .get("channel")
            .cloned();
        assert_eq!(set, ok.then(|| json!(channel.to_string())), "{radio}");
        assert_eq!(plan.rejected.is_empty(), ok, "{radio}: {}", reasons(&plan));
        if !ok {
            assert!(reasons(&plan).contains(&format!("has no channel {channel}")));
        }
    }
}

/// Two entries for one band would both set the same radio: the first wins, the others are
/// refused.
#[test]
fn a_band_given_twice_is_refused_after_the_first() {
    let plan = wireless(
        &json!({ "radios": [
            { "band": "5G", "channel": 36 },
            { "band": "2G", "channel": 6 },
            { "band": "5G", "channel": 149 }
        ]}),
        &current(),
    );
    let radio1: Vec<&Op> = plan
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Set { section, .. } if section == "radio1"))
        .collect();
    assert_eq!(radio1.len(), 1);
    assert_eq!(values(radio1[0])["channel"], "36");
    assert_eq!(plan.rejected.len(), 1, "{}", reasons(&plan));
    assert!(plan.rejected[0].parameter.get("/radios/2").is_some());
    assert!(reasons(&plan).contains("set by /radios/0 already"));
}

/// Anything the renderer doesn't handle is rejected, so the answer isn't 0 for a
/// configuration that isn't applied as sent: unknown keys (at the top, on a radio, on an
/// SSID), and values of the wrong kind, which are never guessed at.
#[test]
fn unhandled_fields_are_rejected() {
    let ssid = |extra: Value| {
        let mut s = json!({ "name": "X", "wifi-bands": ["5G"] });
        s.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({ "interfaces": [{ "ssids": [s] }] })
    };
    let cases = [
        (
            json!({ "unit": { "hostname": "ap" } }),
            "unit isn't supported yet",
        ),
        (
            json!({ "services": { "ssh": {} } }),
            "services isn't supported yet",
        ),
        (
            json!({ "radios": [{ "band": "5G", "beacon-interval": 200 }] }),
            "beacon-interval isn't supported yet",
        ),
        (
            json!({ "radios": [{ "band": "5G", "maximum-clients": 10 }] }),
            "maximum-clients isn't supported yet",
        ),
        (
            json!({ "radios": [{ "band": "5G", "mimo": "2x2" }] }),
            "mimo isn't supported yet",
        ),
        (
            json!({ "radios": [{ "band": "5G", "tx-power": "20" }] }),
            "whole number of dBm",
        ),
        (
            json!({ "radios": [{ "band": "5G", "country": 44 }] }),
            "two capital letters",
        ),
        (
            json!({ "radios": [{ "band": "5G", "channel-width": "80" }] }),
            "number of MHz",
        ),
        (json!({ "radios": { "band": "5G" } }), "radios is a list"),
        (
            ssid(json!({ "maximum-clients": 5 })),
            "maximum-clients isn't supported yet",
        ),
        (
            ssid(json!({ "proxy-arp": true })),
            "proxy-arp isn't supported yet",
        ),
        (
            ssid(json!({ "isolate-clients": "yes" })),
            "isolate-clients is true or false",
        ),
        (
            json!({ "interfaces": [{ "ssids": [{ "name": "X" }] }] }),
            "at least one band",
        ),
        (
            json!({ "interfaces": [{ "ssids": { "name": "X" } }] }),
            "ssids is a list",
        ),
    ];
    for (config, want) in cases {
        let plan = wireless(&config, &current());
        assert!(
            reasons(&plan).contains(want),
            "{config}: {}",
            reasons(&plan)
        );
    }
    // The radio still gets what it does handle; the enable of the wrong kind isn't applied.
    let plan = wireless(
        &json!({ "radios": [{ "band": "5G", "channel": 36, "enable": "false" }] }),
        &current(),
    );
    assert!(reasons(&plan).contains("enable is true or false"));
    let r1 = values(find(&plan, "radio1").unwrap());
    assert_eq!(r1["channel"], "36");
    assert!(!r1.contains_key("disabled"));
    // An encryption that isn't an object with a proto would have been an open network: the
    // SSID is refused instead.
    for enc in [
        json!("psk2"),
        json!({ "key": "correct horse battery" }),
        json!({ "proto": ["psk2"] }),
    ] {
        let plan = wireless(&ssid(json!({ "encryption": enc.clone() })), &current());
        assert!(
            plan.ops.iter().all(|op| matches!(op, Op::Delete { .. })),
            "{enc}: {:?}",
            plan.ops
        );
        assert!(!plan.rejected.is_empty(), "{enc}");
        assert!(
            !reasons(&plan).contains("correct horse"),
            "{}",
            reasons(&plan)
        );
    }
}

/// Rejections of blocks the renderer doesn't handle carry them redacted: multi-PSK keys, a
/// captive portal's RADIUS secret and passwords, pass-point's, raw hostapd lines on an SSID
/// and a radio, and raw UCI at the top.
#[test]
fn unsupported_blocks_are_rejected_without_their_secrets() {
    let plan = wireless(
        &json!({
            "config-raw": [["set", "system.@system[0].hostname", "leaked-raw-uci"]],
            "radios": [{ "band": "5G", "hostapd-iface-raw": ["wpa_passphrase=leaked-iface-raw"] }],
            "interfaces": [{ "ssids": [{
                "name": "M", "wifi-bands": ["5G"],
                "encryption": { "proto": "psk2", "key": "correct horse battery" },
                "multi-psk": [{ "mac": "00:00:5e:00:53:01", "key": "leaked-mpsk-key" }],
                "captive": { "auth-mode": "radius", "auth-server": "192.0.2.1",
                             "auth-secret": "leaked-captive-secret",
                             "credentials": [{ "username": "u", "password": "leaked-password" }] },
                "pass-point": { "venue-name": ["x"], "passphrase": "leaked-passphrase" },
                "hostapd-bss-raw": ["wpa_passphrase=leaked-raw-passphrase"]
            }]}]
        }),
        &current(),
    );
    let r = serde_json::to_string(&plan.rejected).unwrap();
    assert_eq!(plan.rejected.len(), 6, "{}", reasons(&plan));
    for leaked in ["leaked", "correct horse battery", "wpa_passphrase"] {
        assert!(!r.contains(leaked), "{leaked} in {r}");
    }
    // What isn't secret is still there, so the answer says what was refused.
    assert!(
        r.contains("auth-mode") && r.contains("00:00:5e:00:53:01"),
        "{r}"
    );
    // The SSID itself runs (with its main key): answered 1, with the rejections.
    assert_eq!(values(find(&plan, "stw_0_0_5g").unwrap())["ssid"], "M");
}

/// An SSID's `encryption` is held to the same rule: keys it doesn't handle are rejected (the
/// SSID runs with its proto and key), and management frame protection is one of TIP's values.
/// Any other refuses the SSID, rather than running it without the protection asked for.
#[test]
fn encryption_is_read_strictly() {
    let ssid = |enc: Value| json!({ "interfaces": [{ "ssids": [{ "name": "X", "wifi-bands": ["5G"], "encryption": enc }] }] });
    let plan = wireless(
        &ssid(json!({ "proto": "psk2", "key": "correct horse battery",
                      "key-caching": false, "unknown-thing": 1 })),
        &current(),
    );
    assert!(
        reasons(&plan).contains("key-caching works on 802.1X (enterprise) SSIDs only")
            && reasons(&plan).contains("unknown-thing isn't supported yet"),
        "{}",
        reasons(&plan)
    );
    assert!(plan.rejected.iter().any(|r| {
        r.parameter
            .get("/interfaces/0/ssids/0/encryption/key-caching")
            .is_some()
    }));
    assert!(!reasons(&plan).contains("correct horse"));
    assert_eq!(
        values(find(&plan, "stw_0_0_5g").unwrap())["encryption"],
        "psk2"
    );
    for bad in [json!(true), json!("sometimes"), json!(1), json!("Required")] {
        let plan = wireless(
            &ssid(
                json!({ "proto": "psk2", "key": "correct horse battery", "ieee80211w": bad.clone() }),
            ),
            &current(),
        );
        assert!(find(&plan, "stw_0_0_5g").is_none(), "{bad}");
        assert!(
            reasons(&plan).contains("ieee80211w is disabled, optional or required"),
            "{bad}: {}",
            reasons(&plan)
        );
    }
    for (enc, want) in [
        (
            json!({ "proto": "psk2", "key": "correct horse battery", "ieee80211w": "disabled" }),
            "0",
        ),
        (
            json!({ "proto": "psk2", "key": "correct horse battery" }),
            "0",
        ),
        (
            json!({ "proto": "psk2", "key": "correct horse battery", "ieee80211w": "optional" }),
            "1",
        ),
        (
            json!({ "proto": "psk2", "key": "correct horse battery", "ieee80211w": "required" }),
            "2",
        ),
        (
            json!({ "proto": "sae", "key": "correct horse battery" }),
            "2",
        ),
        (json!({ "proto": "owe", "ieee80211w": "disabled" }), "2"),
    ] {
        let plan = wireless(&ssid(enc.clone()), &current());
        // owe needs it required: asked disabled, that's a substitution in the answer.
        let subs: Vec<Value> = plan
            .rejected
            .iter()
            .filter_map(|r| r.substitution.clone())
            .collect();
        let expected = if enc["proto"] == "owe" {
            vec![json!("required")]
        } else {
            vec![]
        };
        assert_eq!(subs, expected, "{enc}: {}", reasons(&plan));
        assert_eq!(plan.rejected.len(), expected.len(), "{enc}");
        assert_eq!(
            values(find(&plan, "stw_0_0_5g").unwrap())["ieee80211w"],
            want,
            "{enc}"
        );
    }
}

/// A key on a mode that takes none (none, owe) is rejected, redacted, rather than dropped: its
/// operator believes the SSID is keyed. The SSID still runs as its proto says.
#[test]
fn a_key_on_a_mode_without_one_is_rejected() {
    for proto in ["none", "owe"] {
        let config = json!({ "interfaces": [{ "ssids": [{ "name": "X", "wifi-bands": ["5G"],
            "encryption": { "proto": proto, "key": "correct horse battery" } }] }] });
        let plan = wireless(&config, &current());
        assert_eq!(
            reasons(&plan),
            format!(
                "[{{\"/interfaces/0/ssids/0/encryption/key\":\"…\"}} {proto} takes no key: \
                 only the PSK and SAE modes do] "
            ),
            "{proto}"
        );
        let v = values(find(&plan, "stw_0_0_5g").unwrap());
        assert_eq!(v["encryption"], proto);
        assert!(!v.contains_key("key"), "{proto}");
    }
    // Without a key, nothing to say.
    let plan = wireless(
        &json!({ "interfaces": [{ "ssids": [{ "name": "X", "wifi-bands": ["5G"],
            "encryption": { "proto": "none" } }] }] }),
        &current(),
    );
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

/// tx-power is 0 to 30 dBm (TIP's radio.yml); outside that it's refused, not written.
#[test]
fn tx_power_is_within_the_schemas_range() {
    for (power, ok) in [
        (json!(0), true),
        (json!(30), true),
        (json!(31), false),
        (json!(500), false),
        (json!(-1), false),
    ] {
        let plan = wireless(
            &json!({ "radios": [{ "band": "5G", "tx-power": power.clone() }] }),
            &current(),
        );
        let set = values(find(&plan, "radio1").unwrap())
            .get("txpower")
            .cloned();
        assert_eq!(set, ok.then(|| json!(power.to_string())), "{power}");
        assert_eq!(plan.rejected.is_empty(), ok, "{power}: {}", reasons(&plan));
    }
}

/// The agent owns a section only when it's named `stw_*` and marked: a user's section that
/// carries the marker, or one named like the agent's without it, is never deleted.
#[test]
fn ownership_takes_the_name_and_the_marker() {
    let w = Wireless::from_uci(
        json!({ "values": {
            "radio1": { ".type": "wifi-device", "band": "5g" },
            "myssid": { ".type": "wifi-iface", "device": "radio1", MARKER: "1" },
            "stw_unmarked": { ".type": "wifi-iface", "device": "radio1" },
            "stw_9_9_5g": { ".type": "wifi-iface", "device": "radio1", MARKER: "1" },
        }})
        .as_object()
        .unwrap(),
    );
    assert_eq!(w.owned, ["stw_9_9_5g"]);
    let plan = wireless(&json!({ "uuid": 1 }), &w);
    assert_eq!(
        plan.ops,
        [Op::Delete {
            config: "wireless".into(),
            section: "stw_9_9_5g".into()
        }]
    );
}

/// A band listed twice in wifi-bands is one section, added once.
#[test]
fn a_band_listed_twice_is_added_once() {
    let plan = wireless(
        &json!({ "interfaces": [{ "ssids": [{ "name": "X", "wifi-bands": ["5G", "2G", "5G"] }] }] }),
        &current(),
    );
    let sections: Vec<&str> = plan
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Add { name, .. } => Some(name.as_str()),
            Op::Set { section, .. } if section.starts_with("stw_") => Some(section.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sections, ["stw_0_0_5g", "stw_0_0_2g"]);
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

/// An op shows what it does by name, for logs and errors, never a value: no key, no SSID.
#[test]
fn an_op_shows_names_not_values() {
    let plan = wireless(&config(), &current());
    let shown: Vec<String> = plan.ops.iter().map(Op::to_string).collect();
    let home = "add wireless wifi-iface stw_0_0_5g (key, encryption, ieee80211w, device, mode, \
                ssid, network, hidden, isolate, disabled, steward)";
    assert!(shown.iter().any(|s| s == home), "{shown:?}");
    assert!(
        shown.iter().any(|s| s == "delete wireless stw_9_9_5g"),
        "{shown:?}"
    );
    assert!(
        shown
            .iter()
            .any(|s| s == "set wireless radio0 (channel, htmode, country, txpower, disabled)"),
        "{shown:?}"
    );
    for s in &shown {
        assert!(
            !s.contains("correct horse") && !s.contains("Home") && !s.contains("GB"),
            "{s}"
        );
    }
}

/// A passphrase is printable: a newline would add a line to hostapd's config. SAE takes no
/// 64-hex key: the scripts would write it as a PSK and leave SAE without a password.
#[test]
fn keys_are_printable_and_sae_takes_a_passphrase() {
    let ssid = |proto: &str, key: &str| {
        json!({ "interfaces": [{ "ssids": [{ "name": "K", "wifi-bands": ["5G"],
                "encryption": { "proto": proto, "key": key } }] }] })
    };
    let hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    for (proto, key, runs) in [
        ("psk2", "correct horse\nwpa_psk=x", false),
        ("psk2", "correct\thorse", false),
        ("psk2", hex, true),
        ("sae", hex, false),
        ("sae-mixed", hex, false),
        ("sae", "correct horse battery", true),
    ] {
        let plan = wireless(&ssid(proto, key), &current());
        assert_eq!(find(&plan, "stw_0_0_5g").is_some(), runs, "{proto} {key:?}");
        assert!(
            !serde_json::to_string(&plan.rejected)
                .unwrap()
                .contains("horse")
        );
    }
}

/// What the protocol runs, against what was asked: a difference is a substitution.
#[test]
fn mfp_follows_the_protocol_and_says_so() {
    let ssid = |proto: &str, mfp: &str| {
        let mut enc = json!({ "proto": proto, "ieee80211w": mfp });
        if proto.starts_with("psk") || proto.starts_with("sae") {
            enc["key"] = json!("correct horse battery");
        }
        json!({ "interfaces": [{ "ssids": [{ "name": "M", "wifi-bands": ["5G"], "encryption": enc }] }] })
    };
    for (proto, asked, runs, substitution) in [
        ("sae", "optional", "2", Some("required")),
        ("sae", "required", "2", None),
        ("owe", "disabled", "2", Some("required")),
        ("sae-mixed", "disabled", "1", Some("optional")),
        ("sae-mixed", "required", "2", None),
        ("psk", "required", "0", Some("disabled")),
        ("none", "optional", "0", Some("disabled")),
        ("psk2", "optional", "1", None),
        ("psk-mixed", "required", "2", None),
    ] {
        let plan = wireless(&ssid(proto, asked), &current());
        assert_eq!(
            values(find(&plan, "stw_0_0_5g").unwrap())["ieee80211w"],
            runs,
            "{proto}/{asked}"
        );
        let subs: Vec<Value> = plan
            .rejected
            .iter()
            .filter_map(|r| r.substitution.clone())
            .collect();
        assert_eq!(
            subs,
            substitution
                .map(|s| json!(s))
                .into_iter()
                .collect::<Vec<_>>(),
            "{proto}/{asked}"
        );
    }
}
