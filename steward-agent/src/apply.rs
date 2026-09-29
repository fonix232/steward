//! Applying a configuration: render it (`steward_render`), stage the changes in an rpcd session
//! of the agent's own, and put them live with rpcd's rollback. The caller confirms once the
//! controller is reachable again; otherwise it rolls them back at once ([`settle`]), and
//! should that fail, rpcd reverts them when its window ends.
//!
//! "Nothing to change" is answered only when no apply is pending on the device: one that is
//! (LuCI's Save & Apply, say) may revert what the configuration matched ([`nothing`]). The
//! agent asks rpcd with `confirm` from a fresh session ([`Transaction::pending`]): rpcd's
//! `rpc_uci_confirm` answers NoData when nothing is pending and PermissionDenied when another
//! session applied, and only the applying session's confirm changes anything.
//!
//! State kept in the agent's state directory:
//! - `running.json`: the uuid of the configuration the device runs, and the last one that
//!   rolled back (refused until the controller sends another, so it can't loop).
//! - `originals.json`: what the device's own sections (radios, PoE ports) held before the agent
//!   first set them (`"<config>.<section>.<option>"` → value, or null for unset), to restore
//!   them later. It replaces `radio-originals.json` (radios only), which moves in on first use.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use steward_proto::{CommandStatus, Rejection, configure_error};
use steward_render::{Current, Network, Op, Plan, Poe, Ports, Usteer, Wireless};
use steward_ubus::Ubus;
use steward_ubus::uci::Transaction;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Running {
    /// The configuration the device runs (0: none from a controller).
    pub uuid: u64,
    /// The last configuration that rolled back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<u64>,
}

fn write_json(path: &Path, v: &impl Serialize) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

pub struct State {
    dir: PathBuf,
}

