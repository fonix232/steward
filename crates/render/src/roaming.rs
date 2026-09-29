//! Fast roaming and steering → `wifi-iface` options:
//! - 802.11r from `roaming`;
//! - 802.11k and the rest of radio resource management from `rrm`;
//! - 802.11v BSS transition from `services: ["wifi-steering"]`, which hands the SSID to usteer.
//!
//! usteer is the device's steering daemon, one for all its SSIDs and configured on its own:
//! Steward never edits it, and refuses steering it can't give (no usteer, or one that steers
//! other SSIDs only).
//!
//! The stock scripts derive 802.11r's key-holder key from the SSID's key, or, they mean to,
//! its RADIUS secret. For EAP that fails: they read `auth_secret` after it became
//! `auth_server_shared_secret`, and `FT_KEY_CANT_BE_DERIVED` fails the whole radio. So an
//! enterprise SSID gets its key holders from here, derived from the SSID and its secret: the
//! same on every AP with the same configuration.
//!
//! A key it doesn't read is rejected as unsupported, and so is a value of the wrong kind, never
//! guessed at.

use crate::security::{Security, printable};
use crate::{Plan, boolean, reject, unsupported};
use serde_json::{Map, Value, json};

/// The keys of `roaming` that [`fast_roaming`] reads; any other is rejected.
const ROAMING_KEYS: [&str; 6] = [
    "message-exchange",
    "generate-psk",
    "domain-identifier",
    "pmk-r0-key-holder",
    "pmk-r1-key-holder",
    "key-aes-256",
];
/// The keys of `rrm` that [`rrm`] reads; `stationary-ap` is refused with a reason of its own.
const RRM_KEYS: [&str; 6] = [
    "neighbor-reporting",
    "reduced-neighbor-reporting",
    "ftm-responder",
    "lci",
    "civic-location",
    "stationary-ap",
];

/// usteer as it runs on the device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usteer {
    /// Whether it answers on ubus.
    pub running: bool,
    /// The SSIDs it steers (`ssid_list`); `None` for all of them.
    pub ssid_list: Option<Vec<String>>,
}

impl Usteer {
    /// From `usteer get_config` over ubus; `None` (not running) when that failed.
    pub fn from_config(answer: Option<&Map<String, Value>>) -> Usteer {
        let Some(config) = answer else {
            return Usteer::default();
        };
        let ssid_list = config
            .get("ssid_list")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|l| !l.is_empty());
        Usteer {
            running: true,
            ssid_list,
        }
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| is_hex(p, 2))
}

/// Which key holder: hostapd checks their ids differently.
#[derive(Clone, Copy)]
enum Holder {
    /// `<MAC>,<R0KH-ID>,<key>`: the id is the NAS identifier, 1 to 47 characters.
    R0,
    /// `<MAC>,<R1KH-ID>,<key>`: the id is a MAC.
    R1,
}

/// A key holder as uCentral writes it (`<MAC>,<id>,<key>`), checked as hostapd's `add_r0kh` and
/// `add_r1kh` check it: a holder it refuses fails the whole radio.
fn key_holder(v: &Value, which: Holder) -> Option<String> {
    let s = v.as_str().filter(|s| printable(s) && !s.contains(' '))?;
    let parts: Vec<&str> = s.split(',').collect();
    let [mac, id, key] = parts[..] else {
        return None;
    };
    let id_ok = match which {
        Holder::R0 => (1..=47).contains(&id.len()),
        Holder::R1 => is_mac(id),
    };
    (is_mac(mac) && id_ok && (is_hex(key, 32) || is_hex(key, 64))).then(|| s.to_owned())
}

/// Key holders that accept every AP of the mobility domain, with `key`: what the stock scripts
/// write for a key they derived.
fn wildcard_holders(key: &str, values: &mut Map<String, Value>) {
    values.insert("r0kh".into(), json!([format!("ff:ff:ff:ff:ff:ff,*,{key}")]));
    values.insert(
        "r1kh".into(),
        json!([format!("00:00:00:00:00:00,00:00:00:00:00:00,{key}")]),
    );
}

