//! An SSID's security: its encryption, management frame protection (MFP) and RADIUS servers →
//! the `wifi-iface` options OpenWrt's wifi scripts read (`/usr/share/ucode/wifi/ap.uc`).
//!
//! - Enterprise modes (`wpa`, `wpa2`, `wpa-mixed`, `wpa3`, `wpa3-mixed`, `wpa3-192`) need a
//!   RADIUS authentication server; hostapd takes its address, not a name.
//! - On open, OWE, PSK and SAE networks, `radius.authentication.mac-filter` sends each client's
//!   MAC to that server (the scripts turn a server on a non-EAP network into
//!   `macaddr_acl=2`). Accounting works on any network.
//! - Stock hostapd has one port and secret per server list, so a secondary server is added as
//!   a second address only when it shares them.
//! - Secrets never go into a rejection, and no string with a control character goes into
//!   hostapd's configuration: the scripts write most options unquoted, one per line.
//! - A key it doesn't read, or one that doesn't apply to the SSID (an authentication server
//!   without `mac-filter`, `key-caching` off 802.1X), is rejected, and so is a value of the
//!   wrong kind, never guessed at. One that decides who may join or how they're protected
//!   (`ieee80211w`, `mac-filter`) refuses the SSID rather than run it with less than was asked.

use crate::{Plan, boolean, reject, unsupported};
use serde_json::{Map, Value, json};
use std::net::IpAddr;
use steward_proto::Rejection;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Open,
    Owe,
    Psk,
    Eap,
}

/// What the protocol makes of the configured MFP.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mfp {
    /// As configured (WPA2 and older).
    Configured,
    /// At least optional: the WPA3 transition modes.
    Optional,
    /// Required: WPA3 and OWE.
    Required,
}

/// uCentral's protocol → UCI's `encryption`, its kind and MFP rule.
fn protocol(proto: &str) -> Option<(&'static str, Kind, Mfp)> {
    use Kind::*;
    use Mfp::*;
    Some(match proto {
        "none" => ("none", Open, Configured),
        "owe" => ("owe", Owe, Required),
        "psk" => ("psk", Psk, Configured),
        "psk2" => ("psk2", Psk, Configured),
        "psk-mixed" => ("psk-mixed", Psk, Configured),
        "sae" => ("sae", Psk, Required),
        "sae-mixed" => ("sae-mixed", Psk, Optional),
        "wpa" => ("wpa", Eap, Configured),
        "wpa2" => ("wpa2", Eap, Configured),
        "wpa-mixed" => ("wpa-mixed", Eap, Configured),
        "wpa3" => ("wpa3", Eap, Required),
        "wpa3-mixed" => ("wpa3-mixed", Eap, Optional),
        "wpa3-192" => ("wpa3-192", Eap, Required),
        _ => return None,
    })
}

/// What an encryption becomes on 6 GHz, which allows only WPA3 and OWE: itself, its WPA3 form
/// (a substitution), or nothing.
fn on_6ghz(encryption: &'static str) -> Option<&'static str> {
    match encryption {
        "sae" | "owe" | "wpa3" | "wpa3-192" => Some(encryption),
        "sae-mixed" => Some("sae"),
        "wpa3-mixed" => Some("wpa3"),
        _ => None,
    }
}

/// Whether a string can go into hostapd's configuration: no control characters.
pub(crate) fn printable(s: &str) -> bool {
    !s.chars().any(char::is_control)
}

/// `v`, an object that holds secrets (a server, `radius`, `certificates`), as a rejection
/// shows it: every secret replaced, and all of it when it isn't an object, since what it holds
/// can't be told apart.
fn redacted(v: &Value) -> Value {
    if v.is_object() {
        redacted_in(v)
    } else {
        json!("…")
    }
}

fn redacted_in(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let secret = ["secret", "password", "key"].iter().any(|s| k.contains(s));
                    (k.clone(), if secret { json!("…") } else { redacted_in(v) })
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(redacted_in).collect()),
        v => v.clone(),
    }
}

