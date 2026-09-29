//! Routed networks: the IPv4 of a network Steward made (`stw_vlan<vid>`), DHCP on it
//! (dnsmasq), its firewall zone (fw4), and local DNS records.
//!
//! Only Steward's own VLAN networks are addressed: the device's `lan` and the networks it
//! joined keep theirs. DHCP and DNS go into owned `dhcp` sections, and the zone into owned
//! `firewall` ones. fw4 warns about options it doesn't know, so firewall sections carry no
//! marker and are known by their `stw_` names. A `dhcp` section without the marker under a name
//! Steward wants is someone else's: what would go there is refused, since rpcd's `add` would
//! merge into it.
//!
//! What can't be served is refused, never dropped: reservations without a pool, pool numbers
//! that aren't whole numbers (only missing ones take dnsmasq's 100 and 150), lease times
//! dnsmasq would change or misread, and anything for dnsmasq or fw4 while it isn't running
//! (`Sections::running`). A network whose subnet overlaps another interface's isn't addressed.
//!
//! Current dnsmasq serves DHCPv4 on a section only with `dhcpv4 'server'` (OpenWrt's own
//! `lan` has it); without it the range is left out.

use crate::security::printable;
use crate::{MARKER, Network, Op, PREFIX, Plan, put, reject, unsupported};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};

/// A config Steward adds sections to (`dhcp`, `firewall`): the sections it owns there, and the
/// names the others go by.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sections {
    /// Owned sections: name → (type, options).
    pub owned: BTreeMap<String, (String, Vec<String>)>,
    /// Every section's type and `name` option (firewall zones by name).
    pub names: Vec<(String, String)>,
    /// Every section's name, owned or not: rpcd's `add` under a name that's taken merges into
    /// that section.
    pub sections: BTreeSet<String>,
    /// The `cname` sections that aren't Steward's, as (alias, target): dnsmasq takes their
    /// CNAMEs with Steward's.
    pub cnames: Vec<(String, String)>,
    /// Whether what reads the config runs: dnsmasq ([`dnsmasq_running`]) for `dhcp`, fw4
    /// ([`firewall_active`]) for `firewall`. What only it would serve (pools, reservations, DNS
    /// records, zones) is refused otherwise, with the reason; the owned sections the
    /// configuration no longer wants still go. `from_uci` leaves it false: the caller asks.
    pub running: bool,
}

/// Whether dnsmasq runs, from procd's `service list {"name": "dnsmasq"}` over ubus: one of its
/// instances (one per `dnsmasq` section) is `running`. A stopped dnsmasq, or one never started
/// (disabled), isn't listed (`{}`), and an instance that exited stays listed with `running`
/// false.
pub fn dnsmasq_running(answer: &Map<String, Value>) -> bool {
    answer
        .get("dnsmasq")
        .and_then(|s| s.get("instances"))
        .and_then(Value::as_object)
        .is_some_and(|i| i.values().any(|x| x.get("running") == Some(&json!(true))))
}

/// Whether fw4 is active, from procd's `service list {"name": "firewall"}` over ubus. Its init
/// script starts no daemon: `start` runs `fw4 start` once and registers the service with its
/// reload triggers, so a started firewall is listed with no instances (`{"firewall": {}}`), and
/// a change to its config reloads it. `stop` flushes the ruleset and removes the service, and a
/// firewall disabled at boot was never listed: `{}`.
pub fn firewall_active(answer: &Map<String, Value>) -> bool {
    answer.contains_key("firewall")
}

impl Sections {
    /// From `uci get` over ubus. `marked`: whether owned sections carry the marker (`dhcp`), or
    /// are known by their `stw_` name alone (`firewall`).
    pub fn from_uci(answer: &Map<String, Value>, marked: bool) -> Sections {
        let mut s = Sections::default();
        for (name, section) in answer
            .get("values")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let kind = section
                .get(".type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if let Some(n) = section.get("name").and_then(Value::as_str) {
                s.names.push((kind.clone(), n.to_owned()));
            }
            let owned = name.starts_with(PREFIX)
                && (!marked || section.get(MARKER).and_then(Value::as_str) == Some("1"));
            s.sections.insert(name.clone());
            if owned {
                s.owned
                    .insert(name.clone(), (kind, crate::options_of(section)));
            } else if kind == "cname" {
                let option = |o: &str| section.get(o).and_then(Value::as_str).map(str::to_owned);
                if let (Some(alias), Some(target)) = (option("cname"), option("target")) {
                    s.cnames.push((alias, target));
                }
            }
        }
        s
    }

    fn has(&self, kind: &str, name: &str) -> bool {
        self.names.iter().any(|(k, n)| k == kind && n == name)
    }
}

