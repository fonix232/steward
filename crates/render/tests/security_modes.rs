//! Every encryption mode against the band, MFP and RADIUS rules, and what nothing may leak: an
//! end-to-end review's probes for STW-22, kept as tests.
use serde_json::{Map, Value, json};
use steward_render::{Op, Plan, Wireless, wireless};

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
fn six_ghz_runs_only_wpa3_and_owe_for_every_mode() {
    for mode in MODES {
        let plan = wireless(&one(ssid(mode, &["6G"])), &radios(json!({})));
        let v = section(&plan, "stw_0_0_6g");
        let want = match mode {
            "owe" | "sae" | "wpa3" | "wpa3-192" => Some((mode, None)),
            "sae-mixed" => Some(("sae", Some(json!("sae")))),
            "wpa3-mixed" => Some(("wpa3", Some(json!("wpa3")))),
            _ => None,
        };
        match want {
            Some((enc, substitution)) => {
                let v = v.unwrap_or_else(|| panic!("{mode}: {}", reasons(&plan)));
                assert_eq!(v["encryption"], enc, "{mode}");
                assert_eq!(v["ieee80211w"], "2", "{mode}: 6 GHz needs MFP");
                assert_eq!(
                    plan.rejected.first().and_then(|r| r.substitution.clone()),
                    substitution,
                    "{mode}"
                );
            }
            None => {
                assert!(v.is_none(), "{mode} must not run on 6 GHz");
                assert!(reasons(&plan).contains("6 GHz allows only"), "{mode}");
            }
        }
    }
}

#[test]
fn mfp_matrix() {
    // (mode, configured) → ieee80211w written.
    for mode in MODES {
        for (configured, n) in [("disabled", 0), ("optional", 1), ("required", 2)] {
            let mut s = ssid(mode, &["5G"]);
            s["encryption"]["ieee80211w"] = json!(configured);
            let plan = wireless(&one(s), &radios(json!({})));
            let v = section(&plan, "stw_0_0_5g").unwrap();
            let want = match mode {
                "owe" | "sae" | "wpa3" | "wpa3-192" => 2,
                "sae-mixed" | "wpa3-mixed" => n.max(1),
                // Open and WPA1-only networks have no MFP: turned off, as a substitution.
                "none" | "psk" | "wpa" => 0,
                _ => n,
            };
            assert_eq!(v["ieee80211w"], want.to_string(), "{mode}/{configured}");
            // Whatever runs other than asked is a substitution, named in the answer.
            let subs: Vec<&Value> = plan
                .rejected
                .iter()
                .filter_map(|r| r.substitution.as_ref())
                .collect();
            if want == n {
                assert!(subs.is_empty(), "{mode}/{configured}: {subs:?}");
            } else {
                let named = ["disabled", "optional", "required"][want as usize];
                assert_eq!(subs, [&json!(named)], "{mode}/{configured}");
            }
        }
    }
}

#[test]
fn stale_options_follow_mode_changes() {
    // What an earlier configuration left: EAP with roaming, steering, RRM and RADIUS extras.
    let had = json!({ "stw_0_0_5g": {
        ".type": "wifi-iface", "steward": "1", "device": "radio1", "mode": "ap", "ssid": "Probe",
        "network": "lan", "hidden": "0", "isolate": "0", "disabled": "0",
        "encryption": "wpa2", "ieee80211w": "0", "auth_server": ["192.0.2.10", "192.0.2.11"],
        "auth_port": "1812", "auth_secret": "old", "acct_server": "192.0.2.12", "acct_port": "1813",
        "acct_secret": "old", "acct_interval": "60", "nasid": "n", "request_cui": "1",
        "dae_client": "192.0.2.10", "dae_port": "3799", "dae_secret": "old",
        "radius_auth_req_attr": ["32:s:x"], "eap_reauth_period": "60", "auth_cache": "1",
        "ieee80211r": "1", "ft_over_ds": "1", "mobility_domain": "abcd", "r0kh": ["x"],
        "r1kh": ["y"], "bss_transition": "1", "ieee80211k": "1", "rnr": "1", "lci": "01" } });
    let current = radios(had);
    let plan = wireless(&one(ssid("psk2", &["5G"])), &current);
    let unset: Vec<String> = plan
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Unset { options, .. } => Some(options.clone()),
            _ => None,
        })
        .expect("stale options are removed");
    for gone in [
        "auth_server",
        "auth_port",
        "auth_secret",
        "acct_server",
        "acct_secret",
        "acct_interval",
        "nasid",
        "request_cui",
        "dae_client",
        "dae_secret",
        "radius_auth_req_attr",
        "eap_reauth_period",
        "auth_cache",
        "ieee80211r",
        "ft_over_ds",
        "mobility_domain",
        "r0kh",
        "r1kh",
        "bss_transition",
        "ieee80211k",
        "rnr",
        "lci",
    ] {
        assert!(unset.iter().any(|o| o == gone), "{gone} stays: {unset:?}");
    }
    for kept in ["ssid", "encryption", "device", "steward", "ieee80211w"] {
        assert!(!unset.iter().any(|o| o == kept), "{kept} removed");
    }
    // And a PSK network that becomes enterprise loses its key.
    let had = json!({ "stw_0_0_5g": { ".type": "wifi-iface", "steward": "1", "ssid": "Probe",
                                       "encryption": "psk2", "key": "old-key-1234" } });
    let plan = wireless(&one(ssid("wpa2", &["5G"])), &radios(had));
    assert!(
        plan.ops.iter().any(
            |op| matches!(op, Op::Unset { options, .. } if options.iter().any(|o| o == "key"))
        )
    );
}