/// The keys of `encryption` that [`Security::of`] reads; any other is rejected.
const ENCRYPTION_KEYS: [&str; 5] = [
    "proto",
    "key",
    "ieee80211w",
    "key-caching",
    "eap-reauth-period",
];
/// The keys of `radius`: `local` and `health` are refused with reasons of their own.
const RADIUS_KEYS: [&str; 7] = [
    "authentication",
    "accounting",
    "dynamic-authorization",
    "nas-identifier",
    "chargeable-user-id",
    "local",
    "health",
];
/// The keys of `radius.authentication`.
const AUTHENTICATION_KEYS: [&str; 6] = [
    "host",
    "port",
    "secret",
    "secondary",
    "request-attribute",
    "mac-filter",
];
/// The keys of `radius.accounting`.
const ACCOUNTING_KEYS: [&str; 6] = [
    "host",
    "port",
    "secret",
    "secondary",
    "request-attribute",
    "interval",
];
/// The keys of a secondary server, and of `dynamic-authorization` (its client).
const SERVER_KEYS: [&str; 3] = ["host", "port", "secret"];
/// The keys of a vendor attribute's sub-attribute.
const VENDOR_ATTRIBUTE_KEYS: [&str; 2] = ["id", "value"];

/// A RADIUS server: address, port, secret.
struct Server {
    host: String,
    port: u16,
    secret: String,
}

impl Server {
    /// From `{host, port, secret}`; the reason when it can't be used.
    fn parse(v: &Value) -> Result<Server, &'static str> {
        let host = v.get("host").and_then(Value::as_str).unwrap_or("");
        let port = v.get("port").and_then(Value::as_u64).unwrap_or(0);
        let secret = v.get("secret").and_then(Value::as_str).unwrap_or("");
        if host.is_empty() || port == 0 || secret.is_empty() {
            return Err("a RADIUS server needs a host, a port and a secret");
        }
        if host.parse::<IpAddr>().is_err() {
            return Err("hostapd takes a RADIUS server's address, not its name");
        }
        let port = u16::try_from(port).map_err(|_| "a port is 1 to 65535")?;
        if !printable(secret) {
            return Err("a secret can't hold control characters");
        }
        Ok(Server {
            host: host.to_owned(),
            port,
            secret: secret.to_owned(),
        })
    }
}

/// hostapd's form of a RADIUS request attribute (`<id>:s:<text>`, `<id>:d:<number>`,
/// `<id>:x:<hex>`, or a vendor-specific attribute as `26:x:<vendor><type><length><value>`).
/// A RADIUS attribute's value is 253 bytes at most: hostapd loads a longer one, then fails
/// every request it would go in (`radius_msg_add_attr`), so nobody could authenticate.
fn attribute(a: &Value) -> Option<String> {
    let hex = |s: &str| s.len() % 2 == 0 && s.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(vendor) = a.get("vendor-id") {
        // The schema's bounds: a vendor id of 1 to 65535, and at least one attribute.
        let vendor = vendor.as_u64().filter(|v| (1..=65535).contains(v))?;
        let vsas = a
            .get("vendor-attributes")?
            .as_array()
            .filter(|v| !v.is_empty())?;
        let mut out = format!("26:x:{vendor:08x}");
        // The vendor id's 4 bytes, then each sub-attribute's type, length and value.
        let mut size = 4;
        for vsa in vsas {
            let id = vsa
                .get("id")
                .and_then(Value::as_u64)
                .filter(|i| (1..=255).contains(i))?;
            let value = vsa
                .get("value")
                .and_then(Value::as_str)
                .filter(|v| hex(v))?;
            let len = u8::try_from(value.len() / 2 + 2).ok()?;
            size += usize::from(len);
            if size > 253 {
                return None;
            }
            out.push_str(&format!("{id:02x}{len:02x}{}", value.to_ascii_lowercase()));
        }
        return Some(out);
    }
    let id = a
        .get("id")
        .and_then(Value::as_u64)
        .filter(|i| (1..=255).contains(i))?;
    if let Some(h) = a.get("hex-value") {
        let h = h.as_str()?;
        let fits = (2..=506).contains(&h.len());
        return (fits && hex(h)).then(|| format!("{id}:x:{}", h.to_ascii_lowercase()));
    }
    match a.get("value")? {
        Value::String(s) if printable(s) && (1..=253).contains(&s.len()) => {
            Some(format!("{id}:s:{s}"))
        }
        // A RADIUS integer is 32 bits.
        Value::Number(n) => n
            .as_u64()
            .filter(|n| *n <= u64::from(u32::MAX))
            .map(|n| format!("{id}:d:{n}")),
        _ => None,
    }
}