impl State {
    pub fn new(dir: &Path) -> State {
        State {
            dir: dir.to_owned(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn running(&self) -> Running {
        read_json(&self.dir.join("running.json"))
    }

    pub fn set_running(&self, r: &Running) -> Result<(), String> {
        write_json(&self.dir.join("running.json"), r)
    }

    fn originals_path(&self) -> PathBuf {
        self.dir.join("originals.json")
    }

    /// `originals.json`. An agent from before PoE kept only radios' originals, in
    /// `radio-originals.json` (`"<section>.<option>"`): on first use they move in as
    /// `wireless.<section>.<option>`, and the old file goes. They win over the same key in the
    /// new file, which can only hold a value the agent had already set, recorded by an upgraded
    /// agent that didn't know the old file.
    fn originals(&self) -> Result<Map<String, Value>, String> {
        let mut known: Map<String, Value> = read_json(&self.originals_path());
        let old = self.dir.join("radio-originals.json");
        if !old.exists() {
            return Ok(known);
        }
        let radios: Map<String, Value> = read_json(&old);
        for (key, value) in radios {
            known.insert(format!("wireless.{key}"), value);
        }
        write_json(&self.originals_path(), &known)?;
        std::fs::remove_file(&old).map_err(|e| format!("{}: {e}", old.display()))?;
        Ok(known)
    }
}

/// Adds to `known` the original value of every option on the device's own sections the plan
/// sets and `known` doesn't hold yet: the first value the agent replaced is the one to restore.
/// `current` is each config's `uci get` answer.
pub fn record_originals(
    known: &mut Map<String, Value>,
    plan: &Plan,
    current: &BTreeMap<&str, Map<String, Value>>,
) {
    for (config, section, option) in &plan.device_options {
        let key = format!("{config}.{section}.{option}");
        if !known.contains_key(&key) {
            let value = current
                .get(config.as_str())
                .and_then(|c| c.get("values"))
                .and_then(|v| v.get(section))
                .and_then(|s| s.get(option))
                .cloned();
            known.insert(key, value.unwrap_or(Value::Null));
        }
    }
}

/// The answer to `configure`, from the rejections and whether the rest was applied.
pub fn status(rejected: Vec<Rejection>, applied: bool, text: &str) -> CommandStatus {
    let error = match (applied, rejected.is_empty()) {
        (false, _) => configure_error::REJECTED,
        (true, true) => configure_error::APPLIED,
        (true, false) => configure_error::APPLIED_WITH_SUBSTITUTIONS,
    };
    CommandStatus {
        error,
        text: text.into(),
        when: None,
        rejected,
    }
}

/// usteer as it runs: `None` when it isn't installed. What it steers is asked of the daemon, not
/// read from its UCI: sections later in `/etc/config/usteer` replace earlier ones' lists.
pub fn usteer(ubus: &mut Ubus) -> Option<Usteer> {
    if !Path::new("/sbin/usteerd").exists() {
        return None;
    }
    let config = ubus.call("usteer", "get_config", &Map::new()).ok();
    Some(Usteer::from_config(config.as_ref()))
}

/// What staging a configuration came to.
pub enum Staged {
    /// Nothing to change: the device already runs it, and no apply is pending that could
    /// revert it.
    Nothing(Plan),
    /// Live, awaiting confirmation ([`settle`]).
    Applied(Plan, Transaction),
    /// Not applied: another change is waiting for confirmation (LuCI's Save & Apply, say), and
    /// rpcd allows one at a time. Nothing changed; try again.
    Busy,
}

/// Nothing to change, unless an apply is waiting for its confirmation (`pending`, asked of
/// rpcd by [`Transaction::pending`]). What's live then is on rpcd's rollback timer, whether
/// it's a LuCI Save & Apply or the agent's own that it couldn't revert, and may be reverted
/// right after this is answered 0. So it's busy, like an apply rpcd refuses: tried again.
fn nothing(
    plan: Plan,
    pending: impl FnOnce() -> steward_ubus::Result<bool>,
) -> Result<Staged, String> {
    match pending() {
        Ok(false) => Ok(Staged::Nothing(plan)),
        Ok(true) => Ok(Staged::Busy),
        Err(e) => Err(format!("asking rpcd whether a change is pending: {e}")),
    }
}

/// Why staging `op` failed. The op is shown by name ([`Op`]'s Display), never with its
/// values: the error is logged and sent as `configure`'s answer, and values hold keys.
fn staging_failed(op: &Op, e: impl std::fmt::Display) -> String {
    format!("staging {op}: {e}")
}

/// Whether staging left each config in `before` (its name, and its `uci get` from before
/// staging) as it was, read back through the session. rpcd stages a change for a list set to
/// the value it has (a delete and a re-add), so an empty `uci changes` alone would miss a
/// no-op and reload the radios for nothing.
fn unchanged(t: &mut Transaction, before: &[(&str, &Map<String, Value>)]) -> Result<bool, String> {
    for (config, was) in before {
        let now = t
            .get(config)
            .map_err(|e| format!("reading the staged {config} config: {e}"))?;
        if now.get("values") != was.get("values") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Renders `config` against the device's wireless and network configs, records the radio
/// options it's about to replace, stages the changes to both in one transaction and applies
/// them with a `rollback` window. Blocking.
pub fn stage(state: &State, config: &Value, rollback: Duration) -> Result<Staged, String> {
    let mut ubus = Ubus::connect().map_err(|e| e.to_string())?;
    let mut get = |name: &str| {
        ubus.call("uci", "get", json!({ "config": name }).as_object().unwrap())
            .map_err(|e| format!("reading the {name} config: {e}"))
    };
    let current = get("wireless")?;
    let network_uci = get("network")?;
    let network = Network::from_uci(&network_uci);
    // realtek-poe's config, where the device powers ports.
    let poe_config = get("poe").ok();
    let poe = poe_config.as_ref().map(Poe::from_uci);
    let board: Value = std::fs::read_to_string("/etc/board.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut wireless = Wireless::from_uci(&current);
    // What each radio runs (`iwinfo info` on its phy), so it's never set to a mode it lacks.
    let status = ubus
        .call("network.wireless", "status", &Map::new())
        .unwrap_or_default();
    wireless.read_htmodes(&status, |device| {
        ubus.call(
            "iwinfo",
            "info",
            json!({ "device": device }).as_object().unwrap(),
        )
        .ok()
    });
    wireless.usteer = usteer(&mut ubus);
    let plan = steward_render::render(
        config,
        &Current {
            wireless: &wireless,
            network: &network,
            ports: &Ports::from_board(&board),
            poe: poe.as_ref(),
        },
    );
    if plan.ops.is_empty() {
        return nothing(plan, Transaction::pending);
    }
    let configs: &[&str] = if poe.is_some() {
        &["network", "wireless", "poe"]
    } else {
        &["network", "wireless"]
    };
    let mut t = Transaction::open(configs).map_err(|e| e.to_string())?;
    for op in &plan.ops {
        match op {
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
        }
        .map_err(|e| staging_failed(op, e))?;
    }
    // Every config staged, read back: poe too, or a change only to it would pass for none.
    let mut staged = vec![("network", &network_uci), ("wireless", &current)];
    if let Some(p) = &poe_config {
        staged.push(("poe", p));
    }
    if unchanged(&mut t, &staged)? {
        // Every value was already what the plan sets.
        return nothing(plan, Transaction::pending);
    }
    let before = state.originals()?;
    let mut originals = before.clone();
    let mut answers = BTreeMap::from([("wireless", current)]);
    if let Some(p) = poe_config {
        answers.insert("poe", p);
    }
    record_originals(&mut originals, &plan, &answers);
    write_json(&state.originals_path(), &originals)?;
    if let Err(e) = t.apply(rollback) {
        // Nothing changed, so nothing was replaced: the next try records what's there then.
        write_json(&state.originals_path(), &before)?;
        return match e {
            steward_ubus::Error::Status(steward_ubus::Status::PermissionDenied) => Ok(Staged::Busy),
            e => Err(format!("applying: {e}")),
        };
    }
    Ok(Staged::Applied(plan, t))
}

/// What became of an applied configuration.
#[derive(Debug, PartialEq)]
pub enum Settled {
    /// Confirmed: it stays.
    Kept,
    /// Reverted by the agent, at once.
    Reverted,
    /// Neither: rpcd reverts it when its window ends. Until then the pending apply makes the
    /// next configuration wait ([`nothing`]).
    Pending,
}

/// An applied change, as [`settle`] needs it: rpcd's session ([`Transaction`]), or a
/// stand-in in tests.
pub trait Applied {
    fn confirm(&mut self) -> Result<(), String>;
    fn rollback(&mut self) -> Result<(), String>;
}

impl Applied for Transaction {
    fn confirm(&mut self) -> Result<(), String> {
        Transaction::confirm(self).map_err(|e| e.to_string())
    }

    fn rollback(&mut self) -> Result<(), String> {
        Transaction::rollback(self).map_err(|e| e.to_string())
    }
}

/// Confirms what was applied when the controller answered (`reachable`); otherwise, or when
/// the confirmation fails, reverts it now (`uci rollback` from the applying session:
/// `rpc_uci_rollback` in rpcd's uci.c restores the configs as they were and reloads their
/// services, as its timer would). Left for rpcd's window to end, an apply answered 2 would
/// stay live another 20 to 30 s, and a configuration with the same content sent meanwhile
/// would find nothing to change, be answered 0 and then be reverted. Also returns what
/// failed on the way, for the log.
pub fn settle(reachable: bool, t: &mut impl Applied) -> (Settled, Vec<String>) {
    let mut problems = Vec::new();
    if reachable {
        match t.confirm() {
            Ok(()) => return (Settled::Kept, problems),
            Err(e) => problems.push(format!("confirming: {e}")),
        }
    }
    match t.rollback() {
        Ok(()) => (Settled::Reverted, problems),
        Err(e) => {
            problems.push(format!("reverting: {e}"));
            (Settled::Pending, problems)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_follows_what_happened() {
        let rej = || {
            vec![Rejection {
                parameter: json!({ "/radios/2/band": "6G" }),
                reason: "no 6G radio".into(),
                substitution: None,
            }]
        };
        assert_eq!(status(vec![], true, "").error, configure_error::APPLIED);
        let partly = status(rej(), true, "");
        assert_eq!(partly.error, configure_error::APPLIED_WITH_SUBSTITUTIONS);
        assert_eq!(partly.rejected.len(), 1);
        assert_eq!(
            status(vec![], false, "rolled back").error,
            configure_error::REJECTED
        );
        assert_eq!(status(rej(), false, "").error, configure_error::REJECTED);
    }

    #[test]
    fn the_first_replaced_value_is_the_one_kept() {
        let answers = |wireless: Value, poe: Value| {
            BTreeMap::from([
                ("wireless", wireless.as_object().unwrap().clone()),
                ("poe", poe.as_object().unwrap().clone()),
            ])
        };
        let plan = Plan {
            device_options: vec![
                ("wireless".into(), "radio0".into(), "channel".into()),
                ("wireless".into(), "radio0".into(), "txpower".into()),
                ("poe".into(), "cfg0a".into(), "enable".into()),
            ],
            ..Default::default()
        };
        let mut known = Map::new();
        let now = answers(
            json!({ "values": { "radio0": { "channel": "1", "htmode": "HT40" } } }),
            json!({ "values": { "cfg0a": { "enable": "1" } } }),
        );
        record_originals(&mut known, &plan, &now);
        assert_eq!(known["wireless.radio0.channel"], "1");
        assert_eq!(known["poe.cfg0a.enable"], "1");
        assert_eq!(
            known["wireless.radio0.txpower"],
            Value::Null,
            "unset before: restore by removing"
        );
        // A later configuration sees the agent's own value: the original stays.
        let later = answers(
            json!({ "values": { "radio0": { "channel": "6" } } }),
            json!({ "values": { "cfg0a": { "enable": "0" } } }),
        );
        record_originals(&mut known, &plan, &later);
        assert_eq!(known["wireless.radio0.channel"], "1");
        assert_eq!(known["poe.cfg0a.enable"], "1");
    }

    /// Stands in for rpcd: records the calls, and answers each with the given result.
    struct Fake {
        calls: Vec<&'static str>,
        confirm: Result<(), String>,
        rollback: Result<(), String>,
    }

    impl Applied for Fake {
        fn confirm(&mut self) -> Result<(), String> {
            self.calls.push("confirm");
            self.confirm.clone()
        }

        fn rollback(&mut self) -> Result<(), String> {
            self.calls.push("rollback");
            self.rollback.clone()
        }
    }

    fn fake(confirm: Result<(), String>, rollback: Result<(), String>) -> Fake {
        Fake {
            calls: vec![],
            confirm,
            rollback,
        }
    }

    /// Reachable: confirmed and kept. Otherwise, or when the confirmation fails, it's rolled
    /// back at once, before the answer, rather than left live until rpcd's window ends. Only
    /// a failed rollback leaves it pending.
    #[test]
    fn what_isnt_confirmed_is_rolled_back_at_once() {
        let mut t = fake(Ok(()), Ok(()));
        assert_eq!(settle(true, &mut t), (Settled::Kept, vec![]));
        assert_eq!(t.calls, ["confirm"]);

        let mut t = fake(Ok(()), Ok(()));
        assert_eq!(settle(false, &mut t), (Settled::Reverted, vec![]));
        assert_eq!(t.calls, ["rollback"]);

        let mut t = fake(Err("ubus: NoData".into()), Ok(()));
        let (settled, problems) = settle(true, &mut t);
        assert_eq!(settled, Settled::Reverted);
        assert_eq!(t.calls, ["confirm", "rollback"]);
        assert_eq!(problems, ["confirming: ubus: NoData"]);

        let mut t = fake(Ok(()), Err("ubus: Timeout".into()));
        let (settled, problems) = settle(false, &mut t);
        assert_eq!(settled, Settled::Pending);
        assert_eq!(problems, ["reverting: ubus: Timeout"]);
    }

    /// "Nothing to change" only when no apply is pending: one that is may be reverted after
    /// the answer, so that's busy (tried again). Not knowing is an error, never a 0.
    #[test]
    fn nothing_to_change_while_an_apply_is_pending_is_busy() {
        assert!(matches!(
            nothing(Plan::default(), || Ok(false)),
            Ok(Staged::Nothing(_))
        ));
        assert!(matches!(
            nothing(Plan::default(), || Ok(true)),
            Ok(Staged::Busy)
        ));
        let unknown = nothing(Plan::default(), || {
            Err(steward_ubus::Error::Status(steward_ubus::Status::Timeout))
        });
        assert!(matches!(unknown, Err(e) if e.contains("pending")));
    }

    /// A staging error is logged and sent as the answer's text: it names the op, never the
    /// values it carries.
    #[test]
    fn a_staging_error_names_the_op_without_its_values() {
        let op = Op::Add {
            config: "wireless".into(),
            kind: "wifi-iface".into(),
            name: "stw_0_0_5g".into(),
            values: json!({ "ssid": "Home", "key": "correct horse battery" })
                .as_object()
                .unwrap()
                .clone(),
        };
        assert_eq!(
            staging_failed(&op, "ubus: InvalidArgument"),
            "staging add wireless wifi-iface stw_0_0_5g (ssid, key): ubus: InvalidArgument"
        );
    }

    #[test]
    fn the_running_configuration_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("steward-running-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = State::new(&dir);
        assert_eq!(state.running(), Running::default());
        state
            .set_running(&Running {
                uuid: 7,
                rolled_back: Some(8),
            })
            .unwrap();
        assert_eq!(
            State::new(&dir).running(),
            Running {
                uuid: 7,
                rolled_back: Some(8)
            }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn radio_originals_from_before_move_into_originals_json() {
        let dir = std::env::temp_dir().join(format!("steward-originals-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("radio-originals.json");
        let new = dir.join("originals.json");
        let state = State::new(&dir);
        // Nothing kept yet: nothing to move.
        assert!(state.originals().unwrap().is_empty());
        assert!(!new.exists());
        // An agent from before PoE kept radios' options without their config.
        std::fs::write(
            &old,
            json!({ "radio0.channel": "1", "radio1.txpower": null }).to_string(),
        )
        .unwrap();
        // An upgraded agent that didn't know the old file recorded its own channel as the
        // original, and a PoE port's.
        std::fs::write(
            &new,
            json!({ "wireless.radio0.channel": "6", "poe.cfg0a.enable": "1" }).to_string(),
        )
        .unwrap();
        let known = state.originals().unwrap();
        assert_eq!(
            known["wireless.radio0.channel"], "1",
            "the older record wins"
        );
        assert_eq!(known["wireless.radio1.txpower"], Value::Null);
        assert_eq!(known["poe.cfg0a.enable"], "1");
        assert_eq!(known.len(), 3);
        assert!(!old.exists(), "the old file goes");
        let written: Map<String, Value> = read_json(&new);
        assert_eq!(written, known);
        // The next use reads the new file alone.
        assert_eq!(state.originals().unwrap(), known);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