/// Adds or sets an owned section of `config`; one of another type under that name is replaced.
/// A section under that name that isn't Steward's (an unmarked `stw_` one in `dhcp`) is left
/// alone, and the reason comes back for the caller to refuse its part: adding over it would
/// merge Steward's options into someone else's section.
fn put_owned(
    plan: &mut Plan,
    config: &str,
    current: &Sections,
    kind: &str,
    section: &str,
    values: Map<String, Value>,
) -> Result<(), String> {
    match current.owned.get(section) {
        Some((k, had)) if k == kind => put(plan, config, kind, section, values, Some(had)),
        Some(_) => {
            plan.ops.push(Op::Delete {
                config: config.into(),
                section: section.into(),
            });
            put(plan, config, kind, section, values, None);
        }
        None if current.sections.contains(section) => {
            return Err(format!(
                "{config} already has a section {section} that isn't Steward's (no {MARKER} marker)"
            ));
        }
        None => put(plan, config, kind, section, values, None),
    }
    Ok(())
}

fn marked(values: &[(&str, Value)]) -> Map<String, Value> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .chain([(MARKER.to_string(), json!("1"))])
        .collect()
}

fn unmarked(values: &[(&str, Value)]) -> Map<String, Value> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// An IPv4 subnet: the device's address in it, and its prefix.
#[derive(Debug, Clone, Copy)]
struct Subnet {
    address: Ipv4Addr,
    prefix: u8,
}

impl Subnet {
    fn parse(s: &str) -> Option<Subnet> {
        let (a, p) = s.split_once('/')?;
        let prefix: u8 = p.parse().ok().filter(|p| (8..=30).contains(p))?;
        let address: Ipv4Addr = a.parse().ok()?;
        let sub = Subnet { address, prefix };
        (address != sub.network() && address != sub.broadcast()).then_some(sub)
    }
    fn mask(&self) -> u32 {
        u32::MAX << (32 - self.prefix)
    }
    fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.address) & self.mask())
    }
    fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.network()) | !self.mask())
    }
    /// The subnet's address `offset` on from its network address, when it's a host address.
    fn host(&self, offset: u64) -> Option<Ipv4Addr> {
        let a = u64::from(u32::from(self.network())).checked_add(offset)?;
        (offset >= 1 && a < u64::from(u32::from(self.broadcast())))
            .then(|| Ipv4Addr::from(a as u32))
    }
    fn span(&self) -> (u32, u32) {
        span(self.address, self.prefix)
    }
}

/// The first and last address of `address`'s prefix.
fn span(address: Ipv4Addr, prefix: u8) -> (u32, u32) {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    let first = u32::from(address) & mask;
    (first, first | !mask)
}

/// A subnet a routed network's mustn't overlap.
#[derive(Debug)]
pub(crate) struct Taken {
    /// The interface that has it.
    whose: String,
    /// Its first and last address and how it's written, or, when it can't be read for sure,
    /// what it is: an overlap can't be ruled out then.
    span: Result<(u32, u32, String), String>,
}

/// The subnets of the device's static interfaces that aren't Steward's, as their config gives
/// them: this configuration's routed networks must stay clear of them. An interface Steward
/// made is addressed by this configuration, so its old address doesn't count.
pub(crate) fn taken(network: &Network) -> Vec<Taken> {
    let mut taken = vec![];
    for (name, (addresses, netmask)) in &network.addresses {
        if network.is_owned(name) {
            continue;
        }
        for a in addresses {
            let written = match netmask {
                Some(m) => format!("{a} (netmask {m})"),
                None => a.clone(),
            };
            let span = static_span(a, netmask.as_deref())
                .map(|(first, last)| (first, last, written.clone()))
                .ok_or_else(|| format!("{name}'s address {written}"));
            taken.push(Taken {
                whose: name.clone(),
                span,
            });
        }
    }
    taken
}

/// An `ipaddr` entry of netifd's static protocol, as its first and last address: `a.b.c.d/n`
/// (the prefix wins over `netmask`), or `a.b.c.d` with the interface's `netmask` (dotted and
/// contiguous, or a prefix), or alone (netifd's /32). `None` for anything else, which netifd
/// may read otherwise: it takes a netmask in any base `strtoul` knows (`030` is 24), and any
/// dotted mask.
fn static_span(entry: &str, netmask: Option<&str>) -> Option<(u32, u32)> {
    let prefix = |p: &str| {
        let decimal = p == "0" || (!p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit()));
        decimal
            .then(|| p.parse::<u8>().ok())
            .flatten()
            .filter(|p| *p <= 32)
    };
    // A netmask netifd might read otherwise leaves every entry unsure, CIDR ones too (netifd
    // refuses the interface over a netmask it can't read).
    let mask = match netmask {
        None => 32,
        Some(m) if m.contains('.') => {
            let bits = u32::from(m.parse::<Ipv4Addr>().ok()?);
            // Contiguous: ones, then zeros.
            (bits.leading_ones() + bits.trailing_zeros() == 32)
                .then_some(bits.leading_ones() as u8)?
        }
        Some(m) => prefix(m)?,
    };
    let (address, prefix) = match entry.split_once('/') {
        Some((a, p)) => (a, p.parse::<u8>().ok().filter(|p| *p <= 32)?),
        None => (entry, mask),
    };
    Some(span(address.parse().ok()?, prefix))
}