/// The keys [`attribute`] reads in `a`'s form: a vendor's, a hex value or a value.
fn attribute_keys(a: &Value) -> &'static [&'static str] {
    if a.get("vendor-id").is_some() {
        &["vendor-id", "vendor-attributes"]
    } else if a.get("hex-value").is_some() {
        &["id", "hex-value"]
    } else {
        &["id", "value"]
    }
}

/// An SSID's security, worked out once and then rendered for each of its bands.
pub(crate) struct Security {
    encryption: &'static str,
    mfp: Mfp,
    configured_mfp: &'static str,
    /// `ieee80211w` as the configuration gave it, when it did.
    asked_mfp: Option<Value>,
    /// Everything else: the key, RADIUS, key caching.
    values: Map<String, Value>,
}

impl Security {
    /// From the SSID at `at`, with what can't be used rejected; `None` when the SSID can't run.
    pub(crate) fn of(ssid: &Value, at: &str, plan: &mut Plan) -> Option<Security> {
        let path = |f: &str| format!("{at}/{f}");
        let enc = ssid
            .get("encryption")
            .cloned()
            .unwrap_or(json!({ "proto": "none" }));
        // No encryption is an open network; an encryption without a protocol isn't.
        let Some(proto) = enc.get("proto").and_then(Value::as_str) else {
            reject(
                plan,
                &path("encryption"),
                &enc,
                "an encryption names its proto",
            );
            return None;
        };
        let Some((encryption, kind, mfp)) = protocol(proto) else {
            let reason = match proto {
                "psk2-radius" | "mpsk-radius" => {
                    "keys per client (private PSK) aren't supported yet".to_string()
                }
                p => format!("{p} isn't supported yet"),
            };
            reject(plan, &path("encryption/proto"), &json!(proto), reason);
            return None;
        };
        unsupported(plan, &path("encryption"), &enc, &ENCRYPTION_KEYS);
        let mut values = Map::new();
        if kind == Kind::Psk {
            // SAE takes a password: a 64-hex PSK would go to wpa_psk and leave SAE without one.
            let sae = encryption.starts_with("sae");
            let key = enc.get("key").and_then(Value::as_str);
            let Some(key) = key.filter(|k| {
                ((8..=63).contains(&k.len()) && k.chars().all(|c| (' '..='~').contains(&c)))
                    || (!sae && k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit()))
            }) else {
                let reason = if sae {
                    "an SAE password is 8 to 63 printable characters"
                } else {
                    "a key is 8 to 63 printable characters, or 64 hex digits"
                };
                reject(plan, &path("encryption/key"), &json!("…"), reason);
                return None;
            };
            values.insert("key".into(), json!(key));
        } else if let Some(v) = enc.get("key") {
            // Dropped quietly, it would answer 0 for an SSID its operator believes is keyed.
            reject(
                plan,
                &path("encryption/key"),
                v,
                format!("{proto} takes no key: only the PSK and SAE modes do"),
            );
        }
        let mut configured_mfp = match enc.get("ieee80211w") {
            None => "0",
            Some(v) => match v.as_str() {
                Some("disabled") => "0",
                Some("optional") => "1",
                Some("required") => "2",
                _ => {
                    reject(
                        plan,
                        &path("encryption/ieee80211w"),
                        v,
                        "ieee80211w is disabled, optional or required",
                    );
                    return None;
                }
            },
        };
        // Open and WPA1-only networks have no MFP: the scripts write ieee80211w=0 whatever is
        // configured (and a SHA-256 AKM WPA1 clients don't know), so it's turned off here.
        if configured_mfp != "0" && matches!(encryption, "none" | "psk" | "wpa") {
            plan.rejected.push(Rejection {
                parameter: json!({ path("encryption/ieee80211w"): enc["ieee80211w"] }),
                reason: format!("{encryption} has no management frame protection"),
                substitution: Some(json!("disabled")),
            });
            configured_mfp = "0";
        }
        // The protocol can need more than was asked: sae, owe, wpa3 and wpa3-192 run with it
        // required, the WPA3 transition modes at least optional. An `ieee80211w` given lower is a
        // substitution too, so the answer says what runs.
        if enc.get("ieee80211w").is_some() {
            let runs = match mfp {
                Mfp::Required => "2",
                Mfp::Optional => configured_mfp.max("1"),
                Mfp::Configured => configured_mfp,
            };
            if runs != configured_mfp {
                let named = if runs == "2" { "required" } else { "optional" };
                plan.rejected.push(Rejection {
                    parameter: json!({ path("encryption/ieee80211w"): enc["ieee80211w"] }),
                    reason: format!("{encryption} needs management frame protection {named}"),
                    substitution: Some(json!(named)),
                });
            }
        }