/// Every string field that can hold a secret or reach hostapd, filled with secrets and control
/// characters.
fn hostile() -> Vec<Value> {
    let srv = |secret: &str| json!({ "host": "192.0.2.10", "port": 1812, "secret": secret });
    vec![
        json!({ "name": "A", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" },
                "radius": { "authentication": { "host": "name.example", "port": 1812, "secret": "ZZ1" } } }),
        json!({ "name": "B", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" },
                "radius": { "authentication": { "host": "192.0.2.10", "port": 0, "secret": "ZZ2" } } }),
        json!({ "name": "C", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" },
                "radius": { "authentication": srv("ZZ3\n"),
                            "accounting": srv("ZZ4\r") } }),
        json!({ "name": "D", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" },
                "radius": { "authentication": { "host": "192.0.2.10", "port": 1812, "secret": "ZZ5",
                                                "secondary": { "host": "192.0.2.11", "port": 1813, "secret": "ZZ6" } },
                            "accounting": { "host": "192.0.2.12", "port": 1813, "secret": "ZZ7",
                                            "secondary": { "host": "x.example", "port": 1813, "secret": "ZZ7" } },
                            "dynamic-authorization": { "host": "x.example", "secret": "ZZ8" },
                            "nas-identifier": "a\u{7}b",
                            "local": { "users": [{ "user-name": "u", "password": "ZZ9" }] },
                            "health": { "secret": "ZZA", "password": "ZZB" } },
                "certificates": { "private-key": "ZZC", "private-key-password": "ZZD" } }),
        json!({ "name": "E", "wifi-bands": ["5G"], "encryption": { "proto": "psk2", "key": "ZZE\u{0}abcdef" } }),
        json!({ "name": "F", "wifi-bands": ["5G"], "encryption": { "proto": "psk2", "key": "ZZF" } }),
        json!({ "name": "G", "wifi-bands": ["5G"], "encryption": { "proto": "psk2-radius", "key": "ZZG12345" } }),
        json!({ "name": "H", "wifi-bands": ["6G"], "encryption": { "proto": "psk2", "key": "ZZH12345" } }),
        json!({ "name": "K", "wifi-bands": ["5G"], "encryption": { "proto": "wpa2" },
                "radius": { "authentication": { "host": "192.0.2.10", "port": 1812, "secret": "ZZL",
                                                "request-attribute": [{ "id": 1, "value": "x\ny" },
                                                                      { "id": 2, "hex-value": "0\n" },
                                                                      { "vendor-id": 9, "vendor-attributes": [{ "id": 1, "value": "\n" }] }] } } }),
    ]
}

#[test]
fn no_secret_in_any_rejection() {
    let config = json!({ "interfaces": [{ "name": "LAN", "ssids": hostile() }] });
    let plan = wireless(&config, &radios(json!({})));
    let got = reasons(&plan);
    assert!(plan.rejected.len() >= 15, "{got}");
    assert!(!got.contains("ZZ"), "{got}");
}

