use serde_json::{Map, Value, json};
use steward_render::{MARKER, Op, Plan, Wireless, wireless};

/// Three radios, and one SSID the agent made before for RADIUS (`stw_0_0_5g`).
fn current() -> Wireless {
    let answer = json!({ "values": {
        "radio0": { ".type": "wifi-device", "band": "2g" },
        "radio1": { ".type": "wifi-device", "band": "5g" },
        "radio2": { ".type": "wifi-device", "band": "6g" },
        "stw_0_0_5g": { ".type": "wifi-iface", ".name": "stw_0_0_5g", "device": "radio1",
                        "ssid": "Corp", "encryption": "wpa2", "auth_server": "192.0.2.10",
                        "auth_port": "1812", "auth_secret": "old-secret", "nasid": "ap-1",
                        MARKER: "1" },
    }});
    Wireless::from_uci(answer.as_object().unwrap())
}

fn ssid(v: Value) -> Value {
    json!({ "interfaces": [{ "name": "LAN", "ssids": [v] }] })
}

fn find<'a>(plan: &'a Plan, section: &str) -> Option<&'a Map<String, Value>> {
    plan.ops.iter().find_map(|op| match op {
        Op::Add { name, values, .. } if name == section => Some(values),
        Op::Set {
            section: s, values, ..
        } if s == section => Some(values),
        _ => None,
    })
}

fn reasons(plan: &Plan) -> String {
    plan.rejected
        .iter()
        .map(|r| format!("{} {} ", r.parameter, r.reason))
        .collect()
}

const SECRETS: [&str; 4] = ["s3cret-auth", "s3cret-acct", "s3cret-dae", "s3cret-other"];

fn enterprise() -> Value {
    json!({
        "name": "Corp", "wifi-bands": ["5G"],
        "encryption": { "proto": "wpa2", "ieee80211w": "optional", "eap-reauth-period": 7200,
                        "key-caching": false },
        "radius": {
            "nas-identifier": "ap-lobby",
            "chargeable-user-id": true,
            "authentication": {
                "host": "192.0.2.10", "port": 1812, "secret": "s3cret-auth",
                "secondary": { "host": "192.0.2.11", "port": 1812, "secret": "s3cret-auth" },
                "request-attribute": [{ "id": 32, "value": "lobby" }, { "id": 27, "value": 900 }]
            },
            "accounting": {
                "host": "192.0.2.12", "port": 1813, "secret": "s3cret-acct", "interval": 120,
                "secondary": { "host": "192.0.2.13", "port": 1814, "secret": "s3cret-other" }
            },
            "dynamic-authorization": { "host": "192.0.2.10", "port": 3799, "secret": "s3cret-dae" }
        }
    })
}

#[test]
fn an_enterprise_ssid_gets_its_radius_servers() {
    let plan = wireless(&ssid(enterprise()), &current());
    let v = find(&plan, "stw_0_0_5g").expect("the 5 GHz section");
    assert_eq!(v["encryption"], "wpa2");
    assert_eq!(v["ieee80211w"], "1");
    // A secondary with the same port and secret is a second address.
    assert_eq!(v["auth_server"], json!(["192.0.2.10", "192.0.2.11"]));
    assert_eq!(
        (&v["auth_port"], &v["auth_secret"]),
        (&json!("1812"), &json!("s3cret-auth"))
    );
    assert_eq!(v["radius_auth_req_attr"], json!(["32:s:lobby", "27:d:900"]));
    // One with its own port and secret can't be, and says so.
    assert_eq!(v["acct_server"], "192.0.2.12");
    assert_eq!(
        (&v["acct_port"], &v["acct_interval"]),
        (&json!("1813"), &json!("120"))
    );
    assert!(
        reasons(&plan).contains("accounting/secondary"),
        "{}",
        reasons(&plan)
    );
    assert_eq!(v["nasid"], "ap-lobby");
    assert_eq!(v["request_cui"], "1");
    assert_eq!(
        (&v["dae_client"], &v["dae_port"], &v["dae_secret"]),
        (&json!("192.0.2.10"), &json!("3799"), &json!("s3cret-dae"))
    );
    assert_eq!(v["eap_reauth_period"], "7200");
    assert_eq!(v["auth_cache"], "0");
    assert!(!v.contains_key("key"));
    assert_eq!(plan.rejected.len(), 1, "{}", reasons(&plan));
}

