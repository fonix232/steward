//! rollback-check <config>: stages a section in <config> (which must exist and
//! be one no service watches), applies it with a 10 s rollback and checks rpcd
//! reverts it; then applies another and confirms it. Leaves the confirmed one.
use serde_json::json;
use std::{process::Command, thread::sleep, time::Duration};
use steward_ubus::uci::Transaction;

fn live(config: &str, section: &str) -> String {
    let out = Command::new("uci")
        .args(["-q", "get", &format!("{config}.{section}.value")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn main() {
    let config = std::env::args().nth(1).expect("config");
    let mut t = Transaction::open(&[&config]).unwrap();
    t.add(&config, "probe", "p1", json!({ "value": "one" }))
        .unwrap();
    println!("staged: {:?}", t.changes().unwrap());
    t.apply(Duration::from_secs(10)).unwrap();
    println!("after apply: p1={:?}", live(&config, "p1"));
    sleep(Duration::from_secs(14));
    println!("unconfirmed, 14 s later: p1={:?}", live(&config, "p1"));
    drop(t);

    let mut t = Transaction::open(&[&config]).unwrap();
    t.add(&config, "probe", "p2", json!({ "value": "two" }))
        .unwrap();
    t.apply(Duration::from_secs(10)).unwrap();
    t.confirm().unwrap();
    sleep(Duration::from_secs(12));
    println!("confirmed, 12 s later: p2={:?}", live(&config, "p2"));
    println!(
        "other config refused: {:?}",
        t.set("network", "lan", json!({ "proto": "none" })).err()
    );
}