/// The shortest lease time taken, in seconds: dnsmasq raises a shorter one to it (`Leases of a
/// minute or less confuse some clients`).
const LEASE_MIN: u64 = 120;
/// The longest, a year: dnsmasq multiplies the number by its unit in a C `int`, which
/// overflows past about 68 years; `infinite` says longer.
const LEASE_MAX: u64 = 365 * 24 * 3600;

/// dnsmasq's lease time: a number of seconds, or with a unit (s, m, h, d, w), from 2 minutes to
/// a year, or `infinite`. The reason otherwise.
fn lease_time(v: &Value) -> Result<String, &'static str> {
    const FORM: &str = "a lease time is a number with s, m, h, d or w, or infinite";
    const RANGE: &str = "a lease time is 2 minutes to a year (dnsmasq raises a shorter one to 2 minutes, and misreads one much longer), or infinite";
    let s = match v {
        Value::Number(n) => n.as_u64().ok_or(FORM)?.to_string(),
        Value::String(s) => s.clone(),
        _ => return Err(FORM),
    };
    if s == "infinite" {
        return Ok(s);
    }
    let (digits, unit) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        Some((i, 'd')) => (&s[..i], 86_400),
        Some((i, 'w')) => (&s[..i], 604_800),
        _ => (s.as_str(), 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(FORM);
    }
    match digits.parse::<u64>().ok().and_then(|n| n.checked_mul(unit)) {
        Some(t) if (LEASE_MIN..=LEASE_MAX).contains(&t) => Ok(s),
        _ => Err(RANGE),
    }
}

/// One or more IPv4 addresses (a list, or one as a string). An empty list is none: dnsmasq
/// won't start on the `6,` it would make, and netifd would get `dns` with nothing in it.
fn ipv4_list(v: &Value) -> Option<Vec<String>> {
    let list: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    if list.is_empty() {
        return None;
    }
    list.iter()
        .map(|a| {
            a.as_str()
                .filter(|s| s.parse::<Ipv4Addr>().is_ok())
                .map(str::to_owned)
        })
        .collect()
}