#[test]
fn no_control_character_in_any_option_but_the_ssid() {
    let mut ssids = hostile();
    // The SSID itself: accepted; the stock scripts write ssid2 hex-encoded (checked on bifrost).
    ssids.push(json!({ "name": "S\nwpa=0", "wifi-bands": ["5G"],
                       "encryption": { "proto": "psk2", "key": KEY } }));
    let config = json!({ "interfaces": [{ "name": "LAN", "ssids": ssids }] });
    let plan = wireless(&config, &radios(json!({})));
    let mut ssid_seen = false;
    for op in &plan.ops {
        if let Op::Add { values, .. } | Op::Set { values, .. } = op {
            for (k, v) in values {
                let control = match v {
                    Value::String(s) => s.chars().any(char::is_control),
                    Value::Array(a) => a
                        .iter()
                        .any(|s| s.as_str().is_some_and(|s| s.chars().any(char::is_control))),
                    _ => false,
                };
                if k == "ssid" {
                    ssid_seen |= control;
                } else {
                    assert!(!control, "{k}: {v}");
                }
            }
        }
    }
    assert!(
        ssid_seen,
        "SSID names aren't checked for control characters"
    );
}

#[test]
fn dae_on_a_non_eap_ssid_is_refused() {
    let s = json!({ "name": "Shop", "wifi-bands": ["5G"],
                    "encryption": { "proto": "psk2", "key": KEY },
                    "radius": { "authentication": { "host": "192.0.2.10", "port": 1812, "secret": AUTH,
                                                    "mac-filter": true },
                                "dynamic-authorization": { "host": "192.0.2.10", "secret": "ZZDAE" } } });
    let plan = wireless(&one(s), &radios(json!({})));
    let v = section(&plan, "stw_0_0_5g").unwrap();
    assert!(
        !v.contains_key("dae_client") && reasons(&plan).contains("dynamic-authorization"),
        "dae_client written on a PSK SSID without a rejection"
    );
}

#[test]
fn radius_extras_without_a_server_are_refused() {
    let s = json!({ "name": "Open", "wifi-bands": ["5G"], "encryption": { "proto": "none" },
                    "radius": { "nas-identifier": "n", "chargeable-user-id": true,
                                "dynamic-authorization": { "host": "192.0.2.10", "secret": "ZZDAE" } } });
    let plan = wireless(&one(s), &radios(json!({})));
    assert!(
        !plan.rejected.is_empty(),
        "nothing rejected, nothing written"
    );
}

#[test]
fn mfp_on_wpa1_or_open_is_refused() {
    for mode in ["none", "psk", "wpa"] {
        let mut s = ssid(mode, &["5G"]);
        s["encryption"]["ieee80211w"] = json!("required");
        let plan = wireless(&one(s), &radios(json!({})));
        assert!(!plan.rejected.is_empty(), "{mode}: accepted");
    }
}

#[test]
fn sae_refuses_a_hex_key() {
    for mode in ["sae", "sae-mixed"] {
        let mut s = ssid(mode, &["5G"]);
        s["encryption"]["key"] = json!("ab".repeat(32));
        let plan = wireless(&one(s), &radios(json!({})));
        assert!(!plan.rejected.is_empty(), "{mode}: accepted");
    }
}

fn enterprise_with(attribute: Value) -> Value {
    let mut s = ssid("wpa2", &["5G"]);
    s["radius"]["authentication"]["request-attribute"] = json!([attribute]);
    s
}

/// A RADIUS attribute's value is 253 bytes at most: hostapd fails every request a longer one
/// would go in.
#[test]
fn request_attributes_fit_in_a_radius_attribute() {
    for a in [
        json!({ "id": 32, "value": "t".repeat(254) }),
        json!({ "id": 30, "hex-value": "ab".repeat(254) }),
        json!({ "vendor-id": 9, "vendor-attributes": [{ "id": 1, "value": "ab".repeat(248) }] }),
        json!({ "vendor-id": 9, "vendor-attributes": [{ "id": 1, "value": "ab".repeat(130) },
                                                      { "id": 2, "value": "cd".repeat(130) }] }),
    ] {
        let plan = wireless(&one(enterprise_with(a.clone())), &radios(json!({})));
        let v = section(&plan, "stw_0_0_5g").unwrap();
        assert!(
            !v.contains_key("radius_auth_req_attr") && !plan.rejected.is_empty(),
            "accepted: {}",
            &a.to_string()[..60]
        );
    }
    // At the limit: 253 bytes of text, and a vendor attribute of 4 + 2 + 247.
    for a in [
        json!({ "id": 32, "value": "t".repeat(253) }),
        json!({ "vendor-id": 9, "vendor-attributes": [{ "id": 1, "value": "ab".repeat(247) }] }),
    ] {
        let plan = wireless(&one(enterprise_with(a)), &radios(json!({})));
        assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
    }
}

