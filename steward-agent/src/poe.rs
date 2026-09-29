//! `powercycle`: PoE ports off for a while, then on again, through realtek-poe's
//! `poe manage {port, enable}`. realtek-poe manages only ports whose power is enabled in its
//! config, and answers OK for any other port without doing anything, so the plan checks that
//! first.

use serde_json::{Map, Value, json};
use std::time::Duration;
use steward_proto::Powercycle;
use steward_render::{Poe, Ports};
use steward_ubus::Ubus;

/// How long a port stays off without `cycle`, and at most.
const DEFAULT_MS: u64 = 5000;
const MAX_MS: u64 = 60_000;

/// A port to cycle: realtek-poe's name for it, and how long it stays off.
#[derive(Debug, Clone, PartialEq)]
pub struct Cycle {
    pub port: String,
    pub off: Duration,
}

/// The ports `req` names, as realtek-poe has them (`poe`: its config, `None` without it;
/// `running`: whether its `poe` object is on ubus); the reason when any can't be cycled, so
/// nothing is.
pub fn plan(
    req: &Powercycle,
    poe: Option<&Poe>,
    running: bool,
    ports: &Ports,
) -> Result<Vec<Cycle>, String> {
    let Some(poe) = poe else {
        return Err("this device has no PoE controller (realtek-poe)".into());
    };
    // `poe manage` is the daemon's: with its config there but the daemon stopped, every cycle
    // would fail after an answer that said 0.
    if !running {
        return Err("realtek-poe isn't running (no poe object on ubus)".into());
    }
    if req.ports.is_empty() {
        return Err("no ports named".into());
    }
    let mut out: Vec<Cycle> = vec![];
    for p in &req.ports {
        let ms = p.cycle.unwrap_or(DEFAULT_MS);
        if ms == 0 || ms > MAX_MS {
            return Err(format!("{}: a cycle is 1 to {MAX_MS} ms, not {ms}", p.name));
        }
        // The device's own name (lan3), or uCentral's (LAN3, LAN*).
        let names = match poe.port(&p.name) {
            Some(port) => vec![port.name.clone()],
            None => ports
                .select(&p.name)
                .ok_or_else(|| format!("{}: no such port", p.name))?,
        };
        for name in names {
            let Some(port) = poe.port(&name) else {
                if p.name.ends_with('*') {
                    continue;
                }
                return Err(format!("{name} has no PoE"));
            };
            if !port.enable {
                return Err(format!(
                    "{name}'s power is off in the configuration: there's nothing to cycle"
                ));
            }
            if !out.iter().any(|c| c.port == name) {
                out.push(Cycle {
                    port: name,
                    off: Duration::from_millis(ms),
                });
            }
        }
    }
    if out.is_empty() {
        return Err("none of the ports named has PoE".into());
    }
    Ok(out)
}

/// realtek-poe's config, whether its `poe` object is on ubus, and the board's ports, for
/// [`plan`]. Blocking.
pub fn current() -> (Option<Poe>, bool, Ports) {
    let mut ubus = Ubus::connect().ok();
    let poe = ubus
        .as_mut()
        .and_then(|u| {
            u.call(
                "uci",
                "get",
                json!({ "config": "poe" }).as_object().unwrap(),
            )
            .ok()
        })
        .map(|a| Poe::from_uci(&a));
    let running = ubus.as_mut().is_some_and(|u| u.lookup("poe").is_ok());
    let board: Value = std::fs::read_to_string("/etc/board.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    (poe, running, Ports::from_board(&board))
}

/// Turns a port's power on or off. Blocking.
pub fn power(port: &str, on: bool) -> Result<(), String> {
    let mut args = Map::new();
    args.insert("port".into(), json!(port));
    args.insert("enable".into(), json!(on));
    Ubus::connect()
        .and_then(|mut u| u.call("poe", "manage", &args))
        .map(drop)
        .map_err(|e| format!("poe manage {port}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use steward_proto::PowercyclePort;

    /// Shaped like realtek-poe's default config: lan1 to lan3 powered, lan4 off.
    fn poe() -> Poe {
        let answer = json!({ "values": {
            "cfg01": { ".type": "global", "budget": "170" },
            "cfg02": { ".type": "port", "id": "1", "name": "lan1", "enable": "1" },
            "cfg03": { ".type": "port", "id": "2", "name": "lan2", "enable": "1" },
            "cfg04": { ".type": "port", "id": "3", "name": "lan3", "enable": "1" },
            "cfg05": { ".type": "port", "id": "4", "name": "lan4", "enable": "0" },
        }});
        Poe::from_uci(answer.as_object().unwrap())
    }

    fn ports() -> Ports {
        Ports {
            lan: ["lan1", "lan2", "lan3", "lan4", "lan5"]
                .map(String::from)
                .to_vec(),
            wan: vec!["wan".into()],
        }
    }

    fn req(ports: &[(&str, Option<u64>)]) -> Powercycle {
        Powercycle {
            serial: "00005e005301".into(),
            ports: ports
                .iter()
                .map(|(n, c)| PowercyclePort {
                    name: n.to_string(),
                    cycle: *c,
                })
                .collect(),
            when: 0,
        }
    }

    #[test]
    fn ports_are_named_either_way_and_cycle_five_seconds_by_default() {
        let got = plan(
            &req(&[("lan1", None), ("LAN2", Some(10_000))]),
            Some(&poe()),
            true,
            &ports(),
        );
        assert_eq!(
            got.unwrap(),
            [
                Cycle {
                    port: "lan1".into(),
                    off: Duration::from_secs(5)
                },
                Cycle {
                    port: "lan2".into(),
                    off: Duration::from_secs(10)
                },
            ]
        );
        // A wildcard takes the powered ports it selects and skips the rest (lan5), but a
        // port whose power is off stops it: lan4.
        let err = plan(&req(&[("LAN*", None)]), Some(&poe()), true, &ports()).unwrap_err();
        assert!(err.contains("lan4"), "{err}");
    }

    #[test]
    fn realtek_poe_must_be_running_to_cycle() {
        // Its config is there, its `poe` object isn't: refused (error 2), not answered 0 and
        // left to fail in the background.
        let err = plan(&req(&[("lan1", None)]), Some(&poe()), false, &ports()).unwrap_err();
        assert!(err.contains("realtek-poe isn't running"), "{err}");
        // Without its config, the device has no PoE, whatever is on ubus.
        let err = plan(&req(&[("lan1", None)]), None, true, &ports()).unwrap_err();
        assert!(err.contains("no PoE controller"), "{err}");
    }

    #[test]
    fn what_cant_be_cycled_says_why() {
        let cases = [
            (req(&[("lan1", None)]), None, "no PoE controller"),
            (req(&[]), Some(poe()), "no ports named"),
            (req(&[("lan9", None)]), Some(poe()), "no such port"),
            (req(&[("LAN5", None)]), Some(poe()), "lan5 has no PoE"),
            (
                req(&[("lan4", None)]),
                Some(poe()),
                "off in the configuration",
            ),
            (req(&[("lan1", Some(0))]), Some(poe()), "1 to 60000 ms"),
            (
                req(&[("lan1", Some(120_000))]),
                Some(poe()),
                "1 to 60000 ms",
            ),
        ];
        for (r, p, want) in cases {
            let err = plan(&r, p.as_ref(), true, &ports()).unwrap_err();
            assert!(err.contains(want), "{want}: {err}");
        }
    }
}