fn is_mac(s: &str) -> bool {
    let p: Vec<&str> = s.split(':').collect();
    p.len() == 6
        && p.iter()
            .all(|x| x.len() == 2 && x.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A DNS name: labels of letters, digits and hyphens, 253 characters at most.
fn is_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The keys handled, or refused with a reason of their own, in an owned network's `ipv4`, its
/// `dhcp`, a reservation and a DNS record. Any other is rejected: dropped, it would be
/// answered 0.
const IPV4_KEYS: [&str; 9] = [
    "addressing",
    "subnet",
    "gateway",
    "use-dns",
    "send-hostname",
    "dhcp",
    "dhcp-leases",
    "port-forward",
    "disallow-upstream-subnet",
];
const DHCP_KEYS: [&str; 4] = ["lease-first", "lease-count", "lease-time", "use-dns"];
const LEASE_KEYS: [&str; 4] = [
    "macaddr",
    "static-lease-offset",
    "lease-time",
    "publish-hostname",
];
const RECORD_KEYS: [&str; 3] = ["name", "type", "value"];

/// What an owned VLAN network's `ipv4` adds to it: its interface's options (`proto` and the
/// rest), and the `dhcp` and `firewall` sections it needs (pushed on `plan`, their names in
/// `wanted`). Its subnet must stay clear of those `taken`, and joins them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ipv4(
    at: &str,
    iface: &Value,
    vid: u16,
    network: &str,
    dhcp: Option<&Sections>,
    firewall: Option<&Sections>,
    taken: &mut Vec<Taken>,
    wanted: &mut Vec<(String, String)>,
    plan: &mut Plan,
) -> Map<String, Value> {
    let mut values = Map::new();
    values.insert("proto".into(), json!("none"));
    let Some(ipv4) = iface.get("ipv4") else {
        return values;
    };
    let at = format!("{at}/ipv4");
    let path = |f: &str| format!("{at}/{f}");
    if !ipv4.is_object() {
        reject(plan, &at, ipv4, "ipv4 is an object");
        return values;
    }
    unsupported(plan, &at, ipv4, &IPV4_KEYS);
    for key in ["port-forward", "disallow-upstream-subnet"] {
        if let Some(v) = ipv4.get(key) {
            let reason = match key {
                "port-forward" => "port forwards aren't supported yet",
                _ => "keeping a network from others is the firewall's, not supported yet",
            };
            reject(plan, &path(key), v, reason);
        }
    }
    if iface.get("role").and_then(Value::as_str) == Some("upstream") {
        reject(
            plan,
            &at,
            ipv4,
            "addressing an upstream (WAN) network isn't supported yet",
        );
        return values;
    }
    // Missing is none; a value of the wrong kind (5, true, null) is refused, never taken for
    // none. A refused one leaves the network unaddressed (proto none).
    let addressing = match ipv4.get("addressing") {
        None => "none",
        Some(Value::String(a)) if ["static", "dynamic", "none"].contains(&a.as_str()) => a,
        Some(other) => {
            reject(
                plan,
                &path("addressing"),
                other,
                "addressing is static, dynamic or none",
            );
            "refused"
        }
    };
    let serving = ["dhcp", "dhcp-leases"];
    // Only static addressing uses these: with any other, they'd be dropped.
    if addressing != "static" {
        refuse(
            ipv4,
            &["subnet", "gateway", "use-dns"],
            &path,
            "a subnet, a gateway and DNS servers go with static addressing",
            plan,
        );
    }
    // Whether the DHCP client sends the device's name (TIP's default: it does). netifd's dhcp
    // protocol sends the hostname unless `hostname` is `*`; other addressing has no client, so
    // it asks nothing there.
    let send_hostname = match ipv4.get("send-hostname") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            reject(
                plan,
                &path("send-hostname"),
                other,
                "send-hostname is true or false",
            );
            true
        }
    };
    match addressing {
        "static" => {}
        "dynamic" => {
            values.insert("proto".into(), json!("dhcp"));
            if !send_hostname {
                values.insert("hostname".into(), json!("*"));
            }
            refuse(
                ipv4,
                &serving,
                &path,
                "DHCP serving needs static addressing",
                plan,
            );
            return values;
        }
        _ => {
            refuse(
                ipv4,
                &serving,
                &path,
                "DHCP serving needs static addressing",
                plan,
            );
            return values;
        }
    }
    let raw = ipv4.get("subnet").and_then(Value::as_str).unwrap_or("");
    let refused_subnet = |plan: &mut Plan, reason: String| {
        reject(plan, &path("subnet"), &json!(raw), reason);
        refuse(
            ipv4,
            &["gateway", "use-dns"],
            &path,
            "a gateway and DNS servers need the network's subnet, which was refused",
            plan,
        );
        refuse(
            ipv4,
            &serving,
            &path,
            "DHCP serving needs the network's subnet, which was refused",
            plan,
        );
    };
    let Some(subnet) = Subnet::parse(raw) else {
        let reason = if raw.starts_with("auto/") {
            "automatic subnets (globals.ipv4-network) aren't supported yet"
        } else {
            "a subnet is the device's address and a prefix of 8 to 30 (192.0.2.1/24)"
        };
        refused_subnet(plan, reason.into());
        return values;
    };
    // Two networks on one subnet would route and serve it twice. Refused, not guessed at, when
    // another interface's address can't be read for sure.
    let (first, last) = subnet.span();
    let clash = taken.iter().find(|t| {
        t.span
            .as_ref()
            .map_or(true, |(f, l, _)| *f <= last && first <= *l)
    });
    if let Some(t) = clash {
        let reason = match &t.span {
            Ok((_, _, theirs)) => format!("it overlaps {theirs} on {}", t.whose),
            Err(what) => {
                format!("{what} isn't one Steward reads for sure, so an overlap can't be ruled out")
            }
        };
        refused_subnet(plan, reason);
        return values;
    }
    taken.push(Taken {
        whose: network.into(),
        span: Ok((first, last, raw.into())),
    });
    values.insert("proto".into(), json!("static"));
    values.insert(
        "ipaddr".into(),
        json!(format!("{}/{}", subnet.address, subnet.prefix)),
    );
    if let Some(g) = ipv4.get("gateway") {
        match g.as_str().filter(|g| g.parse::<Ipv4Addr>().is_ok()) {
            Some(g) => {
                values.insert("gateway".into(), json!(g));
            }
            None => reject(plan, &path("gateway"), g, "a gateway is an IPv4 address"),
        }
    }
    if let Some(d) = ipv4.get("use-dns") {
        match ipv4_list(d) {
            Some(list) => {
                values.insert("dns".into(), json!(list));
            }
            None => reject(
                plan,
                &path("use-dns"),
                d,
                "DNS servers are one or more IPv4 addresses",
            ),
        }
    }
    let zone = format!("stw{vid}");
    let serves_dhcp = if ipv4.get("dhcp").is_some() {
        serve_dhcp(ipv4, &path, &subnet, network, vid, dhcp, wanted, plan)
    } else {
        refuse(
            ipv4,
            &["dhcp-leases"],
            &path,
            "reservations need a DHCP pool (ipv4.dhcp)",
            plan,
        );
        false
    };
    // Without fw4 active the network is still addressed, but nothing filters it: its zone is
    // refused, so the answer says so.
    if let Some(fw) = firewall.filter(|f| f.running) {
        let mut put_fw = |kind: &str, section: String, v: Map<String, Value>| {
            if let Err(reason) = put_owned(plan, "firewall", fw, kind, &section, v) {
                reject(plan, &at, &json!(section), reason);
                return;
            }
            wanted.push(("firewall".into(), section));
        };
        put_fw(
            "zone",
            format!("{PREFIX}zone{vid}"),
            unmarked(&[
                ("name", json!(zone)),
                ("network", json!([network])),
                ("input", json!("REJECT")),
                ("output", json!("ACCEPT")),
                ("forward", json!("REJECT")),
            ]),
        );
        if serves_dhcp {
            put_fw(
                "rule",
                format!("{PREFIX}dhcp{vid}"),
                unmarked(&[
                    ("name", json!(format!("Steward: DHCP on {zone}"))),
                    ("src", json!(zone)),
                    ("proto", json!("udp")),
                    ("dest_port", json!("67")),
                    ("family", json!("ipv4")),
                    ("target", json!("ACCEPT")),
                ]),
            );
        }
        if dhcp.is_some_and(|d| d.running) {
            put_fw(
                "rule",
                format!("{PREFIX}dns{vid}"),
                unmarked(&[
                    ("name", json!(format!("Steward: DNS on {zone}"))),
                    ("src", json!(zone)),
                    ("proto", json!(["tcp", "udp"])),
                    ("dest_port", json!("53")),
                    ("target", json!("ACCEPT")),
                ]),
            );
        }
        if fw.has("zone", "wan") {
            put_fw(
                "forwarding",
                format!("{PREFIX}fwd{vid}"),
                unmarked(&[("src", json!(zone)), ("dest", json!("wan"))]),
            );
        }
    } else {
        reject(
            plan,
            &at,
            &json!(format!("{PREFIX}zone{vid}")),
            "the firewall (fw4) isn't active on this device: the network is addressed, but gets no zone and isn't filtered",
        );
    }
    values
}