#[test]
fn a_default_false_needs_no_server() {
    let s = json!({ "name": "Open", "wifi-bands": ["5G"], "encryption": { "proto": "none" },
                    "radius": { "chargeable-user-id": false } });
    assert!(wireless(&one(s), &radios(json!({}))).rejected.is_empty());
}

#[test]
fn dae_stays_on_enterprise_ssids_with_its_default_port() {
    let mut s = ssid("wpa3", &["5G"]);
    s["radius"]["dynamic-authorization"] = json!({ "host": "192.0.2.10", "secret": "ZZDAE" });
    let plan = wireless(&one(s), &radios(json!({})));
    let v = section(&plan, "stw_0_0_5g").unwrap();
    assert_eq!(
        (&v["dae_client"], &v["dae_port"]),
        (&json!("192.0.2.10"), &json!("3799"))
    );
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
}

/// hostapd reads eap_reauth_period with atoi: from 2^31 it turns negative ("invalid period")
/// and the radio fails. Only the schema's 0 to 86400 is written.
#[test]
fn eap_reauth_period_is_0_to_86400() {
    for (p, ok) in [
        (json!(0), true),
        (json!(86400), true),
        (json!(86401), false),
        (json!(2147483648u64), false),
        (json!(u64::MAX), false),
        (json!(-1), false),
        (json!(600.5), false),
        (json!("600"), false),
    ] {
        let mut s = ssid("wpa2", &["5G"]);
        s["encryption"]["eap-reauth-period"] = p.clone();
        let plan = wireless(&one(s), &radios(json!({})));
        let v = section(&plan, "stw_0_0_5g").unwrap();
        let want = ok.then(|| json!(p.to_string()));
        assert_eq!(v.get("eap_reauth_period"), want.as_ref(), "{p}");
        assert_eq!(plan.rejected.is_empty(), ok, "{p}: {}", reasons(&plan));
    }
}

/// A key on a mode that takes none is refused, not dropped: the operator believes the SSID is
/// keyed. The SSID still runs as its proto says, and the key stays out of the answer.
#[test]
fn a_key_on_a_mode_that_takes_none_is_refused() {
    for mode in MODES {
        let mut s = ssid(mode, &["5G"]);
        let takes_key = s["encryption"].get("key").is_some();
        s["encryption"]["key"] = json!(KEY);
        let plan = wireless(&one(s), &radios(json!({})));
        let v = section(&plan, "stw_0_0_5g").unwrap_or_else(|| panic!("{mode} runs"));
        let got = reasons(&plan);
        assert_eq!(v["encryption"], mode, "{mode}");
        assert_eq!(v.contains_key("key"), takes_key, "{mode}");
        assert_eq!(got.contains("takes no key"), !takes_key, "{mode}: {got}");
        assert_eq!(
            plan.rejected.len(),
            usize::from(!takes_key),
            "{mode}: {got}"
        );
        assert!(got.contains("encryption/key") || takes_key, "{mode}: {got}");
        assert!(!got.contains("ZZ"), "{mode}: {got}");
    }
    // Whatever its kind.
    let s = json!({ "name": "Open", "wifi-bands": ["5G"],
                    "encryption": { "proto": "owe", "key": ["ZZKEYlist"] } });
    let got = reasons(&wireless(&one(s), &radios(json!({}))));
    assert!(got.contains("takes no key") && !got.contains("ZZ"), "{got}");
}