        let radius = match ssid.get("radius") {
            None => Value::Null,
            Some(r @ Value::Object(_)) => r.clone(),
            // What it holds (mac-filter among it) can't be read, so the SSID can't run.
            Some(other) => {
                reject(
                    plan,
                    &path("radius"),
                    &redacted(other),
                    "radius is an object",
                );
                return None;
            }
        };
        unsupported(plan, &path("radius"), &radius, &RADIUS_KEYS);
        for key in ["local", "health"] {
            if let Some(v) = radius.get(key) {
                let reason = match key {
                    "local" => "hostapd's built-in EAP server isn't supported yet",
                    _ => "RADIUS health checks aren't supported",
                };
                reject(plan, &path(&format!("radius/{key}")), &redacted(v), reason);
            }
        }
        if let Some(v) = ssid.get("certificates") {
            reject(
                plan,
                &path("certificates"),
                &redacted(v),
                "certificates are only for hostapd's built-in EAP server, which isn't supported yet",
            );
        }
        let auth = radius.get("authentication");
        if let Some(a) = auth.filter(|a| !a.is_object()) {
            reject(
                plan,
                &path("radius/authentication"),
                &redacted(a),
                "a RADIUS server is an object with a host, a port and a secret",
            );
            return None;
        }
        // MAC authentication decides who may join: one that can't be read refuses the SSID,
        // rather than run it without.
        let mac_filter = match auth.and_then(|a| a.get("mac-filter")) {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => {
                reject(
                    plan,
                    &path("radius/authentication/mac-filter"),
                    other,
                    "mac-filter is true or false",
                );
                return None;
            }
        };
        // The scripts do MAC authentication (`macaddr_acl=2`) on open, OWE, PSK and SAE SSIDs
        // only: on an 802.1X one it would be dropped, so the SSID is refused rather than run
        // without it.
        if mac_filter && kind == Kind::Eap {
            reject(
                plan,
                &path("radius/authentication/mac-filter"),
                &json!(true),
                format!(
                    "{proto} is 802.1X: MAC authentication works only on open, OWE, PSK and SAE SSIDs"
                ),
            );
            return None;
        }
        if kind == Kind::Eap || mac_filter {
            let Some(auth) = auth else {
                reject(
                    plan,
                    &path("radius/authentication"),
                    &Value::Null,
                    format!("{proto} needs a RADIUS authentication server"),
                );
                return None;
            };
            let keys = &AUTHENTICATION_KEYS;
            unsupported(plan, &path("radius/authentication"), auth, keys);
            match Server::parse(auth) {
                Ok(primary) => {
                    let servers = servers(auth, &primary, &path("radius/authentication"), plan);
                    values.insert("auth_server".into(), servers);
                    values.insert("auth_port".into(), json!(primary.port.to_string()));
                    values.insert("auth_secret".into(), json!(primary.secret));
                    attributes(
                        auth,
                        "radius_auth_req_attr",
                        &path("radius/authentication"),
                        &mut values,
                        plan,
                    );
                }
                Err(reason) => {
                    reject(
                        plan,
                        &path("radius/authentication"),
                        &redacted(auth),
                        reason,
                    );
                    return None;
                }
            }
        } else if let Some(auth) = auth {
            reject(
                plan,
                &path("radius/authentication"),
                &redacted(auth),
                "an open, OWE, PSK or SAE SSID uses an authentication server only for MAC authentication (mac-filter)",
            );
        }
        if let Some(acct) = radius.get("accounting") {
            let at = path("radius/accounting");
            unsupported(plan, &at, acct, &ACCOUNTING_KEYS);
            match Server::parse(acct) {
                Ok(primary) => {
                    let servers = servers(acct, &primary, &at, plan);
                    values.insert("acct_server".into(), servers);
                    values.insert("acct_port".into(), json!(primary.port.to_string()));
                    values.insert("acct_secret".into(), json!(primary.secret));
                    // The schema's bounds: hostapd reads it with atoi and doesn't check it.
                    if let Some(i) = acct.get("interval") {
                        match i.as_u64().filter(|i| (60..=600).contains(i)) {
                            Some(i) => {
                                values.insert("acct_interval".into(), json!(i.to_string()));
                            }
                            None => reject(
                                plan,
                                &format!("{at}/interval"),
                                i,
                                "interval is 60 to 600 seconds",
                            ),
                        }
                    }
                    attributes(acct, "radius_acct_req_attr", &at, &mut values, plan);
                }
                Err(reason) => reject(plan, &at, &redacted(acct), reason),
            }
        }
        let has_server = values.contains_key("auth_server") || values.contains_key("acct_server");
        if !has_server {
            for key in [
                "nas-identifier",
                "chargeable-user-id",
                "dynamic-authorization",
            ] {
                // `chargeable-user-id: false` is the schema's default: nothing to refuse. (The
                // others have no such default: a `false` there is refused like anything else.)
                let default = key == "chargeable-user-id" && radius.get(key) == Some(&json!(false));
                if let Some(v) = radius.get(key).filter(|_| !default) {
                    reject(
                        plan,
                        &path(&format!("radius/{key}")),
                        &redacted(v),
                        "this SSID has no RADIUS server to use it with",
                    );
                }
            }
        }
        if has_server {
            if let Some(v) = radius.get("nas-identifier") {
                match v
                    .as_str()
                    .filter(|n| printable(n) && !n.is_empty() && n.len() <= 48)
                {
                    Some(nasid) => {
                        values.insert("nasid".into(), json!(nasid));
                    }
                    None => reject(
                        plan,
                        &path("radius/nas-identifier"),
                        v,
                        "a NAS identifier is 1 to 48 printable characters",
                    ),
                }
            }
            if boolean(plan, &radius, &path("radius/chargeable-user-id")) == Some(true) {
                values.insert("request_cui".into(), json!("1"));
            }
            if let Some(dae) = radius.get("dynamic-authorization")
                && kind != Kind::Eap
            {
                // The scripts add the secret to radius_das_client only for EAP, and hostapd
                // refuses a client without one: the whole radio would fail to start.
                reject(
                    plan,
                    &path("radius/dynamic-authorization"),
                    &redacted(dae),
                    "dynamic authorization works on 802.1X (enterprise) SSIDs only",
                );
            } else if let Some(dae) = radius.get("dynamic-authorization") {
                let at = path("radius/dynamic-authorization");
                unsupported(plan, &at, dae, &SERVER_KEYS);
                let host = dae.get("host").and_then(Value::as_str).unwrap_or("");
                let secret = dae.get("secret").and_then(Value::as_str).unwrap_or("");
                // Only a missing port takes the default.
                let port = dae
                    .get("port")
                    .map_or(Some(3799), Value::as_u64)
                    .filter(|p| (1..=65535).contains(p));
                if host.parse::<IpAddr>().is_err() {
                    reject(
                        plan,
                        &at,
                        &redacted(dae),
                        "dynamic authorization needs its client's address",
                    );
                } else if secret.is_empty() || !printable(secret) {
                    reject(
                        plan,
                        &at,
                        &redacted(dae),
                        "dynamic authorization needs a secret without control characters",
                    );
                } else if let Some(port) = port {
                    values.insert("dae_client".into(), json!(host));
                    values.insert("dae_port".into(), json!(port.to_string()));
                    values.insert("dae_secret".into(), json!(secret));
                } else {
                    reject(plan, &at, &redacted(dae), "a port is 1 to 65535");
                }
            }
        }
        if kind == Kind::Eap {
            // hostapd reads the period with atoi: from 2^31 it turns negative ("invalid
            // period") and the radio fails, so only the schema's 0 to 86400 is taken.
            if let Some(v) = enc.get("eap-reauth-period") {
                match v.as_u64().filter(|p| *p <= 86400) {
                    Some(p) => {
                        values.insert("eap_reauth_period".into(), json!(p.to_string()));
                    }
                    None => reject(
                        plan,
                        &path("encryption/eap-reauth-period"),
                        v,
                        "eap-reauth-period is 0 to 86400 seconds",
                    ),
                }
            }
            // PMKSA caching: uCentral's default is on; the scripts' is off for EAP.
            let caching = boolean(plan, &enc, &path("encryption/key-caching")).unwrap_or(true);
            values.insert("auth_cache".into(), json!(if caching { "1" } else { "0" }));
        } else {
            // Both are EAP's. The schema's defaults change nothing: nothing to refuse.
            for (key, default) in [
                ("key-caching", json!(true)),
                ("eap-reauth-period", json!(3600)),
            ] {
                if let Some(v) = enc.get(key).filter(|v| **v != default) {
                    reject(
                        plan,
                        &path(&format!("encryption/{key}")),
                        v,
                        format!("{key} works on 802.1X (enterprise) SSIDs only"),
                    );
                }
            }
        }
        Some(Security {
            encryption,
            mfp,
            configured_mfp,
            asked_mfp: enc.get("ieee80211w").cloned(),
            values,
        })
    }

    /// The options for the SSID at `at` on `band` (UCI's: `2g`, `5g`, `6g`; `band_at` its entry
    /// in `wifi-bands`); `None` when it can't run on that band.
    pub(crate) fn on(
        &self,
        band: &str,
        at: &str,
        band_at: &str,
        plan: &mut Plan,
    ) -> Option<Map<String, Value>> {
        let mut encryption = self.encryption;
        let mut mfp = self.mfp;
        if band == "6g" {
            match on_6ghz(encryption) {
                Some(e) if e == encryption => {}
                Some(e) => {
                    plan.rejected.push(Rejection {
                        parameter: json!({ format!("{at}/encryption/proto"): encryption }),
                        reason: "on 6 GHz, which allows only WPA3 and OWE".into(),
                        substitution: Some(json!(e)),
                    });
                    // What runs on the other bands, as the answer already says.
                    let elsewhere = match mfp {
                        Mfp::Required => "2",
                        Mfp::Optional => self.configured_mfp.max("1"),
                        Mfp::Configured => self.configured_mfp,
                    };
                    if let Some(asked) = self.asked_mfp.as_ref().filter(|_| elsewhere != "2") {
                        plan.rejected.push(Rejection {
                            parameter: json!({ format!("{at}/encryption/ieee80211w"): asked }),
                            reason: format!(
                                "on 6 GHz, where {e} needs management frame protection required"
                            ),
                            substitution: Some(json!("required")),
                        });
                    }
                    encryption = e;
                    mfp = Mfp::Required;
                }
                None => {
                    reject(
                        plan,
                        band_at,
                        &json!("6G"),
                        format!("6 GHz allows only WPA3 and OWE, not {encryption}"),
                    );
                    return None;
                }
            }
        }
        let mut values = self.values.clone();
        values.insert("encryption".into(), json!(encryption));
        let ieee80211w = match mfp {
            Mfp::Required => "2",
            Mfp::Optional if self.configured_mfp == "2" => "2",
            Mfp::Optional => "1",
            Mfp::Configured => self.configured_mfp,
        };
        values.insert("ieee80211w".into(), json!(ieee80211w));
        Some(values)
    }
}

