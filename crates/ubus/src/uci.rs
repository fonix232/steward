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
    /// A session of its own, granted nothing.
    fn create() -> Result<Transaction> {
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
        Ok(Transaction { ubus, sid })
    }

    /// Opens a session that may read and change `configs`, and nothing else.
    pub fn open(configs: &[&str]) -> Result<Transaction> {
        let objects: Vec<Value> = configs
            .iter()
            .flat_map(|c| [json!([c, "read"]), json!([c, "write"])])
            .collect();
        let mut t = Transaction::create()?;
        t.session("grant", json!({ "scope": "uci", "objects": objects }))?;
        Ok(t)
    }

    /// Whether an apply with a rollback is waiting for its confirmation on the device (a LuCI
    /// Save & Apply, another transaction's), asked without touching it. rpcd's `confirm`
    /// (`rpc_uci_confirm` in rpcd's uci.c) answers NoData when nothing is pending, and
    /// PermissionDenied, changing nothing, when the session asking isn't the one that
    /// applied. It's asked from a fresh session, which has applied nothing, so it can never
    /// confirm anything.
    pub fn pending() -> Result<bool> {
        pending_from(Transaction::create()?.confirm())
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

    /// `config` as this session sees it: what's committed, with the changes staged here.
    /// Same shape as `uci get` (`{"values": {section: {...}}}`).
    pub fn get(&mut self, config: &str) -> Result<Map<String, Value>> {
        self.uci("get", json!({ "config": config }))
    }

    /// Removes options from a section.
    pub fn unset(&mut self, config: &str, section: &str, options: &[String]) -> Result<()> {
        self.uci(
            "delete",
            json!({ "config": config, "section": section, "options": options }),
        )
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

/// What `confirm` from a session that has applied nothing says: NoData, nothing is pending;
/// PermissionDenied, another session's apply is.
fn pending_from(confirm: Result<()>) -> Result<bool> {
    match confirm {
        Err(Error::Status(Status::NoData)) => Ok(false),
        Err(Error::Status(Status::PermissionDenied)) => Ok(true),
        // Only the applying session's confirm succeeds, and this one applied nothing.
        Ok(()) => Err(Error::Status(Status::UnknownError)),
        Err(e) => Err(e),
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_from_a_session_that_applied_nothing_tells_whether_an_apply_is_pending() {
        let status = |s| Err(Error::Status(s));
        assert!(!pending_from(status(Status::NoData)).unwrap());
        assert!(pending_from(status(Status::PermissionDenied)).unwrap());
        assert!(pending_from(Ok(())).is_err());
        assert!(pending_from(status(Status::Timeout)).is_err());
    }
}
