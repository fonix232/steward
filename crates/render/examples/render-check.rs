//! render-check <config.json>: renders a uCentral configuration against this device's
//! wireless config and stages the result in an rpcd session of its own, prints the staged
//! changes and the rejections, then discards it all: nothing is applied, no radio reloads.
use serde_json::{Map, Value, json};
use steward_render::{Op, wireless};
use steward_ubus::Ubus;
use steward_ubus::uci::Transaction;

fn main() {
    let path = std::env::args().nth(1).expect("config.json");
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut ubus = Ubus::connect().unwrap();
    let current = ubus
        .call(
            "uci",
            "get",
            json!({ "config": "wireless" }).as_object().unwrap(),
        )
        .unwrap();
    let mut current = steward_render::Wireless::from_uci(&current);
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
    let plan = wireless(&config, &current);
    for r in &plan.rejected {
        match &r.substitution {
            Some(s) => println!("substituted: {} ({}): {s}", r.parameter, r.reason),
            None => println!("rejected: {} ({})", r.parameter, r.reason),
        }
    }
    let mut t = Transaction::open(&["wireless"]).unwrap();
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
            let shown = if shown.get(2).map(String::as_str) == Some("key") {
                format!("{} {} key <redacted>", shown[0], shown[1])
            } else {
                shown.join(" ")
            };
            println!("staged {config}: {shown}");
        }
    }
    let _: Map<String, Value> = changes;
    drop(t); // the session goes, and its staged changes with it
    println!(
        "discarded; {} operations, {} rejections",
        plan.ops.len(),
        plan.rejected.len()
    );
}
