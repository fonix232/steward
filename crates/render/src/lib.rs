//! uCentral configuration → the UCI changes that make a device run it.
//!
//! Pure: given the configuration and what the device's UCI holds now, [`wireless`] returns
//! the changes and whatever it can't apply (for `configure`'s answer). Applying them, with a
//! rollback, is the agent's job (`steward_ubus::uci::Transaction`).
//!
//! Ownership: the agent only ever creates sections it names `stw_*` and marks with
//! `steward '1'`, and deletes only those. Radios are the exception: they are the device's
//! own `wifi-device` sections, so the renderer sets options on them and lists each one in
//! [`Plan::radio_options`], for the agent to record what it replaces.

mod network;

pub use network::{Network, Ports, Vlan};
use serde_json::{Map, Value, json};
use steward_proto::Rejection;

/// The option that marks a section as the agent's.
pub const MARKER: &str = "steward";
/// Every section the agent creates starts with this.
pub const PREFIX: &str = "stw_";

/// One UCI change. Values are UCI strings.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Create a named section.
    Add {
        config: String,
        kind: String,
        name: String,
        values: Map<String, Value>,
    },
    /// Set options on an existing section.
    Set {
        config: String,
        section: String,
        values: Map<String, Value>,
    },
    /// Delete a section.
    Delete { config: String, section: String },
}

/// What an op does, by name: its config, section and option names, never their values (a key,
/// a password), so it can go into a log or an answer.
impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |values: &Map<String, Value>| {
            values
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Op::Add {
                config,
                kind,
                name,
                values,
            } => write!(f, "add {config} {kind} {name} ({})", names(values)),
            Op::Set {
                config,
                section,
                values,
            } => write!(f, "set {config} {section} ({})", names(values)),
            Op::Delete { config, section } => write!(f, "delete {config} {section}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    pub rejected: Vec<Rejection>,
    /// (section, option) on the device's own radios that the plan sets.
    pub radio_options: Vec<(String, String)>,
}

/// A radio the device has: its `wifi-device` section and band (`2g`, `5g`, `6g`, `60g`).
#[derive(Debug, Clone, PartialEq)]
pub struct Radio {
    pub section: String,
    pub band: String,
    /// The HT modes it supports (`iwinfo info`'s `htmodes`: `HT20`, `HE80`, ...), filled in by
    /// [`Wireless::read_htmodes`]. `None`: unknown, so htmode isn't checked against them.
    pub htmodes: Option<Vec<String>>,
}

/// What the device's wireless config holds now.
#[derive(Debug, Default)]
pub struct Wireless {
    pub radios: Vec<Radio>,
    /// The `wifi-iface` sections the agent owns.
    pub owned: Vec<String>,
}

impl Wireless {
    /// From `uci get {"config": "wireless"}` over ubus: `{"values": {section: {...}}}`.
    pub fn from_uci(answer: &Map<String, Value>) -> Wireless {
        let mut w = Wireless::default();
        let Some(Value::Object(sections)) = answer.get("values") else {
            return w;
        };
        for (name, s) in sections {
            match s.get(".type").and_then(Value::as_str) {
                Some("wifi-device") => {
                    if let Some(band) = s.get("band").and_then(Value::as_str) {
                        w.radios.push(Radio {
                            section: name.clone(),
                            band: band.to_owned(),
                            htmodes: None,
                        });
                    }
                }
                // Both the name and the marker: a user's section that carries the marker
                // isn't the agent's to delete.
                Some("wifi-iface")
                    if name.starts_with(PREFIX)
                        && s.get(MARKER).and_then(Value::as_str) == Some("1") =>
                {
                    w.owned.push(name.clone())
                }
                _ => {}
            }
        }
        w
    }

    /// Fills in each radio's `htmodes` from `network.wireless status` and `info`, which answers
    /// `iwinfo info` for a device: the radio's phy (`config.phy`), or else one of its
    /// interfaces (a radio addressed by `path=` has no phy in its config). A radio it can't
    /// read stays unchecked.
    pub fn read_htmodes(
        &mut self,
        status: &Map<String, Value>,
        mut info: impl FnMut(&str) -> Option<Map<String, Value>>,
    ) {
        for radio in &mut self.radios {
            let Some(s) = status.get(&radio.section) else {
                continue;
            };
            let device = s["config"]["phy"]
                .as_str()
                .or_else(|| s["interfaces"][0]["ifname"].as_str());
            radio.htmodes = device
                .and_then(&mut info)
                .and_then(|i| {
                    let modes: Vec<String> = i
                        .get("htmodes")?
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    Some(modes)
                })
                .filter(|m| !m.is_empty());
        }
    }

    fn radio(&self, band: &str) -> Option<&Radio> {
        self.radios.iter().find(|r| r.band == band)
    }
}

/// Whether an option called `key` holds a secret (a key, password, passphrase or secret), or
/// raw lines for hostapd or UCI, which can hold any of them.
fn secret(key: &str) -> bool {
    key.ends_with("-raw")
        || ["secret", "password", "passphrase", "key"]
            .iter()
            .any(|s| key.contains(s))
}

/// `value`, sent as `key`, with every secret in it replaced by "…": what a rejection shows.
fn redacted(key: &str, value: &Value) -> Value {
    match value {
        _ if secret(key) => json!("…"),
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), redacted(k, v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| redacted(key, v)).collect()),
        v => v.clone(),
    }
}

