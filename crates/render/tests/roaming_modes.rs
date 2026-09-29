//! Roaming on every mode, the derived EAP key, key holders hostapd takes, and steering against
//! each state of usteer: an end-to-end review's probes for STW-23, kept as tests.
use serde_json::{Map, Value, json};
use steward_render::{Op, Plan, Usteer, Wireless, wireless};

const MODES: [&str; 13] = [
    "none",
    "owe",
    "psk",
    "psk2",
    "psk-mixed",
    "sae",
    "sae-mixed",
    "wpa",
    "wpa2",
    "wpa-mixed",
    "wpa3",
    "wpa3-mixed",
    "wpa3-192",
];
const KEY: &str = "ZZKEYcorrecthorse";
const AUTH: &str = "ZZSECRETAUTH";

fn radios(extra: Value) -> Wireless {
    let mut values = json!({
        "radio0": { ".type": "wifi-device", "band": "2g" },
        "radio1": { ".type": "wifi-device", "band": "5g" },
        "radio2": { ".type": "wifi-device", "band": "6g" },
    });
    for (k, v) in extra.as_object().unwrap() {
        values[k] = v.clone();
    }
    Wireless::from_uci(json!({ "values": values }).as_object().unwrap())
}

fn one(v: Value) -> Value {
    json!({ "interfaces": [{ "name": "LAN", "ssids": [v] }] })
}

/// An SSID in `mode`, with a key or a RADIUS server as the mode needs.
fn ssid(mode: &str, bands: &[&str]) -> Value {
    let mut s = json!({ "name": "Probe", "wifi-bands": bands, "encryption": { "proto": mode } });
    if matches!(mode, "psk" | "psk2" | "psk-mixed" | "sae" | "sae-mixed") {
        s["encryption"]["key"] = json!(KEY);
    }
    if mode.starts_with("wpa") {
        s["radius"] =
            json!({ "authentication": { "host": "192.0.2.10", "port": 1812, "secret": AUTH } });
    }
    s
}

fn section<'a>(plan: &'a Plan, name: &str) -> Option<&'a Map<String, Value>> {
    plan.ops.iter().find_map(|op| match op {
        Op::Add {
            name: n, values, ..
        } if n == name => Some(values),
        Op::Set {
            section, values, ..
        } if section == name => Some(values),
        _ => None,
    })
}

fn reasons(plan: &Plan) -> String {
    plan.rejected
        .iter()
        .map(|r| format!("{} {} {:?}\n", r.parameter, r.reason, r.substitution))
        .collect()
}

#[test]
fn roaming_on_every_mode() {
    for mode in MODES {
        let mut s = ssid(mode, &["2G", "5G", "6G"]);
        s["roaming"] = json!(true);
        let plan = wireless(&one(s), &radios(json!({})));
        let roams = !matches!(mode, "none" | "owe" | "psk" | "wpa");
        let v = section(&plan, "stw_0_0_5g").unwrap();
        assert_eq!(v.contains_key("ieee80211r"), roams, "{mode}");
        assert_eq!(
            reasons(&plan).contains("fast roaming needs WPA2"),
            !roams,
            "{mode}"
        );
        if roams {
            // EAP gets Steward's key holders (the scripts can't derive them); a key's SSID
            // leaves them to the scripts.
            assert_eq!(v.contains_key("r0kh"), mode.starts_with("wpa"), "{mode}");
            assert_eq!(
                v.contains_key("ft_psk_generate_local"),
                matches!(mode, "psk2" | "psk-mixed"),
                "{mode}"
            );
            // The same roaming options on every band it runs on.
            for band in ["2g", "6g"] {
                if let Some(o) = section(&plan, &format!("stw_0_0_{band}")) {
                    for k in ["ieee80211r", "r0kh", "r1kh", "ft_over_ds"] {
                        assert_eq!(o.get(k), v.get(k), "{mode} {band} {k}");
                    }
                }
            }
        }
    }
}

