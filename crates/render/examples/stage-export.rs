//! stage-export <config.json> <out-dir> [--assume-running]: renders a uCentral configuration as
//! render-check does, stages it in an rpcd session of its own, and writes the staged `dhcp`,
//! `firewall` and `network` configs (as that session sees them) to <out-dir> as UCI text. Then
//! it destroys the session: nothing is committed or applied, nothing reloads.
//!
//! The files are for checking a plan with the device's own tools, offline: dnsmasq's init
//! script and `dnsmasq --test`, fw4's print mode and `nft -c` (`.ai/skills/device-testing`).
//! Wireless isn't written, so no key or secret is either.
//!
//! Like the agent, it renders pools, reservations, DNS records and zones only while dnsmasq
//! runs and fw4 is active. `--assume-running` renders as if both did, for checking them on a
//! device where they don't: the agent would refuse them there.
use serde_json::{Map, Value, json};
use steward_render::{
    Current, Network, Op, Poe, Ports, Sections, Usteer, Wireless, dnsmasq_running, firewall_active,
    render,
};
use steward_ubus::Ubus;

fn args(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Whether a config's service runs, as it's printed.
fn state(s: Option<&Sections>, running: &str) -> String {
    match s {
        None => "no config".into(),
        Some(s) if s.running => running.into(),
        Some(_) => format!("not {running}"),
    }
}

/// A UCI value quoted as the config files quote them.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A `uci get` answer's `values` as a config file, sections in order.
fn uci_text(values: &Map<String, Value>) -> String {
    let mut out = String::new();
    for (name, s) in values {
        let kind = s[".type"].as_str().unwrap_or("");
        if s[".anonymous"] == json!(true) {
            out.push_str(&format!("\nconfig {kind}\n"));
        } else {
            out.push_str(&format!("\nconfig {kind} {}\n", quote(name)));
        }
        for (k, v) in s.as_object().into_iter().flatten() {
            if k.starts_with('.') {
                continue;
            }
            match v {
                Value::Array(list) => {
                    for i in list {
                        out.push_str(&format!("\tlist {k} {}\n", quote(i.as_str().unwrap_or(""))));
                    }
                }
                v => out.push_str(&format!(
                    "\toption {k} {}\n",
                    quote(v.as_str().unwrap_or(""))
                )),
            }
        }
    }
    out
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let (Some(path), Some(out)) = (argv.get(1), argv.get(2)) else {
        eprintln!("usage: stage-export <config.json> <out-dir> [--assume-running]");
        std::process::exit(2);
    };
    let assume = argv.iter().skip(3).any(|a| a == "--assume-running");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let out = std::path::Path::new(out);
    let mut ubus = Ubus::connect().unwrap();
    let mut read = |name: &str| {
        ubus.call("uci", "get", &args(json!({ "config": name })))
            .ok()
    };
    let (wireless, network) = (read("wireless").unwrap(), read("network").unwrap());
    let poe = read("poe").map(|p| Poe::from_uci(&p));
    let mut dhcp = read("dhcp").map(|a| Sections::from_uci(&a, true));
    let mut firewall = read("firewall").map(|a| Sections::from_uci(&a, false));
    // As the agent does (steward-agent/src/apply.rs, `service`), unless told to assume.
    let mut service = |name: &str| {
        ubus.call("service", "list", &args(json!({ "name": name })))
            .unwrap_or_default()
    };
    if let Some(d) = &mut dhcp {
        d.running = assume || dnsmasq_running(&service("dnsmasq"));
    }
    if let Some(f) = &mut firewall {
        f.running = assume || firewall_active(&service("firewall"));
    }
    let assumed = if assume {
        " (assumed: --assume-running; the agent would ask procd)"
    } else {
        ""
    };
    println!(
        "dnsmasq: {}; firewall (fw4): {}{assumed}",
        state(dhcp.as_ref(), "running"),
        state(firewall.as_ref(), "active")
    );
    let board: Value =
        serde_json::from_str(&std::fs::read_to_string("/etc/board.json").unwrap()).unwrap();
    let mut current = Wireless::from_uci(&wireless);
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
        println!("rejected: {} ({})", r.parameter, r.reason);
    }
    // A session of its own, granted the configs the agent's transaction would be (as
    // uci::Transaction does, without applying).
    let sid = ubus
        .call("session", "create", &args(json!({ "timeout": 120 })))
        .unwrap()["ubus_rpc_session"]
        .as_str()
        .unwrap()
        .to_owned();
    let objects: Vec<Value> = ["network", "wireless", "poe", "dhcp", "firewall"]
        .iter()
        .flat_map(|c| [json!([c, "read"]), json!([c, "write"])])
        .collect();
    ubus.call(
        "session",
        "grant",
        &args(json!({ "ubus_rpc_session": sid, "scope": "uci", "objects": objects })),
    )
    .unwrap();
    let mut failed = 0;
    for op in &plan.ops {
        let (method, call, config, section) = match op {
            Op::Add {
                config,
                kind,
                name,
                values,
            } => (
                "add",
                json!({ "config": config, "type": kind, "name": name, "values": values }),
                config,
                name,
            ),
            Op::Set {
                config,
                section,
                values,
            } => (
                "set",
                json!({ "config": config, "section": section, "values": values }),
                config,
                section,
            ),
            Op::Unset {
                config,
                section,
                options,
            } => (
                "delete",
                json!({ "config": config, "section": section, "options": options }),
                config,
                section,
            ),
            Op::Delete { config, section } => (
                "delete",
                json!({ "config": config, "section": section }),
                config,
                section,
            ),
        };
        let mut call = args(call);
        call.insert("ubus_rpc_session".into(), json!(sid));
        if let Err(e) = ubus.call("uci", method, &call) {
            failed += 1;
            // The section, not its values: a wireless one holds keys.
            println!("failed: {method} {config}.{section}: {e}");
        }
    }
    let changes = ubus
        .call("uci", "changes", &args(json!({ "ubus_rpc_session": sid })))
        .unwrap();
    for (config, list) in changes["changes"].as_object().into_iter().flatten() {
        let n = list.as_array().map_or(0, Vec::len);
        println!("staged {config}: {n} changes");
    }
    std::fs::create_dir_all(out).unwrap();
    for name in ["dhcp", "firewall", "network"] {
        let Ok(staged) = ubus.call(
            "uci",
            "get",
            &args(json!({ "config": name, "ubus_rpc_session": sid })),
        ) else {
            println!("{name}: not on this device");
            continue;
        };
        let text = uci_text(staged["values"].as_object().unwrap_or(&Map::new()));
        std::fs::write(out.join(name), text).unwrap();
        println!("wrote {}", out.join(name).display());
    }
    ubus.call(
        "session",
        "destroy",
        &args(json!({ "ubus_rpc_session": sid })),
    )
    .unwrap();
    println!(
        "discarded; {} operations, {failed} failed, {} rejections",
        plan.ops.len(),
        plan.rejected.len()
    );
}
