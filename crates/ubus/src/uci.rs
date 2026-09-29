//! UCI changes through rpcd, applied with a rollback.
//!
//! A [`Transaction`] stages changes in an rpcd session of its own, so they
//! never mix with a LuCI user's, and puts them live with `uci apply
//! {rollback}`: rpcd reverts them unless [`Transaction::confirm`] comes
//! within the timeout. The agent confirms once the controller is reachable
//! again, so a configuration that cuts the device off undoes itself.

use crate::{Error, Result, Status, Ubus};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// How long an rpcd session lives without use.
const SESSION_TIMEOUT: u64 = 600;

pub struct Transaction {
    // Its own connection: a transaction lives until it is confirmed, while
    // the agent goes on using ubus.
    ubus: Ubus,
    sid: String,
}

impl Transaction {
    /// Opens a session that may read and change `configs`, and nothing else.
    pub fn open(configs: &[&str]) -> Result<Transaction> {
        let mut ubus = Ubus::connect()?;
        let created = ubus.call(
            "session",
            "create",
            &obj(json!({ "timeout": SESSION_TIMEOUT })),
        )?;
        let sid = created
            .get("ubus_rpc_session")
            .and_then(Value::as_str)
            .ok_or(Error::Status(Status::NoData))?
            .to_owned();
        let objects: Vec<Value> = configs
            .iter()
            .flat_map(|c| [json!([c, "read"]), json!([c, "write"])])
            .collect();
        let mut t = Transaction { ubus, sid };
        t.session("grant", json!({ "scope": "uci", "objects": objects }))?;
        Ok(t)
    }

    fn session(&mut self, method: &str, args: Value) -> Result<Map<String, Value>> {
        let mut args = obj(args);
        args.insert("ubus_rpc_session".into(), Value::String(self.sid.clone()));
        self.ubus.call("session", method, &args)
    }

    fn uci(&mut self, method: &str, args: Value) -> Result<Map<String, Value>> {
        let mut args = obj(args);
        args.insert("ubus_rpc_session".into(), Value::String(self.sid.clone()));
        self.ubus.call("uci", method, &args)
    }

    /// Adds a section called `name` of `type`, with `values`.
    pub fn add(&mut self, config: &str, kind: &str, name: &str, values: Value) -> Result<()> {
        self.uci(
            "add",
            json!({ "config": config, "type": kind, "name": name, "values": values }),
        )
        .map(drop)
    }

    /// Sets `values` on an existing section.
    pub fn set(&mut self, config: &str, section: &str, values: Value) -> Result<()> {
        self.uci(
            "set",
            json!({ "config": config, "section": section, "values": values }),
        )
        .map(drop)
    }

    /// Deletes a section.
    pub fn delete(&mut self, config: &str, section: &str) -> Result<()> {
        self.uci("delete", json!({ "config": config, "section": section }))
            .map(drop)
    }

    /// The staged changes, per config, as `uci changes` lists them.
    pub fn changes(&mut self) -> Result<Map<String, Value>> {
        Ok(match self.uci("changes", json!({}))?.remove("changes") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        })
    }

    /// Puts the staged changes live; rpcd reverts them after `rollback`
    /// unless [`Transaction::confirm`] comes first. Fails while another
    /// apply with a rollback (LuCI's, say) is pending.
    pub fn apply(&mut self, rollback: Duration) -> Result<()> {
        self.uci(
            "apply",
            json!({ "rollback": true, "timeout": rollback.as_secs().max(1) }),
        )
        .map(drop)
    }

    /// Keeps what [`Transaction::apply`] put live.
    pub fn confirm(&mut self) -> Result<()> {
        self.uci("confirm", json!({})).map(drop)
    }

    /// Reverts what [`Transaction::apply`] put live, now.
    pub fn rollback(&mut self) -> Result<()> {
        self.uci("rollback", json!({})).map(drop)
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _ = self.session("destroy", json!({}));
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