/// What applies only to 802.1X, or only with mac-filter, is refused elsewhere rather than
/// dropped; the schema's defaults change nothing and pass.
#[test]
fn what_doesnt_apply_to_the_ssid_is_refused() {
    for mode in ["none", "owe", "psk2", "sae"] {
        for (key, value, refused) in [
            ("key-caching", json!(false), true),
            ("key-caching", json!("false"), true),
            ("key-caching", json!(true), false),
            ("eap-reauth-period", json!(600), true),
            ("eap-reauth-period", json!(3600), false),
        ] {
            let mut s = ssid(mode, &["5G"]);
            s["encryption"][key] = value.clone();
            let plan = wireless(&one(s), &radios(json!({})));
            let v = section(&plan, "stw_0_0_5g").unwrap();
            assert!(!v.contains_key("auth_cache") && !v.contains_key("eap_reauth_period"));
            assert_eq!(
                reasons(&plan).contains("802.1X (enterprise) SSIDs only"),
                refused,
                "{mode} {key} {value}: {}",
                reasons(&plan)
            );
        }
        // An authentication server without mac-filter (absent or false) isn't used here.
        for mac_filter in [None, Some(false)] {
            let mut s = ssid(mode, &["5G"]);
            s["radius"] =
                json!({ "authentication": { "host": "192.0.2.10", "port": 1812, "secret": AUTH } });
            if let Some(m) = mac_filter {
                s["radius"]["authentication"]["mac-filter"] = json!(m);
            }
            let plan = wireless(&one(s), &radios(json!({})));
            let v = section(&plan, "stw_0_0_5g").unwrap();
            let got = reasons(&plan);
            assert!(!v.contains_key("auth_server"), "{mode}");
            assert!(got.contains("only for MAC authentication"), "{mode}: {got}");
            assert!(!got.contains("ZZ"), "{got}");
        }
    }
}

fn with(mut s: Value, pointer: &str, key: &str, value: Value) -> Value {
    s.pointer_mut(pointer).unwrap()[key] = value;
    s
}

/// A value of the wrong kind is rejected, never guessed at. One that decides who may join
/// refuses the SSID: a mac-filter read as false would run it without MAC authentication.
#[test]
fn values_of_the_wrong_kind_are_rejected_not_guessed() {
    let psk = |radius: Value| with(ssid("psk2", &["5G"]), "", "radius", radius);
    let server = || json!({ "host": "192.0.2.10", "port": 1812, "secret": AUTH });
    let auth = |key: &str, v: Value| with(ssid("wpa2", &["5G"]), "/radius/authentication", key, v);
    let acct = |key: &str, v: Value| {
        let s = with(ssid("wpa2", &["5G"]), "/radius", "accounting", server());
        with(s, "/radius/accounting", key, v)
    };
    // (what, SSID, whether it runs, an option it mustn't have)
    let cases = [
        (
            "mac-filter string",
            psk(json!({ "authentication": with(server(), "", "mac-filter", json!("true")) })),
            false,
            "",
        ),
        (
            "mac-filter on an open SSID",
            with(
                ssid("none", &["5G"]),
                "",
                "radius",
                json!({ "authentication": with(server(), "", "mac-filter", json!(1)) }),
            ),
            false,
            "",
        ),
        ("radius string", psk(json!("ZZSECRETRADIUS")), false, ""),
        (
            "authentication string",
            psk(json!({ "authentication": "ZZSECRETAUTH2" })),
            false,
            "",
        ),
        (
            "ieee80211w unknown",
            with(
                ssid("psk2", &["5G"]),
                "/encryption",
                "ieee80211w",
                json!("Required"),
            ),
            false,
            "",
        ),
        (
            "ieee80211w boolean",
            with(
                ssid("sae", &["5G"]),
                "/encryption",
                "ieee80211w",
                json!(true),
            ),
            false,
            "",
        ),
        (
            "key-caching string",
            with(
                ssid("wpa2", &["5G"]),
                "/encryption",
                "key-caching",
                json!("false"),
            ),
            true,
            "",
        ),
        (
            "eap-reauth-period string",
            with(
                ssid("wpa2", &["5G"]),
                "/encryption",
                "eap-reauth-period",
                json!("600"),
            ),
            true,
            "eap_reauth_period",
        ),
        (
            "interval string",
            acct("interval", json!("120")),
            true,
            "acct_interval",
        ),
        (
            "interval under 60",
            acct("interval", json!(30)),
            true,
            "acct_interval",
        ),
        (
            "interval over 600",
            acct("interval", json!(601)),
            true,
            "acct_interval",
        ),
        (
            "chargeable-user-id string",
            with(
                ssid("wpa2", &["5G"]),
                "/radius",
                "chargeable-user-id",
                json!("true"),
            ),
            true,
            "request_cui",
        ),
        (
            "nas-identifier number",
            with(ssid("wpa2", &["5G"]), "/radius", "nas-identifier", json!(5)),
            true,
            "nasid",
        ),
        (
            "DAE port string",
            with(
                ssid("wpa2", &["5G"]),
                "/radius",
                "dynamic-authorization",
                json!({ "host": "192.0.2.10", "port": "3799", "secret": "ZZSECRETDAE" }),
            ),
            true,
            "dae_client",
        ),
        (
            "request-attribute object",
            auth("request-attribute", json!({ "id": 32, "value": "x" })),
            true,
            "radius_auth_req_attr",
        ),
        (
            "hex-value number",
            auth(
                "request-attribute",
                json!([{ "id": 32, "hex-value": 10, "value": "x" }]),
            ),
            true,
            "radius_auth_req_attr",
        ),
    ];
    for (what, s, runs, absent) in cases {
        let plan = wireless(&one(s), &radios(json!({})));
        let got = reasons(&plan);
        assert!(!plan.rejected.is_empty(), "{what}: accepted");
        assert!(!got.contains("ZZ"), "{what}: {got}");
        let v = section(&plan, "stw_0_0_5g");
        assert_eq!(v.is_some(), runs, "{what}: {got}");
        if let Some(v) = v {
            assert!(!v.contains_key(absent), "{what}: {absent} written");
        }
    }
    // key-caching of the wrong kind takes uCentral's default.
    let s = with(
        ssid("wpa2", &["5G"]),
        "/encryption",
        "key-caching",
        json!(0),
    );
    let plan = wireless(&one(s), &radios(json!({})));
    assert_eq!(section(&plan, "stw_0_0_5g").unwrap()["auth_cache"], "1");
}