/// Refuses those of `keys` that `ipv4` has: DHCP serving that won't happen. Left out
/// silently, a reservation that was never made would be answered 0.
fn refuse(
    ipv4: &Value,
    keys: &[&str],
    path: &dyn Fn(&str) -> String,
    reason: &str,
    plan: &mut Plan,
) {
    for key in keys {
        if let Some(v) = ipv4.get(*key) {
            reject(plan, &path(key), v, reason);
        }
    }
}

/// `ipv4.dhcp` and `ipv4.dhcp-leases` on a static network: the pool and the reservations.
/// Whether it serves DHCP.
#[allow(clippy::too_many_arguments)]
fn serve_dhcp(
    ipv4: &Value,
    path: &dyn Fn(&str) -> String,
    subnet: &Subnet,
    network: &str,
    vid: u16,
    dhcp: Option<&Sections>,
    wanted: &mut Vec<(String, String)>,
    plan: &mut Plan,
) -> bool {
    let d = &ipv4["dhcp"];
    // A refused pool refuses the reservations too: nothing would serve them.
    let no_pool = "reservations need the DHCP pool, which was refused";
    let Some(dhcp) = dhcp.filter(|d| d.running) else {
        let reason = "dnsmasq isn't running on this device, so nothing would serve DHCP";
        reject(plan, &path("dhcp"), d, reason);
        refuse(ipv4, &["dhcp-leases"], path, reason, plan);
        return false;
    };
    if !d.is_object() {
        reject(plan, &path("dhcp"), d, "dhcp is an object");
        refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
        return false;
    }
    unsupported(plan, &path("dhcp"), d, &DHCP_KEYS);
    // Only a missing number takes dnsmasq's default: -5, "10" or 2.5 are refused.
    let mut number = |key: &str, default: u64| match d.get(key) {
        None => Some(default),
        Some(v) => {
            let n = v.as_u64();
            if n.is_none() {
                let reason = format!("{key} is a whole number, 0 or more");
                reject(plan, &path(&format!("dhcp/{key}")), v, reason);
            }
            n
        }
    };
    let (Some(first), Some(count)) = (number("lease-first", 100), number("lease-count", 150))
    else {
        refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
        return false;
    };
    // As with the numbers: only a missing lease time takes the default, and a bad one refuses
    // the pool rather than serve it for 6 hours.
    let lease = match d.get("lease-time").map(lease_time) {
        None => "6h".to_string(),
        Some(Ok(l)) => l,
        Some(Err(reason)) => {
            reject(plan, &path("dhcp/lease-time"), &d["lease-time"], reason);
            refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
            return false;
        }
    };
    let (Some(start), Some(end)) = (
        subnet.host(first),
        count
            .checked_sub(1)
            .and_then(|c| first.checked_add(c))
            .and_then(|last| subnet.host(last)),
    ) else {
        reject(
            plan,
            &path("dhcp"),
            &json!({ "lease-first": first, "lease-count": count }),
            format!(
                "the pool doesn't fit {}/{}",
                subnet.network(),
                subnet.prefix
            ),
        );
        refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
        return false;
    };
    if (start..=end).contains(&subnet.address) {
        reject(
            plan,
            &path("dhcp"),
            &json!({ "lease-first": first, "lease-count": count }),
            format!(
                "the pool takes in the device's own address, {}",
                subnet.address
            ),
        );
        refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
        return false;
    }
    let mut values = vec![
        ("interface", json!(network)),
        ("dhcpv4", json!("server")),
        ("start", json!(first.to_string())),
        ("limit", json!(count.to_string())),
        ("leasetime", json!(lease)),
    ];
    if let Some(dns) = d.get("use-dns") {
        match ipv4_list(dns) {
            Some(list) => values.push(("dhcp_option", json!([format!("6,{}", list.join(","))]))),
            None => reject(
                plan,
                &path("dhcp/use-dns"),
                dns,
                "DNS servers are one or more IPv4 addresses (an empty list would stop dnsmasq)",
            ),
        }
    }
    let section = network.to_string();
    if let Err(reason) = put_owned(plan, "dhcp", dhcp, "dhcp", &section, marked(&values)) {
        reject(plan, &path("dhcp"), d, reason);
        refuse(ipv4, &["dhcp-leases"], path, no_pool, plan);
        return false;
    }
    wanted.push(("dhcp".into(), section));

    let leases = match ipv4.get("dhcp-leases") {
        Some(Value::Array(leases)) => leases.as_slice(),
        Some(v) => {
            reject(plan, &path("dhcp-leases"), v, "dhcp-leases is a list");
            &[]
        }
        None => &[],
    };
    let mut seen: Vec<(String, Ipv4Addr)> = vec![];
    for (n, l) in leases.iter().enumerate() {
        let at = path(&format!("dhcp-leases/{n}"));
        unsupported(plan, &at, l, &LEASE_KEYS);
        let mac = l
            .get("macaddr")
            .and_then(Value::as_str)
            .filter(|m| is_mac(m))
            .map(str::to_ascii_lowercase);
        let ip = l
            .get("static-lease-offset")
            .and_then(Value::as_u64)
            .and_then(|o| subnet.host(o));
        let (Some(mac), Some(ip)) = (mac, ip) else {
            reject(
                plan,
                &at,
                l,
                "a reservation is a MAC and an offset that lands inside the subnet",
            );
            continue;
        };
        if ip == subnet.address {
            reject(plan, &at, l, format!("{ip} is the device's own address"));
            continue;
        }
        if seen.iter().any(|(m, a)| *m == mac || *a == ip) {
            reject(plan, &at, l, "another reservation has this MAC or address");
            continue;
        }
        let mut host = vec![("mac", json!(mac)), ("ip", json!(ip.to_string()))];
        // A bad lease time refuses the reservation: it would take the pool's instead.
        if let Some(v) = l.get("lease-time") {
            match lease_time(v) {
                Ok(t) => host.push(("leasetime", json!(t))),
                Err(reason) => {
                    reject(plan, &format!("{at}/lease-time"), v, reason);
                    continue;
                }
            }
        }
        match l.get("publish-hostname") {
            None | Some(Value::Bool(true)) => {}
            Some(Value::Bool(false)) => reject(
                plan,
                &format!("{at}/publish-hostname"),
                &json!(false),
                "dnsmasq puts every client's name in DNS; keeping one out isn't supported",
            ),
            Some(other) => reject(
                plan,
                &format!("{at}/publish-hostname"),
                other,
                "publish-hostname is true or false",
            ),
        }
        let section = format!("{PREFIX}host{vid}_{n}");
        if let Err(reason) = put_owned(plan, "dhcp", dhcp, "host", &section, marked(&host)) {
            reject(plan, &at, l, reason);
            continue;
        }
        wanted.push(("dhcp".into(), section));
        seen.push((mac, ip));
    }
    true
}