#[test]
fn every_enterprise_mode_runs_and_wpa3_requires_mfp() {
    for (proto, mfp) in [
        ("wpa", "0"),
        ("wpa2", "0"),
        ("wpa-mixed", "0"),
        ("wpa3", "2"),
        ("wpa3-mixed", "1"),
        ("wpa3-192", "2"),
    ] {
        let mut s = enterprise();
        s["encryption"] = json!({ "proto": proto });
        let plan = wireless(&ssid(s), &current());
        let v = find(&plan, "stw_0_0_5g").unwrap_or_else(|| panic!("{proto}"));
        assert_eq!(
            (v["encryption"].as_str(), v["ieee80211w"].as_str()),
            (Some(proto), Some(mfp))
        );
        // Key caching is uCentral's default.
        assert_eq!(v["auth_cache"], "1", "{proto}");
    }
}

#[test]
fn the_transition_modes_keep_mfp_on() {
    let s = json!({ "name": "Home", "wifi-bands": ["5G"],
                    "encryption": { "proto": "sae-mixed", "key": "correct horse battery" } });
    let plan = wireless(&ssid(s), &current());
    assert_eq!(find(&plan, "stw_0_0_5g").unwrap()["ieee80211w"], "1");
    let s = json!({ "name": "Home", "wifi-bands": ["5G"],
                    "encryption": { "proto": "sae-mixed", "key": "correct horse battery",
                                    "ieee80211w": "required" } });
    let plan = wireless(&ssid(s), &current());
    assert_eq!(find(&plan, "stw_0_0_5g").unwrap()["ieee80211w"], "2");
}