#[test]
fn the_derived_eap_key_is_bound_to_the_ssid_and_secret_not_the_mobility_domain() {
    let key = |name: &str, secret: &str, domain: Option<&str>| {
        let mut s = ssid("wpa2", &["5G"]);
        s["name"] = json!(name);
        s["radius"]["authentication"]["secret"] = json!(secret);
        s["roaming"] = match domain {
            Some(d) => json!({ "domain-identifier": d }),
            None => json!(true),
        };
        let plan = wireless(&one(s), &radios(json!({})));
        section(&plan, "stw_0_0_5g").unwrap()["r0kh"].clone()
    };
    let base = key("Corp", AUTH, None);
    assert!(!base.to_string().contains("ZZ"));
    assert_eq!(
        base,
        key("Corp", AUTH, Some("abcd")),
        "domain doesn't change it"
    );
    assert_ne!(base, key("Corp2", AUTH, None));
    assert_ne!(base, key("Corp", "another", None));
}

#[test]
fn steering_follows_usteer() {
    let mut s = ssid("psk2", &["5G"]);
    s["services"] = json!(["wifi-steering"]);
    let with = |u: Option<Usteer>| {
        let mut w = radios(json!({}));
        w.usteer = u;
        wireless(&one(s.clone()), &w)
    };
    let listed = |l: &[&str]| {
        Some(Usteer {
            running: true,
            ssid_list: Some(l.iter().map(|s| s.to_string()).collect()),
        })
    };
    assert!(
        with(Some(Usteer {
            running: true,
            ssid_list: None
        }))
        .rejected
        .is_empty()
    );
    assert!(with(listed(&["Other", "Probe"])).rejected.is_empty());
    for (u, reason) in [
        (None, "isn't installed"),
        (Some(Usteer::default()), "isn't running"),
        (listed(&["Other"]), "ssid_list"),
        // usteer compares names exactly.
        (listed(&["probe"]), "ssid_list"),
    ] {
        let plan = with(u);
        assert!(reasons(&plan).contains(reason), "{reason}");
        let v = section(&plan, "stw_0_0_5g").unwrap();
        assert_eq!(
            (&v["bss_transition"], &v["ieee80211k"]),
            (&json!("1"), &json!("1"))
        );
    }
    // An empty ssid_list from the daemon means every SSID.
    let empty = json!({ "ssid_list": [] });
    assert_eq!(Usteer::from_config(empty.as_object()).ssid_list, None);
}

#[test]
fn an_r1_key_holder_id_is_a_mac() {
    let mut s = ssid("psk2", &["5G"]);
    s["roaming"] = json!({
        "pmk-r0-key-holder": "00:00:5E:00:53:01,00005E005301,00112233445566778899aabbccddeeff",
        "pmk-r1-key-holder": "00:00:5E:00:53:01,00005E005301,00112233445566778899aabbccddeeff" });
    let plan = wireless(&one(s), &radios(json!({})));
    assert!(reasons(&plan).contains("key holder"), "accepted");
}

#[test]
fn an_r0_key_holder_id_is_at_most_48_octets() {
    let mut s = ssid("psk2", &["5G"]);
    s["roaming"] = json!({
        "pmk-r0-key-holder": format!("00:00:5e:00:53:01,{},00112233445566778899aabbccddeeff", "n".repeat(60)),
        "pmk-r1-key-holder": "00:00:5e:00:53:01,00:00:5e:00:53:01,00112233445566778899aabbccddeeff" });
    let plan = wireless(&one(s), &radios(json!({})));
    assert!(reasons(&plan).contains("key holder"), "accepted");
}

/// Roaming fields holding secrets and control characters.
fn hostile() -> Vec<Value> {
    vec![
        json!({ "name": "I", "wifi-bands": ["5G"], "encryption": { "proto": "psk2", "key": KEY },
                "roaming": { "key-aes-256": "ZZI", "pmk-r0-key-holder": "ZZJ", "pmk-r1-key-holder": "ZZK" } }),
        json!({ "name": "J", "wifi-bands": ["5G"], "encryption": { "proto": "psk2", "key": KEY },
                "roaming": { "pmk-r0-key-holder": "00:00:5e:00:53:01,ap\t1,ZZ112233445566778899aabbccddeeff",
                             "pmk-r1-key-holder": "00:00:5e:00:53:01,00:00:5e:00:53:01,ZZ112233445566778899aabbccddeeff" },
                "rrm": { "lci": "0\n1", "civic-location": "zz" } }),
    ]
}