/// Lists `value`, sent at `path`, as not applied. It's shown redacted, so no key, password or
/// raw line reaches the answer, whatever the caller passes.
pub(crate) fn reject(plan: &mut Plan, path: &str, value: &Value, reason: impl Into<String>) {
    let key = path.rsplit('/').next().unwrap_or(path);
    plan.rejected.push(Rejection {
        parameter: json!({ path: redacted(key, value) }),
        reason: reason.into(),
        substitution: None,
    });
}

/// Lists `value`, sent at `path`, as applied as `substitution` instead.
fn substitute(plan: &mut Plan, path: &str, value: &Value, reason: String, substitution: Value) {
    reject(plan, path, value, reason);
    if let Some(r) = plan.rejected.last_mut() {
        r.substitution = Some(json!({ path: substitution }));
    }
}

/// Rejects every key of `object`, sent at `at`, that isn't one of `handled`: dropping it
/// silently would answer 0 for a configuration that isn't applied as sent.
pub(crate) fn unsupported(plan: &mut Plan, at: &str, object: &Value, handled: &[&str]) {
    for (key, v) in object.as_object().into_iter().flatten() {
        if !handled.contains(&key.as_str()) {
            reject(
                plan,
                &format!("{at}/{key}"),
                v,
                format!("{key} isn't supported yet"),
            );
        }
    }
}

/// uCentral's band names to UCI's.
fn band(b: &str) -> Option<&'static str> {
    match b {
        "2G" => Some("2g"),
        "5G" => Some("5g"),
        "6G" => Some("6g"),
        _ => None,
    }
}

/// A UCI band as people say it.
fn ghz(band: &str) -> &'static str {
    match band {
        "2g" => "2.4 GHz",
        "5g" => "5 GHz",
        "6g" => "6 GHz",
        _ => "this band",
    }
}

/// Whether `band` has the 20 MHz channel `c`.
fn has_channel(band: &str, c: u64) -> bool {
    match band {
        "2g" => (1..=14).contains(&c),
        "5g" => (32..=144).contains(&c) && c % 4 == 0 || (149..=177).contains(&c) && c % 4 == 1,
        "6g" => (1..=233).contains(&c) && (c - 1) % 4 == 0,
        _ => false,
    }
}

/// Channel modes, best first.
const MODES: [&str; 4] = ["EHT", "HE", "VHT", "HT"];
/// Channel widths in MHz, widest first.
const WIDTHS: [u64; 5] = [320, 160, 80, 40, 20];

/// uCentral's channel mode and width to UCI's htmode (its mode and width), if `band` has
/// them; otherwise the reason it can't.
fn htmode(mode: &str, width: u64, band: &str) -> Result<(&'static str, u64), String> {
    let mode = match (mode, band) {
        // VHT exists only on 5 GHz; on 2.4 GHz the radio runs HT.
        ("VHT", "2g") => "HT",
        ("HT" | "VHT", "6g") => return Err(format!("6 GHz runs HE or EHT, not {mode}")),
        (m, _) => match MODES.iter().copied().find(|x| *x == m) {
            Some(x) => x,
            None => return Err(format!("channel mode {m} isn't supported")),
        },
    };
    let widths: &[u64] = match mode {
        "HT" => &[20, 40],
        "VHT" | "HE" => &[20, 40, 80, 160],
        _ => &[20, 40, 80, 160, 320],
    };
    if !widths.contains(&width) {
        return Err(format!("{width} MHz with {mode} isn't supported"));
    }
    let widest = match band {
        "2g" => 40,
        "5g" => 160,
        _ => 320,
    };
    if width > widest {
        return Err(format!("{} has no {width} MHz channels", ghz(band)));
    }
    Ok((mode, width))
}