/// Every key STW-22 reads, on one SSID.
fn every_security_key() -> Value {
    let attrs = json!([{ "id": 32, "value": "lobby" }, { "id": 27, "value": 900 },
                       { "id": 30, "hex-value": "0a0b" },
                       { "vendor-id": 14122, "vendor-attributes": [{ "id": 1, "value": "abcd" }] }]);
    let server = |host: &str, port: u16, secret: &str| {
        json!({ "host": host, "port": port, "secret": secret,
                "secondary": { "host": "192.0.2.11", "port": port, "secret": secret } })
    };
    let mut auth = server("192.0.2.10", 1812, AUTH);
    auth["request-attribute"] = attrs;
    auth["mac-filter"] = json!(false);
    let mut acct = server("192.0.2.12", 1813, "ZZSECRETACCT");
    acct["interval"] = json!(120);
    acct["request-attribute"] = json!([{ "id": 32, "value": "acct" }]);
    json!({ "name": "Corp", "wifi-bands": ["5G"],
            "encryption": { "proto": "wpa2", "ieee80211w": "optional",
                            "eap-reauth-period": 3600, "key-caching": false },
            "radius": { "nas-identifier": "corp-nas", "chargeable-user-id": true,
                        "authentication": auth, "accounting": acct,
                        "dynamic-authorization": { "host": "192.0.2.10", "port": 3799,
                                                   "secret": "ZZSECRETDAE" } } })
}