/// 802.11r's options from `roaming` (true, or its object).
fn fast_roaming(
    ssid: &Value,
    name: &str,
    at: &str,
    security: &Security,
    values: &mut Map<String, Value>,
    plan: &mut Plan,
) {
    let path = |f: &str| format!("{at}/roaming{f}");
    let roaming = match ssid.get("roaming") {
        None | Some(Value::Bool(false)) => return,
        Some(Value::Bool(true)) => json!({}),
        Some(o @ Value::Object(_)) => o.clone(),
        Some(other) => {
            reject(
                plan,
                &path(""),
                other,
                "roaming is true, false or an object",
            );
            return;
        }
    };
    if !security.roams() {
        reject(
            plan,
            &path(""),
            &json!(true),
            "fast roaming needs WPA2 or later with a key or 802.1X",
        );
        return;
    }
    unsupported(plan, &path(""), &roaming, &ROAMING_KEYS);
    values.insert("ieee80211r".into(), json!("1"));
    let over_ds = match roaming.get("message-exchange") {
        None => false,
        Some(v) => match v.as_str() {
            Some("air") => false,
            Some("ds") => true,
            _ => {
                reject(
                    plan,
                    &path("/message-exchange"),
                    v,
                    "message-exchange is air or ds",
                );
                false
            }
        },
    };
    values.insert("ft_over_ds".into(), json!(if over_ds { "1" } else { "0" }));
    if let Some(d) = roaming.get("domain-identifier") {
        match d.as_str().filter(|d| is_hex(d, 4)) {
            Some(d) => {
                values.insert("mobility_domain".into(), json!(d.to_ascii_lowercase()));
            }
            None => reject(
                plan,
                &path("/domain-identifier"),
                d,
                "a mobility domain is 4 hex digits",
            ),
        }
    }
    let generate = boolean(plan, &roaming, &path("/generate-psk"));
    if security.is_wpa2_psk() {
        let local = generate.unwrap_or(false);
        values.insert(
            "ft_psk_generate_local".into(),
            json!(if local { "1" } else { "0" }),
        );
    } else if generate == Some(true) {
        reject(
            plan,
            &path("/generate-psk"),
            &json!(true),
            "only WPA2-PSK networks can generate FT responses locally",
        );
    }
    // Key holders: given as a shared key, given as they are, or (for EAP) derived.
    let r0 = roaming.get("pmk-r0-key-holder");
    let r1 = roaming.get("pmk-r1-key-holder");
    if let Some(key) = roaming.get("key-aes-256") {
        // The shared key names the key holders itself: holders beside it are refused.
        for (holder, v) in [("pmk-r0-key-holder", r0), ("pmk-r1-key-holder", r1)] {
            if let Some(v) = v {
                reject(
                    plan,
                    &path(&format!("/{holder}")),
                    v,
                    "key holders come from key-aes-256 or from pmk-r0-key-holder and pmk-r1-key-holder, not both",
                );
            }
        }
        match key.as_str().filter(|k| is_hex(k, 64)) {
            Some(k) => return wildcard_holders(&k.to_ascii_lowercase(), values),
            // An enterprise SSID still gets derived holders below: without any, the scripts
            // fail its whole radio.
            None => reject(
                plan,
                &path("/key-aes-256"),
                &json!("…"),
                "the shared key is 64 hex digits",
            ),
        }
    } else if r0.is_some() || r1.is_some() {
        match (
            r0.and_then(|v| key_holder(v, Holder::R0)),
            r1.and_then(|v| key_holder(v, Holder::R1)),
        ) {
            (Some(r0), Some(r1)) => {
                values.insert("r0kh".into(), json!([r0]));
                values.insert("r1kh".into(), json!([r1]));
                return;
            }
            _ => reject(
                plan,
                &path("/pmk-r0-key-holder"),
                &json!("…"),
                "key holders come in pairs: R0 as <MAC>,<R0KH-ID of 1 to 47 characters>,<key> and R1 as <MAC>,<R1KH-ID, a MAC>,<key>, the keys 32 or 64 hex digits",
            ),
        }
    }
    if let Some(secret) = security.radius_secret() {
        let digest = ring::digest::digest(
            &ring::digest::SHA256,
            format!("steward-ft\n{name}\n{secret}").as_bytes(),
        );
        let key: String = digest.as_ref()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        wildcard_holders(&key, values);
    }
}

/// `rrm`'s options: 802.11k neighbor reports, reduced neighbor reports, the FTM responder.
fn rrm(ssid: &Value, at: &str, values: &mut Map<String, Value>, plan: &mut Plan) {
    let Some(rrm) = ssid.get("rrm") else {
        return;
    };
    if !rrm.is_object() {
        return reject(plan, &format!("{at}/rrm"), rrm, "rrm is an object");
    }
    unsupported(plan, &format!("{at}/rrm"), rrm, &RRM_KEYS);
    let path = |f: &str| format!("{at}/rrm/{f}");
    for (key, option) in [
        ("neighbor-reporting", "ieee80211k"),
        ("reduced-neighbor-reporting", "rnr"),
        ("ftm-responder", "ftm_responder"),
    ] {
        if let Some(b) = boolean(plan, rrm, &path(key)) {
            values.insert(option.into(), json!(if b { "1" } else { "0" }));
        }
    }
    // A measurement subelement holds 255 bytes at most. hostapd reads its config in 4096-byte
    // lines: a longer value splits into an invalid line and fails the radio.
    for (key, option) in [("lci", "lci"), ("civic-location", "civic")] {
        if let Some(v) = rrm.get(key) {
            match v
                .as_str()
                .filter(|s| (2..=510).contains(&s.len()) && s.len() % 2 == 0 && is_hex(s, s.len()))
            {
                Some(s) => {
                    values.insert(option.into(), json!(s));
                }
                None => reject(
                    plan,
                    &path(key),
                    v,
                    format!("{key} is 1 to 255 bytes, as hex digits"),
                ),
            }
        }
    }
    if let Some(v) = rrm.get("stationary-ap") {
        reject(
            plan,
            &path("stationary-ap"),
            v,
            "stationary-ap is a radio setting, not supported yet",
        );
    }
}