#[test]
fn no_roaming_secret_in_any_rejection_or_control_character_in_an_option() {
    let config = json!({ "interfaces": [{ "name": "LAN", "ssids": hostile() }] });
    let plan = wireless(&config, &radios(json!({})));
    let got = reasons(&plan);
    assert!(plan.rejected.len() >= 3, "{got}");
    assert!(!got.contains("ZZ"), "{got}");
    for op in &plan.ops {
        if let Op::Add { values, .. } | Op::Set { values, .. } = op {
            for (k, v) in values {
                assert!(
                    !v.to_string().contains("\\n") && !v.to_string().contains("\\t"),
                    "{k}: {v}"
                );
            }
        }
    }
}

fn steered() -> Wireless {
    let mut w = radios(json!({}));
    w.usteer = Some(Usteer {
        running: true,
        ssid_list: None,
    });
    w
}

/// Everything STW-22 and STW-23 read, on one enterprise SSID.
fn everything() -> Value {
    json!({ "name": "Corp", "wifi-bands": ["2G", "5G", "6G"],
            "encryption": { "proto": "wpa3-mixed", "ieee80211w": "optional",
                            "eap-reauth-period": 3600, "key-caching": false },
            "radius": { "nas-identifier": "corp-nas", "chargeable-user-id": true,
                        "authentication": { "host": "192.0.2.10", "port": 1812, "secret": AUTH },
                        "accounting": { "host": "192.0.2.12", "port": 1813,
                                        "secret": "ZZSECRETACCT", "interval": 120 } },
            "roaming": { "message-exchange": "ds", "domain-identifier": "a1b2",
                         "generate-psk": false },
            "rrm": { "neighbor-reporting": true, "reduced-neighbor-reporting": true,
                     "ftm-responder": true, "lci": "0102", "civic-location": "abcd" },
            "services": ["wifi-steering"] })
}

#[test]
fn keys_it_doesnt_read_are_unsupported_and_the_ones_it_reads_never_are() {
    let plan = wireless(&one(everything()), &steered());
    // Only the 6 GHz substitutions: wpa3-mixed runs as wpa3 there, with MFP required.
    assert_eq!(plan.rejected.len(), 2, "{}", reasons(&plan));
    let subs: Vec<_> = plan
        .rejected
        .iter()
        .map(|r| r.substitution.clone())
        .collect();
    assert_eq!(subs, [Some(json!("wpa3")), Some(json!("required"))]);
    let v = section(&plan, "stw_0_0_5g").unwrap();
    for (k, want) in [
        ("ieee80211r", json!("1")),
        ("ft_over_ds", json!("1")),
        ("mobility_domain", json!("a1b2")),
        ("ieee80211k", json!("1")),
        ("rnr", json!("1")),
        ("ftm_responder", json!("1")),
        ("lci", json!("0102")),
        ("civic", json!("abcd")),
        ("bss_transition", json!("1")),
    ] {
        assert_eq!(v.get(k), Some(&want), "{k}");
    }
    for band in ["2g", "6g"] {
        let o = section(&plan, &format!("stw_0_0_{band}")).unwrap();
        assert_eq!((&o["r0kh"], &o["r1kh"]), (&v["r0kh"], &v["r1kh"]), "{band}");
    }
    for object in ["roaming", "rrm"] {
        let mut s = everything();
        s[object]["unknown"] = json!(true);
        s[object]["pmk-r2-key-holder"] = json!("ZZSECRETHOLDER");
        let plan = wireless(&one(s), &steered());
        let got = reasons(&plan);
        for key in ["unknown", "pmk-r2-key-holder"] {
            assert!(
                got.contains(&format!("{object}/{key}\""))
                    && got.contains(&format!("{key} isn't supported yet")),
                "{object}: {got}"
            );
        }
        assert!(!got.contains("ZZ"), "{got}");
        // The two keys, and the 6 GHz substitutions (the protocol, and MFP with it).
        assert_eq!(plan.rejected.len(), 4, "{object}: {got}");
    }
}

/// A value of the wrong kind is rejected, never guessed at: the SSID runs with the default.
#[test]
fn values_of_the_wrong_kind_are_rejected_not_guessed() {
    let with = |key: &str, v: Value| {
        let mut s = ssid("psk2", &["5G"]);
        s[key] = v;
        s
    };
    for (s, absent) in [
        (
            with("roaming", json!({ "message-exchange": 1 })),
            "mobility_domain",
        ),
        (with("roaming", json!({ "generate-psk": "yes" })), "r0kh"),
        (with("rrm", json!(true)), "ieee80211k"),
        (
            with("rrm", json!({ "neighbor-reporting": "yes" })),
            "ieee80211k",
        ),
        (with("rrm", json!({ "ftm-responder": 1 })), "ftm_responder"),
        (with("rrm", json!({ "lci": 102 })), "lci"),
    ] {
        let plan = wireless(&one(s.clone()), &steered());
        let v = section(&plan, "stw_0_0_5g").unwrap();
        assert!(!plan.rejected.is_empty(), "accepted: {s}");
        assert!(!v.contains_key(absent), "{s}");
        if s.get("roaming").is_some() {
            assert_eq!(
                (&v["ft_over_ds"], &v["ft_psk_generate_local"]),
                (&json!("0"), &json!("0"))
            );
        }
    }
}