/// What to run on a radio that doesn't support `mode` at `width` (both valid on `band`): the
/// same width in a lower mode, then narrower widths, the best mode first. `None` when it
/// supports none of them.
fn fallback(
    mode: &str,
    width: u64,
    band: &str,
    supported: &[String],
) -> Option<(&'static str, u64)> {
    WIDTHS.iter().filter(|w| **w <= width).find_map(|w| {
        MODES
            .iter()
            .skip_while(|m| **m != mode)
            .filter_map(|m| htmode(m, *w, band).ok())
            .find(|(m, w)| supported.contains(&format!("{m}{w}")))
    })
}

fn uci_bool(b: bool) -> Value {
    Value::String(if b { "1" } else { "0" }.into())
}

/// A true/false option of `object`, false when absent. Anything but a boolean is rejected
/// (and taken as false), not guessed at.
fn flag(plan: &mut Plan, object: &Value, path: &str) -> bool {
    let key = path.rsplit('/').next().unwrap_or(path);
    match object.get(key) {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            reject(plan, path, other, format!("{key} is true or false"));
            false
        }
    }
}

/// The keys of a radio that [`radios`] handles; any other is rejected.
const RADIO_KEYS: [&str; 7] = [
    "band",
    "channel",
    "channel-mode",
    "channel-width",
    "country",
    "tx-power",
    "enable",
];

/// The radios: options on the device's `wifi-device` sections.
fn radios(config: &Value, current: &Wireless, plan: &mut Plan) {
    let radios = match config.get("radios") {
        None => return,
        Some(Value::Array(a)) => a,
        Some(other) => return reject(plan, "/radios", other, "radios is a list"),
    };
    // The radio sections set so far, and by which entry.
    let mut set: Vec<(&str, usize)> = Vec::new();
    for (i, r) in radios.iter().enumerate() {
        let path = |f: &str| format!("/radios/{i}/{f}");
        let Some(b) = r.get("band").and_then(Value::as_str) else {
            reject(plan, &format!("/radios/{i}"), r, "a radio without a band");
            continue;
        };
        let Some(uci_band) = band(b) else {
            reject(
                plan,
                &path("band"),
                &json!(b),
                format!("band {b} isn't supported"),
            );
            continue;
        };
        let Some(radio) = current.radio(uci_band) else {
            reject(
                plan,
                &path("band"),
                &json!(b),
                format!("this device has no {b} radio"),
            );
            continue;
        };
        if let Some((_, first)) = set.iter().find(|(s, _)| *s == radio.section) {
            reject(
                plan,
                &format!("/radios/{i}"),
                r,
                format!("the {b} radio is set by /radios/{first} already"),
            );
            continue;
        }
        set.push((&radio.section, i));
        let mut values = Map::new();
        match r.get("channel") {
            None => {}
            Some(Value::String(s)) if s == "auto" => {
                values.insert("channel".into(), json!("auto"));
            }
            Some(v) => match v.as_u64().filter(|c| (1..=233).contains(c)) {
                Some(c) if has_channel(uci_band, c) => {
                    values.insert("channel".into(), json!(c.to_string()));
                }
                Some(c) => reject(
                    plan,
                    &path("channel"),
                    v,
                    format!("{} has no channel {c}", ghz(uci_band)),
                ),
                None => reject(
                    plan,
                    &path("channel"),
                    v,
                    "a channel is a number or \"auto\"",
                ),
            },
        }
        let mode = match r.get("channel-mode") {
            None => "HE".to_string(),
            Some(Value::String(m)) => m.clone(),
            Some(other) => other.to_string(),
        };
        let width = match r.get("channel-width") {
            // On 2.4 GHz the schema's default (80 MHz) doesn't exist: default to 20.
            None if uci_band == "2g" => Some(20),
            None => Some(80),
            Some(w) => {
                if w.as_u64().is_none() {
                    reject(
                        plan,
                        &path("channel-width"),
                        w,
                        "a channel width is a number of MHz",
                    );
                }
                w.as_u64()
            }
        };
        if let Some(width) = width {
            let asked = json!({ "channel-mode": mode, "channel-width": width });
            match htmode(&mode, width, uci_band) {
                Err(reason) => reject(plan, &path("channel-width"), &asked, reason),
                // The radio doesn't run it: the scripts would still turn on what the mode
                // implies (ieee80211ax for HE), and the radio would fail to start.
                Ok((m, w))
                    if radio
                        .htmodes
                        .as_ref()
                        .is_some_and(|s| !s.contains(&format!("{m}{w}"))) =>
                {
                    let supported = radio.htmodes.as_deref().unwrap_or_default();
                    let reason = format!("the {b} radio doesn't support {m}{w}");
                    match fallback(m, w, uci_band, supported) {
                        Some((m, w)) => {
                            values.insert("htmode".into(), json!(format!("{m}{w}")));
                            substitute(
                                plan,
                                &path("channel-width"),
                                &asked,
                                reason,
                                json!({ "channel-mode": m, "channel-width": w }),
                            );
                        }
                        None => reject(
                            plan,
                            &path("channel-width"),
                            &asked,
                            format!("{reason}, nor anything narrower"),
                        ),
                    }
                }
                Ok((m, w)) => {
                    values.insert("htmode".into(), json!(format!("{m}{w}")));
                }
            }
        }
        match r.get("country") {
            None => {}
            Some(Value::String(c)) if c.len() == 2 && c.chars().all(|x| x.is_ascii_uppercase()) => {
                values.insert("country".into(), json!(c));
            }
            Some(other) => reject(
                plan,
                &path("country"),
                other,
                "a country is two capital letters",
            ),
        }
        // TIP's schema: 0 to 30 dBm.
        match r.get("tx-power") {
            None => {}
            Some(Value::Number(p)) if p.as_u64().is_some_and(|p| p <= 30) => {
                values.insert("txpower".into(), json!(p.to_string()));
            }
            Some(other) => reject(
                plan,
                &path("tx-power"),
                other,
                "a tx power is a whole number of dBm, 0 to 30",
            ),
        }
        match r.get("enable") {
            None => {
                values.insert("disabled".into(), uci_bool(false));
            }
            Some(Value::Bool(on)) => {
                values.insert("disabled".into(), uci_bool(!on));
            }
            Some(other) => reject(plan, &path("enable"), other, "enable is true or false"),
        }
        unsupported(plan, &format!("/radios/{i}"), r, &RADIO_KEYS);
        plan.radio_options
            .extend(values.keys().map(|k| (radio.section.clone(), k.clone())));
        plan.ops.push(Op::Set {
            config: "wireless".into(),
            section: radio.section.clone(),
            values,
        });
    }
}

