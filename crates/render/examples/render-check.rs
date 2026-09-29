//! render-check <config.json> [--hostapd <out.json>]: renders a uCentral configuration against
//! this device's configs (wireless, network, and poe, dhcp and firewall where it has them) and
//! whether dnsmasq and fw4 run, as the agent does, and stages the result in an rpcd session of
//! its own, prints the staged changes (secrets redacted) and the rejections, then discards it
//! all: nothing is applied, nothing reloads.
//!
//! `--hostapd` also writes the SSIDs' planned options, with their bands, for
//! `.ai/skills/device-testing/hostapd-check.uc`. That file holds the secrets: remove it after.
use serde_json::{Map, Value, json};
use steward_render::{
    Current, Network, Op, Poe, Ports, Sections, Usteer, Wireless, dnsmasq_running, firewall_active,
    render,
};
use steward_ubus::Ubus;
use steward_ubus::uci::Transaction;

/// Whether an option holds a secret: keys, RADIUS secrets, 802.11r key holders.
fn secret(option: &str) -> bool {
    option == "key"
        || option.contains("secret")
        || option.contains("password")
        || option.ends_with("kh")
}

/// Whether a config's service runs, as it's printed.
fn state(s: Option<&Sections>, running: &str) -> String {
    match s {
        None => "no config".into(),
        Some(s) if s.running => running.into(),
        Some(_) => format!("not {running}"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("config.json");
    let hostapd = args
        .iter()
        .position(|a| a == "--hostapd")
        .and_then(|i| args.get(i + 1));
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut ubus = Ubus::connect().unwrap();
    let mut get = |name: &str| {
        ubus.call("uci", "get", json!({ "config": name }).as_object().unwrap())
            .unwrap()
    };
    let (wireless, network) = (get("wireless"), get("network"));
    let mut read = |name: &str| {
        ubus.call("uci", "get", json!({ "config": name }).as_object().unwrap())
            .ok()
    };
    let poe = read("poe").map(|p| Poe::from_uci(&p));
    let mut dhcp = read("dhcp").map(|a| Sections::from_uci(&a, true));
    let mut firewall = read("firewall").map(|a| Sections::from_uci(&a, false));
    // As the agent does (steward-agent/src/apply.rs, `service`): dnsmasq counts while it runs,
    // fw4 while it's active.
    let mut service = |name: &str| {
        ubus.call(
            "service",
            "list",
            json!({ "name": name }).as_object().unwrap(),
        )
        .unwrap_or_default()
    };
    if let Some(d) = &mut dhcp {
        d.running = dnsmasq_running(&service("dnsmasq"));
    }
    if let Some(f) = &mut firewall {
        f.running = firewall_active(&service("firewall"));
    }
    println!(
        "dnsmasq: {}; firewall (fw4): {}",
        state(dhcp.as_ref(), "running"),
        state(firewall.as_ref(), "active")
    );
    let board: Value =
        serde_json::from_str(&std::fs::read_to_string("/etc/board.json").unwrap()).unwrap();
    let mut current = Wireless::from_uci(&wireless);
    // What each radio supports, as `iwinfo info` reports it for its phy.
    let status = ubus
        .call("network.wireless", "status", &Map::new())
        .unwrap_or_default();
    current.read_htmodes(&status, |device| {
        ubus.call(
            "iwinfo",
            "info",
            json!({ "device": device }).as_object().unwrap(),
        )
        .ok()
    });
    for r in &current.radios {
        println!("radio {} ({}): htmodes {:?}", r.section, r.band, r.htmodes);
    }
    // As the agent does (steward-agent/src/apply.rs, `usteer`).
    if std::path::Path::new("/sbin/usteerd").exists() {
        let config = ubus.call("usteer", "get_config", &Map::new()).ok();
        current.usteer = Some(Usteer::from_config(config.as_ref()));
    }
    let plan = render(
        &config,
        &Current {
            wireless: &current,
            network: &Network::from_uci(&network),
            ports: &Ports::from_board(&board),
            poe: poe.as_ref(),
            dhcp: dhcp.as_ref(),
            firewall: firewall.as_ref(),
        },
    );
    for r in &plan.rejected {
        match &r.substitution {
            Some(s) => println!("substituted: {} ({}): {s}", r.parameter, r.reason),
            None => println!("rejected: {} ({})", r.parameter, r.reason),
        }
    }
    let mut configs = vec!["network", "wireless"];
    for (name, present) in [
        ("poe", poe.is_some()),
        ("dhcp", dhcp.is_some()),
        ("firewall", firewall.is_some()),
    ] {
        if present {
            configs.push(name);
        }
    }
    let mut t = Transaction::open(&configs).unwrap();
    for op in &plan.ops {
        let result = match op {
            Op::Add {
                config,
                kind,
                name,
                values,
            } => t.add(config, kind, name, Value::Object(values.clone())),
            Op::Set {
                config,
                section,
                values,
            } => t.set(config, section, Value::Object(values.clone())),
            Op::Unset {
                config,
                section,
                options,
            } => t.unset(config, section, options),
            Op::Delete { config, section } => t.delete(config, section),
        };
        if let Err(e) = result {
            println!("failed: {op}: {e}");
        }
    }
    let changes = t.changes().unwrap();
    for (config, list) in &changes {
        for change in list.as_array().unwrap_or(&vec![]) {
            // Keys are redacted: the staged changes are printed, not the secrets.
            let shown: Vec<String> = change
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect();
            let shown = match shown.get(2) {
                Some(option) if secret(option) => {
                    format!("{} {} {option} <redacted>", shown[0], shown[1])
                }
                _ => shown.join(" "),
            };
            println!("staged {config}: {shown}");
        }
    }
    let _: Map<String, Value> = changes;
    if let Some(out) = hostapd {
        let ssids: Vec<Value> = plan
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Add { config, values, .. } | Op::Set { config, values, .. }
                    if config == "wireless" && values.contains_key("ssid") =>
                {
                    let radio = values["device"].as_str().unwrap_or("");
                    let band = current.radios.iter().find(|r| r.section == radio)?;
                    Some(json!({ "band": band.band, "options": values }))
                }
                _ => None,
            })
            .collect();
        std::fs::write(out, serde_json::to_vec_pretty(&ssids).unwrap()).unwrap();
        println!("wrote {} SSID(s) to {out}", ssids.len());
    }
    drop(t); // the session goes, and its staged changes with it
    println!(
        "discarded; {} operations, {} rejections",
        plan.ops.len(),
        plan.rejected.len()
    );
}