/// hostapd reads its config in 4096-byte lines: a longer lci= splits into an invalid line and
/// fails the radio. A measurement subelement holds 255 bytes at most.
#[test]
fn lci_and_civic_location_are_1_to_255_bytes() {
    for (value, ok) in [
        ("ab".to_string(), true),
        ("ab".repeat(255), true),
        ("ab".repeat(256), false),
        ("ab".repeat(2100), false),
        (String::new(), false),
        ("abc".to_string(), false),
        ("zz".to_string(), false),
    ] {
        for (key, option) in [("lci", "lci"), ("civic-location", "civic")] {
            let mut s = ssid("psk2", &["5G"]);
            s["rrm"] = json!({ "ftm-responder": true, key: value });
            let plan = wireless(&one(s), &steered());
            let v = section(&plan, "stw_0_0_5g").unwrap();
            assert_eq!(v.contains_key(option), ok, "{key} of {}", value.len());
            assert_eq!(
                reasons(&plan).contains("1 to 255 bytes"),
                !ok,
                "{key} of {}",
                value.len()
            );
        }
    }
}

#[test]
fn services_other_than_steering_are_refused() {
    for (services, steers, refused) in [
        (
            json!(["wifi-steering", "captive"]),
            true,
            &["services/1"][..],
        ),
        (json!(["captive"]), false, &["services/0"]),
        (json!([5, "wifi-steering"]), true, &["services/0"]),
        (json!("wifi-steering"), false, &["services\""]),
        (json!({ "wifi-steering": true }), false, &["services\""]),
    ] {
        let mut s = ssid("psk2", &["5G"]);
        s["services"] = services.clone();
        let plan = wireless(&one(s), &steered());
        let v = section(&plan, "stw_0_0_5g").unwrap();
        assert_eq!(v.contains_key("bss_transition"), steers, "{services}");
        let got = reasons(&plan);
        assert_eq!(plan.rejected.len(), refused.len(), "{services}: {got}");
        for at in refused {
            assert!(got.contains(at), "{services}: {got}");
        }
    }
}

#[test]
fn key_holders_beside_a_shared_key_are_refused() {
    let key = "00112233445566778899aabbccddeeff";
    let holders = json!({
        "pmk-r0-key-holder": format!("00:00:5e:00:53:01,ap1,{key}"),
        "pmk-r1-key-holder": format!("00:00:5e:00:53:01,00:00:5e:00:53:01,{key}") });
    let mut s = ssid("psk2", &["5G"]);
    s["roaming"] = holders.clone();
    s["roaming"]["key-aes-256"] = json!(key.repeat(2));
    let plan = wireless(&one(s), &steered());
    let got = reasons(&plan);
    assert_eq!(plan.rejected.len(), 2, "{got}");
    assert!(got.contains("not both") && !got.contains(key), "{got}");
    // The shared key's holders stand.
    let v = section(&plan, "stw_0_0_5g").unwrap();
    assert_eq!(
        v["r0kh"],
        json!([format!("ff:ff:ff:ff:ff:ff,*,{}", key.repeat(2))])
    );
    // A shared key that's refused leaves an enterprise SSID its derived holders: without any,
    // the scripts fail the whole radio (FT_KEY_CANT_BE_DERIVED).
    let mut s = ssid("wpa2", &["5G"]);
    s["roaming"] = json!({ "key-aes-256": "ZZSHORT" });
    let plan = wireless(&one(s), &steered());
    let got = reasons(&plan);
    assert!(
        got.contains("64 hex digits") && !got.contains("ZZ"),
        "{got}"
    );
    let r0 = section(&plan, "stw_0_0_5g").unwrap()["r0kh"][0].to_string();
    assert!(r0.starts_with("\"ff:ff:ff:ff:ff:ff,*,"), "{r0}");
}