#[test]
fn mac_authentication_and_accounting_work_on_psk_networks() {
    let base = json!({ "name": "Shop", "wifi-bands": ["2G"],
                       "encryption": { "proto": "psk2", "key": "correct horse battery" },
                       "radius": { "authentication": { "host": "192.0.2.10", "port": 1812,
                                                       "secret": "s3cret-auth" } } });
    // Without mac-filter, a PSK network doesn't ask the server, and says so.
    let plan = wireless(&ssid(base.clone()), &current());
    let v = find(&plan, "stw_0_0_2g").unwrap();
    assert!(!v.contains_key("auth_server") && !v.contains_key("auth_cache"));
    assert!(reasons(&plan).contains("only for MAC authentication"));
    let mut with = base.clone();
    with["radius"]["authentication"]["mac-filter"] = json!(true);
    with["radius"]["accounting"] =
        json!({ "host": "192.0.2.12", "port": 1813, "secret": "s3cret-acct" });
    let plan = wireless(&ssid(with), &current());
    let v = find(&plan, "stw_0_0_2g").unwrap();
    assert_eq!(
        (v["encryption"].as_str(), v["auth_server"].as_str()),
        (Some("psk2"), Some("192.0.2.10"))
    );
    assert_eq!(v["acct_server"], "192.0.2.12");
    assert_eq!(v["key"], "correct horse battery");
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

#[test]
fn six_ghz_runs_only_wpa3_and_owe() {
    let s = json!({ "name": "Home", "wifi-bands": ["5G", "6G"],
                    "encryption": { "proto": "sae-mixed", "key": "correct horse battery" } });
    let plan = wireless(&ssid(s), &current());
    assert_eq!(
        find(&plan, "stw_0_0_5g").unwrap()["encryption"],
        "sae-mixed"
    );
    let six = find(&plan, "stw_0_0_6g").unwrap();
    assert_eq!(
        (&six["encryption"], &six["ieee80211w"]),
        (&json!("sae"), &json!("2"))
    );
    let r = &plan.rejected[0];
    assert_eq!(r.substitution, Some(json!("sae")), "a substitution, listed");
    let mut s = enterprise();
    s["wifi-bands"] = json!(["5G", "6G"]);
    s["encryption"] = json!({ "proto": "wpa3-mixed" });
    let plan = wireless(&ssid(s), &current());
    assert_eq!(find(&plan, "stw_0_0_6g").unwrap()["encryption"], "wpa3");
    // WPA2 can't run there: refused for that band only.
    let s = json!({ "name": "Old", "wifi-bands": ["5G", "6G"],
                    "encryption": { "proto": "psk2", "key": "correct horse battery" } });
    let plan = wireless(&ssid(s), &current());
    assert!(find(&plan, "stw_0_0_5g").is_some() && find(&plan, "stw_0_0_6g").is_none());
    assert!(
        reasons(&plan).contains("wifi-bands/1"),
        "{}",
        reasons(&plan)
    );
}

#[test]
fn what_hostapd_cant_do_is_refused_without_its_secrets() {
    let cases = [
        (
            json!({ "radius": { "authentication": { "host": "radius.example.com", "port": 1812,
                                                 "secret": "s3cret-auth" } } }),
            "address, not its name",
            false,
        ),
        (
            json!({ "radius": { "authentication": { "host": "192.0.2.10", "port": 1812 } } }),
            "a host, a port and a secret",
            false,
        ),
        (
            json!({ "radius": {} }),
            "needs a RADIUS authentication server",
            false,
        ),
        (
            json!({ "radius": { "local": { "users": [{ "user-name": "a", "password": "s3cret-other" }] } } }),
            "built-in EAP server",
            false,
        ),
        (
            json!({ "radius": { "authentication": { "host": "192.0.2.10", "port": 1812,
                                                 "secret": "s3cret-auth\nbss=evil" } } }),
            "control characters",
            false,
        ),
        (
            json!({ "radius": { "authentication": { "host": "192.0.2.10", "port": 1812,
                                                 "secret": "s3cret-auth" },
                             "nas-identifier": "two\nlines" } }),
            "NAS identifier",
            true,
        ),
        (
            json!({ "certificates": { "private-key": "s3cret-other" },
                 "radius": { "authentication": { "host": "192.0.2.10", "port": 1812,
                                                 "secret": "s3cret-auth" } } }),
            "certificates are only",
            true,
        ),
    ];
    for (extra, reason, runs) in cases {
        let mut s = enterprise();
        s["radius"] = extra["radius"].clone();
        if let Some(c) = extra.get("certificates") {
            s["certificates"] = c.clone();
        }
        let plan = wireless(&ssid(s), &current());
        let got = reasons(&plan);
        assert!(got.contains(reason), "{reason}: {got}");
        assert_eq!(find(&plan, "stw_0_0_5g").is_some(), runs, "{reason}");
        for secret in SECRETS {
            assert!(!got.contains(secret), "{secret} in {got}");
        }
    }
    for proto in ["psk2-radius", "mpsk-radius"] {
        let s = json!({ "name": "PPSK", "wifi-bands": ["5G"], "encryption": { "proto": proto } });
        let plan = wireless(&ssid(s), &current());
        assert!(reasons(&plan).contains("private PSK"), "{}", reasons(&plan));
    }
    let s = json!({ "name": "Bad", "wifi-bands": ["5G"],
                    "encryption": { "proto": "psk2", "key": "twelve chars\n" } });
    assert!(reasons(&wireless(&ssid(s), &current())).contains("printable"));
}

#[test]
fn a_network_that_stops_using_radius_loses_its_options() {
    let s = json!({ "name": "Corp", "wifi-bands": ["5G"],
                    "encryption": { "proto": "psk2", "key": "correct horse battery" } });
    let plan = wireless(&ssid(s), &current());
    let unset = plan.ops.iter().find_map(|op| match op {
        Op::Unset {
            section, options, ..
        } if section == "stw_0_0_5g" => Some(options.clone()),
        _ => None,
    });
    let mut unset = unset.expect("the old RADIUS options are removed");
    unset.sort();
    assert_eq!(unset, ["auth_port", "auth_secret", "auth_server", "nasid"]);
}

/// Every option written exists in the wifi scripts (tests/wireless-schema.json).
#[test]
fn every_security_option_exists_in_the_wifi_scripts_schema() {
    let schema: Value = serde_json::from_str(include_str!("wireless-schema.json")).unwrap();
    let known = schema["wifi-iface"].as_array().unwrap();
    let plan = wireless(&ssid(enterprise()), &current());
    let mut psk = json!({ "name": "Shop", "wifi-bands": ["2G"],
                          "encryption": { "proto": "psk2", "key": "correct horse battery" },
                          "radius": { "authentication": { "host": "192.0.2.10", "port": 1812,
                                                          "secret": "s3cret-auth", "mac-filter": true } } });
    psk["radius"]["accounting"] = json!({ "host": "192.0.2.12", "port": 1813, "secret": "x" });
    let psk_plan = wireless(&ssid(psk), &current());
    for op in plan.ops.iter().chain(&psk_plan.ops) {
        if let Op::Add { values, .. } | Op::Set { values, .. } = op {
            for option in values.keys().filter(|k| k.as_str() != MARKER) {
                assert!(known.iter().any(|o| o == option), "wifi-iface.{option}");
            }
        }
    }
}