/// The server's address, and a secondary's when it shares the primary's port and secret: one
/// address as an option, two as a list.
fn servers(v: &Value, primary: &Server, at: &str, plan: &mut Plan) -> Value {
    let Some(second) = v.get("secondary") else {
        return json!(primary.host);
    };
    unsupported(plan, &format!("{at}/secondary"), second, &SERVER_KEYS);
    match Server::parse(second) {
        Ok(s) if s.port == primary.port && s.secret == primary.secret => {
            json!([primary.host, s.host])
        }
        Ok(_) => {
            reject(
                plan,
                &format!("{at}/secondary"),
                &redacted(second),
                "a secondary server must share the primary's port and secret (hostapd takes one of each)",
            );
            json!(primary.host)
        }
        Err(reason) => {
            reject(plan, &format!("{at}/secondary"), &redacted(second), reason);
            json!(primary.host)
        }
    }
}

/// A server's request attributes, as a UCI list; the ones hostapd can't take are rejected.
fn attributes(v: &Value, option: &str, at: &str, values: &mut Map<String, Value>, plan: &mut Plan) {
    let Some(list) = v.get("request-attribute") else {
        return;
    };
    let Some(list) = list.as_array() else {
        let at = format!("{at}/request-attribute");
        return reject(plan, &at, list, "request-attribute is a list");
    };
    let mut out = vec![];
    for (i, a) in list.iter().enumerate() {
        let at = format!("{at}/request-attribute/{i}");
        match attribute(a) {
            Some(s) => {
                out.push(s);
                unsupported(plan, &at, a, attribute_keys(a));
                let vsas = a.get("vendor-id").and(a.get("vendor-attributes"));
                for (j, vsa) in vsas
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let at = format!("{at}/vendor-attributes/{j}");
                    unsupported(plan, &at, vsa, &VENDOR_ATTRIBUTE_KEYS);
                }
            }
            None => reject(
                plan,
                &at,
                a,
                "an attribute is an id (1 to 255) with a text, number or hex value, or a vendor's hex values",
            ),
        }
    }
    if !out.is_empty() {
        values.insert(option.into(), json!(out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_attributes_take_hostapds_form() {
        let cases = [
            (json!({ "id": 32, "value": "My NAS" }), Some("32:s:My NAS")),
            (json!({ "id": 27, "value": 900 }), Some("27:d:900")),
            (json!({ "id": 32, "hex-value": "0A0B" }), Some("32:x:0a0b")),
            (
                json!({ "vendor-id": 14122, "vendor-attributes": [{ "id": 1, "value": "abcd" }] }),
                Some("26:x:0000372a0104abcd"),
            ),
            (json!({ "id": 0, "value": "x" }), None),
            (json!({ "id": 32, "hex-value": "abc" }), None),
            (json!({ "id": 32, "value": "two\nlines" }), None),
        ];
        for (a, want) in cases {
            assert_eq!(attribute(&a).as_deref(), want, "{a}");
        }
    }

    #[test]
    fn secrets_are_redacted_at_any_depth() {
        let v = json!({ "host": "192.0.2.1", "secret": "s3", "secondary": { "secret": "s4" },
                        "users": [{ "password": "p" }] });
        let r = redacted(&v).to_string();
        assert!(
            !r.contains("s3") && !r.contains("s4") && !r.contains("\"p\""),
            "{r}"
        );
        assert!(r.contains("192.0.2.1"));
        // A server, radius or certificates sent as anything but an object: all of it.
        for v in [json!("s5"), json!(["192.0.2.1", "s6"])] {
            assert_eq!(redacted(&v), json!("…"));
        }
    }
}