/// uCentral's encryption to UCI's: the `encryption` value, and whether a key is needed.
fn encryption(proto: &str) -> Option<(&'static str, bool)> {
    Some(match proto {
        "none" => ("none", false),
        "owe" => ("owe", false),
        "psk" => ("psk", true),
        "psk2" => ("psk2", true),
        "psk-mixed" => ("psk-mixed", true),
        "sae" => ("sae", true),
        "sae-mixed" => ("sae-mixed", true),
        _ => return None,
    })
}

/// The keys of an SSID that [`ssids`] handles; any other is rejected.
const SSID_KEYS: [&str; 6] = [
    "name",
    "wifi-bands",
    "bss-mode",
    "encryption",
    "hidden-ssid",
    "isolate-clients",
];

/// The keys of an SSID's `encryption` that [`ssids`] handles; any other is rejected.
const ENCRYPTION_KEYS: [&str; 3] = ["proto", "key", "ieee80211w"];

/// The SSIDs: one owned `wifi-iface` per SSID per band.
fn ssids(
    config: &Value,
    current: &Wireless,
    networks: &[Option<String>],
    plan: &mut Plan,
) -> Vec<String> {
    let mut wanted = Vec::new();
    let interfaces = match config.get("interfaces") {
        None => return wanted,
        Some(Value::Array(a)) => a,
        Some(other) => {
            reject(plan, "/interfaces", other, "interfaces is a list");
            return wanted;
        }
    };
    for (i, iface) in interfaces.iter().enumerate() {
        let list = match iface.get("ssids") {
            None => continue,
            Some(Value::Array(a)) => a,
            Some(other) => {
                reject(
                    plan,
                    &format!("/interfaces/{i}/ssids"),
                    other,
                    "ssids is a list",
                );
                continue;
            }
        };
        // The interface's network; without network information, the device's LAN.
        let network = match networks.get(i) {
            Some(Some(n)) => n.clone(),
            Some(None) => {
                reject(
                    plan,
                    &format!("/interfaces/{i}/ssids"),
                    &json!(
                        list.iter()
                            .filter_map(|s| s.get("name"))
                            .collect::<Vec<_>>()
                    ),
                    "its interface's network was refused, so its SSIDs are too",
                );
                continue;
            }
            None => "lan".to_string(),
        };
        for (s, ssid) in list.iter().enumerate() {
            let at = format!("/interfaces/{i}/ssids/{s}");
            let path = |f: &str| format!("{at}/{f}");
            let Some(name) = ssid
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty() && n.len() <= 32)
            else {
                reject(
                    plan,
                    &path("name"),
                    ssid.get("name").unwrap_or(&Value::Null),
                    "an SSID is 1 to 32 bytes",
                );
                continue;
            };
            let mode = match ssid.get("bss-mode") {
                None => "ap".to_string(),
                Some(Value::String(m)) => m.clone(),
                Some(other) => other.to_string(),
            };
            if mode != "ap" {
                reject(
                    plan,
                    &path("bss-mode"),
                    &json!(mode),
                    format!("{mode} isn't supported yet"),
                );
                continue;
            }
            // No encryption is an open network; an encryption without a protocol isn't.
            let enc = ssid
                .get("encryption")
                .cloned()
                .unwrap_or(json!({ "proto": "none" }));
            let Some(proto) = enc.get("proto").and_then(Value::as_str) else {
                reject(
                    plan,
                    &path("encryption"),
                    &enc,
                    "an encryption names its proto",
                );
                continue;
            };
            let Some((uci_enc, needs_key)) = encryption(proto) else {
                reject(
                    plan,
                    &path("encryption/proto"),
                    &json!(proto),
                    format!("{proto} isn't supported yet"),
                );
                continue;
            };
            let key = enc.get("key").and_then(Value::as_str);
            // A passphrase is 8 to 63 printable characters: hostapd reads its config a line at a
            // time, so a newline in one would add a line of its own. 64 hex digits are a PSK
            // itself, which SAE can't use: the scripts would leave it without a password.
            let sae = uci_enc.starts_with("sae");
            if needs_key
                && !key.is_some_and(|k| {
                    ((8..=63).contains(&k.len()) && k.bytes().all(|b| (32..=126).contains(&b)))
                        || (!sae && k.len() == 64 && k.chars().all(|c| c.is_ascii_hexdigit()))
                })
            {
                let reason = if sae {
                    "an SAE key is 8 to 63 printable characters"
                } else {
                    "a key is 8 to 63 printable characters, or 64 hex digits"
                };
                reject(plan, &path("encryption/key"), &json!("…"), reason);
                continue;
            }
            if let (false, Some(v)) = (needs_key, enc.get("key")) {
                // Dropped quietly, it would answer 0 for an SSID its operator believes is keyed.
                reject(
                    plan,
                    &path("encryption/key"),
                    v,
                    format!("{proto} takes no key: only the PSK and SAE modes do"),
                );
            }
            // Management frame protection, one of TIP's values. Anything else refuses the SSID
            // rather than running it with less protection than asked.
            let Some(asked_mfp) = enc.get("ieee80211w").map_or(Some("disabled"), |m| {
                m.as_str()
                    .filter(|m| ["disabled", "optional", "required"].contains(m))
            }) else {
                reject(
                    plan,
                    &path("encryption/ieee80211w"),
                    &enc["ieee80211w"],
                    "ieee80211w is disabled, optional or required",
                );
                continue;
            };
            let configured = match asked_mfp {
                "required" => "2",
                "optional" => "1",
                _ => "0",
            };
            // What the protocol runs: sae and owe with it required, sae-mixed at least optional
            // (its SAE clients need it), open and WPA1-only networks without it (the scripts
            // write ieee80211w=0 there). An ieee80211w asked otherwise is a substitution, so the
            // answer says what runs.
            let mfp = match uci_enc {
                "sae" | "owe" => "2",
                "sae-mixed" => configured.max("1"),
                "none" | "psk" => "0",
                _ => configured,
            };
            if enc.get("ieee80211w").is_some() && mfp != configured {
                let named = match mfp {
                    "2" => "required",
                    "1" => "optional",
                    _ => "disabled",
                };
                let reason = if mfp == "0" {
                    format!("{uci_enc} has no management frame protection")
                } else {
                    format!("{uci_enc} needs management frame protection {named}")
                };
                plan.rejected.push(Rejection {
                    parameter: json!({ path("encryption/ieee80211w"): enc["ieee80211w"] }),
                    reason,
                    substitution: Some(json!(named)),
                });
            }
            let Some(bands) = ssid
                .get("wifi-bands")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
            else {
                reject(
                    plan,
                    &path("wifi-bands"),
                    ssid.get("wifi-bands").unwrap_or(&Value::Null),
                    "an SSID runs on at least one band",
                );
                continue;
            };
            // Multi-PSK, captive portals, RADIUS, rate limits, ACLs, raw hostapd lines...
            unsupported(plan, &at, ssid, &SSID_KEYS);
            unsupported(plan, &path("encryption"), &enc, &ENCRYPTION_KEYS);
            let (hidden, isolate) = (
                flag(plan, ssid, &path("hidden-ssid")),
                flag(plan, ssid, &path("isolate-clients")),
            );
            for (b, bandname) in bands.iter().enumerate() {
                let radio = bandname
                    .as_str()
                    .and_then(band)
                    .and_then(|ub| current.radio(ub));
                let Some(radio) = radio else {
                    let shown = bandname
                        .as_str()
                        .map_or(bandname.to_string(), str::to_owned);
                    reject(
                        plan,
                        &format!("{}/{b}", path("wifi-bands")),
                        bandname,
                        format!("this device has no {shown} radio"),
                    );
                    continue;
                };
                let section = format!("{PREFIX}{i}_{s}_{}", radio.band);
                // A band listed twice is still one section.
                if wanted.contains(&section) {
                    continue;
                }
                let mut values = Map::new();
                values.insert("device".into(), json!(radio.section));
                values.insert("mode".into(), json!("ap"));
                values.insert("ssid".into(), json!(name));
                values.insert("network".into(), json!(network));
                values.insert("encryption".into(), json!(uci_enc));
                if let (true, Some(k)) = (needs_key, key) {
                    values.insert("key".into(), json!(k));
                }
                values.insert("ieee80211w".into(), json!(mfp));
                values.insert("hidden".into(), uci_bool(hidden));
                values.insert("isolate".into(), uci_bool(isolate));
                values.insert("disabled".into(), json!("0"));
                values.insert(MARKER.into(), json!("1"));
                plan.ops.push(if current.owned.contains(&section) {
                    Op::Set {
                        config: "wireless".into(),
                        section: section.clone(),
                        values,
                    }
                } else {
                    Op::Add {
                        config: "wireless".into(),
                        kind: "wifi-iface".into(),
                        name: section.clone(),
                        values,
                    }
                });
                wanted.push(section);
            }
        }
    }
    wanted
}