/// Steward's `dns-records` (uCentral has none): `{name, type, value}` → dnsmasq `domain`
/// (A, AAAA) and `cname` sections. A CNAME dnsmasq wouldn't start with is refused: a second
/// one for an alias, or one that closes a loop, counting the device's own `cname` sections.
pub(crate) fn dns_records(
    config: &Value,
    dhcp: Option<&Sections>,
    wanted: &mut Vec<(String, String)>,
    plan: &mut Plan,
) {
    let records = match config.get("dns-records") {
        None => return,
        Some(Value::Array(a)) => a,
        Some(other) => return reject(plan, "/dns-records", other, "dns-records is a list"),
    };
    let Some(dhcp) = dhcp.filter(|d| d.running) else {
        reject(
            plan,
            "/dns-records",
            &json!(records.len()),
            "dnsmasq isn't running on this device, so nothing would answer for the records",
        );
        return;
    };
    let (mut hosts, mut cnames) = (0, 0);
    // dnsmasq's CNAMEs, alias → target, in lower case: it compares names case-insensitively,
    // and won't start with two CNAMEs for one alias or with a loop. The device's own count.
    let mut aliases: BTreeMap<String, String> = dhcp
        .cnames
        .iter()
        .map(|(a, t)| (a.to_ascii_lowercase(), t.to_ascii_lowercase()))
        .collect();
    for (n, r) in records.iter().enumerate() {
        let at = format!("/dns-records/{n}");
        unsupported(plan, &at, r, &RECORD_KEYS);
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| is_hostname(s));
        let kind = match r.get("type") {
            None => "A",
            Some(Value::String(k)) => k.as_str(),
            Some(other) => {
                reject(
                    plan,
                    &format!("{at}/type"),
                    other,
                    "a record is A, AAAA or CNAME",
                );
                continue;
            }
        };
        let value = r
            .get("value")
            .and_then(Value::as_str)
            .filter(|s| printable(s));
        let (Some(name), Some(value)) = (name, value) else {
            reject(plan, &at, r, "a record has a DNS name and a value");
            continue;
        };
        let mut alias = None;
        let (section, kind, values) = match kind {
            "A" if value.parse::<Ipv4Addr>().is_ok() => {
                hosts += 1;
                (
                    format!("{PREFIX}dns{}", hosts - 1),
                    "domain",
                    vec![("name", json!(name)), ("ip", json!(value))],
                )
            }
            "AAAA" if value.parse::<Ipv6Addr>().is_ok() => {
                hosts += 1;
                (
                    format!("{PREFIX}dns{}", hosts - 1),
                    "domain",
                    vec![("name", json!(name)), ("ip", json!(value))],
                )
            }
            "CNAME" if is_hostname(value) => {
                let (a, t) = (name.to_ascii_lowercase(), value.to_ascii_lowercase());
                if aliases.contains_key(&a) {
                    reject(
                        plan,
                        &at,
                        r,
                        format!(
                            "{name} already has a CNAME, and dnsmasq won't start with two for one name"
                        ),
                    );
                    continue;
                }
                if loops(&aliases, &a, &t) {
                    reject(
                        plan,
                        &at,
                        r,
                        format!(
                            "a CNAME from {name} to {value} closes a loop, and dnsmasq won't start with one"
                        ),
                    );
                    continue;
                }
                alias = Some((a, t));
                cnames += 1;
                (
                    format!("{PREFIX}cname{}", cnames - 1),
                    "cname",
                    vec![("cname", json!(name)), ("target", json!(value))],
                )
            }
            "A" | "AAAA" | "CNAME" => {
                reject(
                    plan,
                    &format!("{at}/value"),
                    &json!(value),
                    format!("not a value for a {kind} record"),
                );
                continue;
            }
            other => {
                reject(
                    plan,
                    &format!("{at}/type"),
                    &json!(other),
                    "a record is A, AAAA or CNAME",
                );
                continue;
            }
        };
        if let Err(reason) = put_owned(plan, "dhcp", dhcp, kind, &section, marked(&values)) {
            reject(plan, &at, r, reason);
            continue;
        }
        wanted.push(("dhcp".into(), section));
        aliases.extend(alias);
    }
}

