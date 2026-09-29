//! rollback-check <config>: stages a section in <config> (which must exist and
//! be one no service watches), applies it with a 10 s rollback and checks rpcd
//! reverts it; then applies another and confirms it. Leaves the confirmed one.
//! While the first is pending, another apply is refused, and a fresh session
//! sees it pending (`Transaction::pending`) without touching it. An apply its
//! own session rolls back is reverted at once. Last, sets a list to the value
//! it has, which stages a change but reads back the same through the session:
//! how the agent tells a no-op.
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
    // While it's pending, another session's apply is refused: what the agent sees while a
    // LuCI Save & Apply is pending, and waits out.
    let mut other = Transaction::open(&[&config]).unwrap();
    other
        .add(&config, "probe", "p0", json!({ "value": "zero" }))
        .unwrap();
    println!(
        "another apply while it's pending: {:?}",
        other.apply(Duration::from_secs(10)).err()
    );
    drop(other);
    // Asked from a fresh session, which can't confirm it: pending (PermissionDenied).
    println!("pending while it is: {:?}", Transaction::pending());
    sleep(Duration::from_secs(14));
    println!("unconfirmed, 14 s later: p1={:?}", live(&config, "p1"));
    println!("pending after the revert: {:?}", Transaction::pending());
    drop(t);

    // Rolled back by the session that applied it: reverted at once, nothing left pending.
    let mut t = Transaction::open(&[&config]).unwrap();
    t.add(&config, "probe", "p3", json!({ "value": "three" }))
        .unwrap();
    t.apply(Duration::from_secs(10)).unwrap();
    println!("after apply: p3={:?}", live(&config, "p3"));
    println!(
        "rolled back by its session: {:?}, p3={:?}, pending: {:?}",
        t.rollback(),
        live(&config, "p3"),
        Transaction::pending()
    );
    drop(t);

    let mut t = Transaction::open(&[&config]).unwrap();
    t.add(
        &config,
        "probe",
        "p2",
        json!({ "value": "two", "ports": ["lan1:t", "wan:t"] }),
    )
    .unwrap();
    t.apply(Duration::from_secs(10)).unwrap();
    t.confirm().unwrap();
    sleep(Duration::from_secs(12));
    println!("confirmed, 12 s later: p2={:?}", live(&config, "p2"));
    println!(
        "other config refused: {:?}",
        t.set("network", "lan", json!({ "proto": "none" })).err()
    );
    drop(t);

    let mut t = Transaction::open(&[&config]).unwrap();
    let before = t.get(&config).unwrap();
    t.set(&config, "p2", json!({ "ports": ["lan1:t", "wan:t"] }))
        .unwrap();
    let staged = t.changes().unwrap();
    let count = staged
        .get(&config)
        .and_then(|c| c.as_array())
        .map_or(0, Vec::len);
    println!(
        "same list set again: {count} change(s) staged, reads back the same: {}",
        t.get(&config).unwrap() == before
    );
    t.set(&config, "p2", json!({ "value": "three" })).unwrap();
    println!(
        "a new value staged: the session reads {}, the config still {:?}",
        t.get(&config).unwrap()["values"]["p2"]["value"],
        live(&config, "p2")
    );
}