/// The top-level keys this renderer handles; any other is rejected.
const TOP_KEYS: [&str; 3] = ["uuid", "radios", "interfaces"];

/// Radios, SSIDs on `networks` (by interface), and the owned wireless sections no longer needed.
/// Top-level keys it doesn't handle are rejected here, as both entry points pass through.
fn wireless_into(config: &Value, current: &Wireless, networks: &[Option<String>], plan: &mut Plan) {
    unsupported(plan, "", config, &TOP_KEYS);
    radios(config, current, plan);
    let wanted = ssids(config, current, networks, plan);
    for stale in current.owned.iter().filter(|s| !wanted.contains(s)) {
        plan.ops.push(Op::Delete {
            config: "wireless".into(),
            section: stale.clone(),
        });
    }
}

/// The wireless part of a configuration: radios and SSIDs, every SSID on the device's LAN.
pub fn wireless(config: &Value, current: &Wireless) -> Plan {
    let mut plan = Plan::default();
    wireless_into(config, current, &[], &mut plan);
    plan
}

/// What the device holds now, for [`render`].
pub struct Current<'a> {
    pub wireless: &'a Wireless,
    pub network: &'a Network,
    pub ports: &'a Ports,
}

/// A whole configuration: networks and VLANs, radios, and SSIDs on their networks. The
/// network changes come first, so a new network exists before an SSID joins it.
pub fn render(config: &Value, current: &Current<'_>) -> Plan {
    let mut plan = Plan::default();
    let (networks, wanted) = network::networks(config, current.network, current.ports, &mut plan);
    wireless_into(config, current.wireless, &networks, &mut plan);
    for (stale, _) in current
        .network
        .owned
        .iter()
        .filter(|(s, _)| !wanted.contains(s))
    {
        plan.ops.push(Op::Delete {
            config: "network".into(),
            section: stale.clone(),
        });
    }
    plan
}