/// Whether a CNAME `alias` → `target` closes a loop through `aliases` (alias → target, all in
/// lower case), as dnsmasq follows them at start: `a` → `a` does, and so do `a` → `b` with
/// `b` → `a`.
fn loops(aliases: &BTreeMap<String, String>, alias: &str, target: &str) -> bool {
    let mut at = target;
    // Each step follows an alias of its own: more steps than aliases means a loop already.
    for _ in 0..=aliases.len() {
        if at == alias {
            return true;
        }
        match aliases.get(at) {
            Some(next) => at = next,
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnets_know_their_hosts() {
        let s = Subnet::parse("192.0.2.1/24").unwrap();
        assert_eq!(s.network(), Ipv4Addr::new(192, 0, 2, 0));
        assert_eq!(s.host(100), Some(Ipv4Addr::new(192, 0, 2, 100)));
        assert_eq!(s.host(254), Some(Ipv4Addr::new(192, 0, 2, 254)));
        assert_eq!(s.host(255), None, "the broadcast address");
        assert_eq!(s.host(0), None, "the network address");
        // Offsets that would overflow (panic in debug, wrap in release) are outside.
        assert_eq!(s.host(u64::MAX), None);
        assert_eq!(
            s.host(u64::MAX - u64::from(u32::from(s.network())) + 101),
            None
        );
        for bad in [
            "192.0.2.0/24",
            "192.0.2.255/24",
            "192.0.2.1",
            "192.0.2.1/31",
            "auto/24",
            "x/24",
        ] {
            assert!(Subnet::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn lease_times_are_dnsmasqs() {
        for good in [
            "6h", "30m", "3600", "1d", "2w", "infinite", "120", "2m", "365d", "52w", "8760h",
            "31536000", "0120",
        ] {
            assert_eq!(lease_time(&json!(good)).as_deref(), Ok(good));
        }
        assert_eq!(lease_time(&json!(600)).as_deref(), Ok("600"));
        for bad in ["", "h", "6hh", "6 h", "six", "6H", "-1", " 6h", "1.5h"] {
            assert!(
                lease_time(&json!(bad)).is_err_and(|r| r.contains("a number with")),
                "{bad}"
            );
        }
        // dnsmasq raises anything under 2 minutes to 2 minutes, and multiplies by the unit in
        // a C int: past a year is refused, and so is what would overflow.
        for bad in [
            "0",
            "119",
            "1m",
            "30s",
            "53w",
            "366d",
            "31536001",
            "4294967296s",
            "99999999999999999999h",
        ] {
            assert!(
                lease_time(&json!(bad)).is_err_and(|r| r.contains("2 minutes to a year")),
                "{bad}"
            );
        }
        assert!(lease_time(&json!(0)).is_err());
        assert!(lease_time(&json!(-5)).is_err());
        assert!(lease_time(&json!(true)).is_err());
    }

    #[test]
    fn static_addresses_are_read_as_netifd_reads_them() {
        let s = |a: &str, m: Option<&str>| {
            static_span(a, m).map(|(f, l)| (Ipv4Addr::from(f), Ipv4Addr::from(l)))
        };
        let lan = Some((Ipv4Addr::new(192, 0, 2, 0), Ipv4Addr::new(192, 0, 2, 255)));
        assert_eq!(s("192.0.2.1", Some("255.255.255.0")), lan);
        assert_eq!(s("192.0.2.1", Some("24")), lan);
        assert_eq!(s("192.0.2.1/24", None), lan);
        // The prefix wins over the netmask.
        assert_eq!(s("192.0.2.1/24", Some("255.255.0.0")), lan);
        // Alone: /32.
        let one = Ipv4Addr::new(192, 0, 2, 7);
        assert_eq!(s("192.0.2.7", None), Some((one, one)));
        assert_eq!(
            s("192.0.2.7", Some("0")),
            Some((Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST))
        );
        // Anything netifd may read otherwise isn't read.
        for (a, m) in [
            ("192.0.2.1", Some("030")),
            ("192.0.2.1", Some("0x18")),
            ("192.0.2.1", Some("33")),
            ("192.0.2.1", Some("255.0.255.0")),
            ("192.0.2.1", Some("255.255.255")),
            ("192.0.2.1/24", Some("x")),
            ("192.0.2.1/33", None),
            ("192.0.2.01", None),
            ("192.0.2", None),
            ("", None),
        ] {
            assert_eq!(s(a, m), None, "{a} {m:?}");
        }
    }

    #[test]
    fn dns_names_are_checked() {
        assert!(is_hostname("nas.home.arpa") && is_hostname("printer"));
        for bad in ["", "-a", "a..b", "a b", "a_b", "x\ny"] {
            assert!(!is_hostname(bad), "{bad:?}");
        }
    }

    #[test]
    fn cname_loops_are_found() {
        let aliases: BTreeMap<String, String> = [("b", "c"), ("c", "d"), ("x", "y"), ("y", "x")]
            .map(|(a, t)| (a.to_string(), t.to_string()))
            .into();
        assert!(loops(&aliases, "a", "a"), "itself");
        assert!(loops(&aliases, "d", "b"), "d → b → c → d");
        assert!(!loops(&aliases, "a", "b"), "a → b → c → d ends");
        assert!(!loops(&aliases, "e", "f"));
        // A loop already there (dnsmasq's dead anyway) ends the walk.
        assert!(loops(&aliases, "a", "x"));
    }
}