#[test]
fn keys_it_doesnt_read_are_unsupported_and_the_ones_it_reads_never_are() {
    let plan = wireless(&one(every_security_key()), &radios(json!({})));
    assert!(plan.rejected.is_empty(), "{}", reasons(&plan));
    let v = section(&plan, "stw_0_0_5g").unwrap();
    for (k, want) in [
        ("ieee80211w", json!("1")),
        ("auth_server", json!(["192.0.2.10", "192.0.2.11"])),
        ("acct_server", json!(["192.0.2.12", "192.0.2.11"])),
        ("acct_interval", json!("120")),
        ("nasid", json!("corp-nas")),
        ("request_cui", json!("1")),
        ("dae_port", json!("3799")),
        ("eap_reauth_period", json!("3600")),
        ("auth_cache", json!("0")),
        (
            "radius_auth_req_attr",
            json!([
                "32:s:lobby",
                "27:d:900",
                "30:x:0a0b",
                "26:x:0000372a0104abcd"
            ]),
        ),
        ("radius_acct_req_attr", json!(["32:s:acct"])),
    ] {
        assert_eq!(v.get(k), Some(&want), "{k}");
    }
    for pointer in [
        "/encryption",
        "/radius",
        "/radius/authentication",
        "/radius/authentication/secondary",
        "/radius/accounting",
        "/radius/accounting/secondary",
        "/radius/dynamic-authorization",
        "/radius/authentication/request-attribute/0",
        "/radius/authentication/request-attribute/2",
        "/radius/authentication/request-attribute/3",
        "/radius/authentication/request-attribute/3/vendor-attributes/0",
    ] {
        let s = with(
            every_security_key(),
            pointer,
            "extra-secret",
            json!("ZZSECRETX"),
        );
        let plan = wireless(&one(s), &radios(json!({})));
        let got = reasons(&plan);
        assert_eq!(plan.rejected.len(), 1, "{pointer}: {got}");
        assert!(
            got.contains(&format!("{pointer}/extra-secret\""))
                && got.contains("extra-secret isn't supported yet"),
            "{pointer}: {got}"
        );
        assert!(!got.contains("ZZ"), "{pointer}: {got}");
        assert!(section(&plan, "stw_0_0_5g").is_some(), "{pointer}");
    }
    // Each attribute form reads its own keys: a value beside a hex value, or an id beside a
    // vendor's, isn't read.
    for (i, key, value) in [(2, "value", json!("x")), (3, "id", json!(5))] {
        let at = format!("/radius/authentication/request-attribute/{i}");
        let s = with(every_security_key(), &at, key, value);
        let got = reasons(&wireless(&one(s), &radios(json!({}))));
        assert!(got.contains(&format!("{key} isn't supported yet")), "{got}");
    }
}

/// mac-filter is MAC authentication on the open, OWE, PSK and SAE SSIDs the scripts do it on;
/// on an 802.1X SSID it would be dropped, so that SSID is refused, not run without it.
#[test]
fn mac_filter_on_an_enterprise_ssid_is_refused() {
    for mode in MODES {
        let mut s = ssid(mode, &["5G"]);
        if !mode.starts_with("wpa") {
            s["radius"] =
                json!({ "authentication": { "host": "192.0.2.10", "port": 1812, "secret": AUTH } });
        }
        s["radius"]["authentication"]["mac-filter"] = json!(true);
        let plan = wireless(&one(s), &radios(json!({})));
        if mode.starts_with("wpa") {
            assert!(section(&plan, "stw_0_0_5g").is_none(), "{mode}");
            assert!(
                reasons(&plan).contains("MAC authentication works only on open, OWE, PSK and SAE"),
                "{mode}: {}",
                reasons(&plan)
            );
        } else {
            let v = section(&plan, "stw_0_0_5g").expect(mode);
            assert!(v.contains_key("auth_server"), "{mode}");
        }
    }
}

/// Only chargeable-user-id has `false` for its default: nas-identifier and dynamic-authorization
/// given as `false` on an SSID with no server are refused like any other value.
#[test]
fn only_chargeable_user_id_takes_false_without_a_server() {
    for (key, refused) in [
        ("chargeable-user-id", false),
        ("nas-identifier", true),
        ("dynamic-authorization", true),
    ] {
        let mut s = ssid("psk2", &["5G"]);
        s["radius"] = json!({ key: false });
        let plan = wireless(&one(s), &radios(json!({})));
        assert_eq!(
            reasons(&plan).contains("no RADIUS server to use it with"),
            refused,
            "{key}: {}",
            reasons(&plan)
        );
    }
}

/// On 6 GHz a WPA3 transition mode runs as pure WPA3, with MFP required: an ieee80211w asked
/// lower is listed as a substitution there too, beside the protocol's.
#[test]
fn mfp_raised_on_6ghz_is_listed() {
    for mode in ["sae-mixed", "wpa3-mixed"] {
        for (asked, listed) in [("disabled", true), ("optional", true), ("required", false)] {
            let mut s = ssid(mode, &["6G"]);
            s["encryption"]["ieee80211w"] = json!(asked);
            let plan = wireless(&one(s), &radios(json!({})));
            let mfp: Vec<&Value> = plan
                .rejected
                .iter()
                .filter(|r| {
                    r.parameter
                        .get("/interfaces/0/ssids/0/encryption/ieee80211w")
                        .is_some()
                })
                .filter_map(|r| r.substitution.as_ref())
                .collect();
            let named: Vec<&str> = mfp.iter().filter_map(|v| v.as_str()).collect();
            assert_eq!(
                named.contains(&"required"),
                listed,
                "{mode}/{asked}: {}",
                reasons(&plan)
            );
        }
    }
}