/// Steering (`services` lists `wifi-steering`): 802.11v BSS transition and 802.11k, for usteer.
fn steering(
    ssid: &Value,
    name: &str,
    at: &str,
    usteer: Option<&Usteer>,
    values: &mut Map<String, Value>,
    plan: &mut Plan,
) {
    let Some(services) = ssid.get("services") else {
        return;
    };
    let Some(list) = services.as_array() else {
        let at = format!("{at}/services");
        return reject(plan, &at, services, "services is a list of strings");
    };
    // wifi-steering is the only service: every other entry is refused, not dropped.
    let mut steer = None;
    for (k, v) in list.iter().enumerate() {
        let at = format!("{at}/services/{k}");
        match v.as_str() {
            Some("wifi-steering") => {
                steer.get_or_insert(k);
            }
            Some(s) => reject(plan, &at, v, format!("{s} isn't supported yet")),
            None => reject(plan, &at, v, "a service is a string"),
        }
    }
    let Some(k) = steer else {
        return;
    };
    values.insert("bss_transition".into(), json!("1"));
    values.entry("ieee80211k").or_insert(json!("1"));
    let refusal = match usteer {
        None => Some("usteer isn't installed on this device"),
        Some(u) if !u.running => Some("usteer isn't running on this device"),
        Some(Usteer {
            ssid_list: Some(list),
            ..
        }) if !list.iter().any(|s| s == name) => {
            Some("usteer on this device steers only the SSIDs in its ssid_list")
        }
        _ => None,
    };
    if let Some(reason) = refusal {
        reject(
            plan,
            &format!("{at}/services/{k}"),
            &json!("wifi-steering"),
            format!("{reason}; the SSID gets 802.11k and 802.11v only"),
        );
    }
}

/// The SSID `name` at `at`: its roaming and steering options, the same on every band.
pub(crate) fn options(
    ssid: &Value,
    name: &str,
    at: &str,
    security: &Security,
    usteer: Option<&Usteer>,
    plan: &mut Plan,
) -> Map<String, Value> {
    let mut values = Map::new();
    fast_roaming(ssid, name, at, security, &mut values, plan);
    rrm(ssid, at, &mut values, plan);
    steering(ssid, name, at, usteer, &mut values, plan);
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_holders_are_checked_as_hostapd_checks_them() {
        let r0 = "00:00:5e:00:53:01,ap1,00112233445566778899aabbccddeeff";
        assert_eq!(key_holder(&json!(r0), Holder::R0).as_deref(), Some(r0));
        let r1 = "00:00:5e:00:53:01,00:00:5e:00:53:01,00112233445566778899aabbccddeeff";
        assert_eq!(key_holder(&json!(r1), Holder::R1).as_deref(), Some(r1));
        // An R1KH-ID is a MAC: TIP's schema example (`14DD204714E4`) isn't one.
        assert_eq!(key_holder(&json!(r0), Holder::R1), None);
        let long = format!(
            "00:00:5e:00:53:01,{},00112233445566778899aabbccddeeff",
            "n".repeat(48)
        );
        assert_eq!(
            key_holder(&json!(long), Holder::R0),
            None,
            "an R0KH-ID is 47 at most"
        );
        let at_limit = format!(
            "00:00:5e:00:53:01,{},00112233445566778899aabbccddeeff",
            "n".repeat(47)
        );
        assert!(key_holder(&json!(at_limit), Holder::R0).is_some());
        for bad in [
            "00:00:5e:00:53:01,ap1",
            "00:00:5e:00:53,ap1,00112233445566778899aabbccddeeff",
            "00:00:5e:00:53:01,ap1,0011",
            "00:00:5e:00:53:01,ap 1,00112233445566778899aabbccddeeff",
            "00:00:5e:00:53:01,ap1\n,00112233445566778899aabbccddeeff",
            "00:00:5e:00:53:01,,00112233445566778899aabbccddeeff",
        ] {
            assert_eq!(key_holder(&json!(bad), Holder::R0), None, "{bad}");
        }
    }

    #[test]
    fn usteer_steers_every_ssid_unless_it_lists_some() {
        let all = json!({ "band_steering_interval": 30000 });
        assert_eq!(
            Usteer::from_config(all.as_object()),
            Usteer {
                running: true,
                ssid_list: None
            }
        );
        let some = json!({ "ssid_list": ["Home"] });
        assert_eq!(
            Usteer::from_config(some.as_object()).ssid_list,
            Some(vec!["Home".to_string()])
        );
        assert!(!Usteer::from_config(None).running);
    }
}
