//! The controller's local control socket, `/var/run/steward-controller.sock` (root only), and the
//! command-line side of it: `steward-controller devices | clients | adopt <serial> | forget <serial>`.
//! One JSON request per line, one JSON answer per line. The web interface will call the same
//! operations through its API.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Devices,
    Clients,
    Adopt { serial: String },
    Forget { serial: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Answer {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub devices: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub clients: Value,
}

impl Answer {
    pub fn ok(message: impl Into<String>) -> Answer {
        Answer {
            ok: true,
            message: message.into(),
            devices: Value::Null,
            clients: Value::Null,
        }
    }
    pub fn error(message: impl Into<String>) -> Answer {
        Answer {
            ok: false,
            message: message.into(),
            devices: Value::Null,
            clients: Value::Null,
        }
    }
}

/// Serves the socket, handing each request to `handle`. It returns only when the socket can't
/// be set up. A failed accept (out of file descriptors, say) is logged, and the next is tried a
/// second later, as on the device port: adopting and forgetting must outlive it.
pub async fn serve<F, Fut>(path: &Path, handle: F) -> std::io::Result<()>
where
    F: Fn(Request) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Answer> + Send,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("steward-controller: control socket: accept: {e}");
                tokio::time::sleep(crate::ACCEPT_RETRY).await;
                continue;
            }
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let answer = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => handle(req).await,
                    Err(e) => Answer::error(format!("bad request: {e}")),
                };
                let mut out = serde_json::to_vec(&answer).unwrap_or_default();
                out.push(b'\n');
                if write.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
    }
}

/// How long ago `t` was, briefly: `45s`, `12m`, `3h`, `2d`.
fn ago(t: u64) -> String {
    let s = crate::store::now().saturating_sub(t);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The command-line side: sends one request and prints the answer (`json`: as the
/// controller gave it). The exit status is 0 when the controller did what was asked.
pub fn client(socket: &Path, req: Request, json: bool) -> i32 {
    let stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "steward-controller: {}: {e} (is the controller running?)",
                socket.display()
            );
            return 1;
        }
    };
    let mut out = serde_json::to_vec(&req).unwrap_or_default();
    out.push(b'\n');
    let mut line = String::new();
    if let Err(e) = (&stream)
        .write_all(&out)
        .and_then(|_| BufReader::new(&stream).read_line(&mut line))
    {
        eprintln!("steward-controller: {e}");
        return 1;
    }
    let answer: Answer = match serde_json::from_str(&line) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("steward-controller: unreadable answer: {e}");
            return 1;
        }
    };
    let lines = if json {
        let shown = if !answer.clients.is_null() {
            answer.clients.clone()
        } else if !answer.devices.is_null() {
            answer.devices.clone()
        } else {
            serde_json::to_value(&answer).unwrap_or_default()
        };
        vec![serde_json::to_string_pretty(&shown).unwrap_or_default()]
    } else {
        listing(&answer)
    };
    if let Err(e) = print(&mut std::io::stdout().lock(), &lines) {
        eprintln!("steward-controller: {e}");
        return 1;
    }
    if answer.ok { 0 } else { 1 }
}

/// Prints the lines. A reader that stops reading (`steward-controller clients | head`) ends
/// the printing quietly, where println! would panic; other errors are returned.
fn print(out: &mut impl Write, lines: &[String]) -> std::io::Result<()> {
    let written = lines
        .iter()
        .try_for_each(|l| writeln!(out, "{l}"))
        .and_then(|_| out.flush());
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => r,
    }
}

/// The answer, for people: clients, devices, then the message.
fn listing(answer: &Answer) -> Vec<String> {
    let mut lines = vec![];
    if let Value::Array(clients) = &answer.clients {
        if clients.is_empty() {
            lines.push("No clients.".to_string());
        }
        for c in clients {
            let w = &c["connection"];
            let place = match w["type"].as_str() {
                Some("wireless") => format!(
                    "{} {} {} {} dBm",
                    w["device"].as_str().unwrap_or("?"),
                    w["ssid"].as_str().unwrap_or("?"),
                    w["band"].as_str().unwrap_or("?"),
                    w["signal"]
                ),
                Some("wired") => format!(
                    "{} {}",
                    w["device"].as_str().unwrap_or("?"),
                    w["port"].as_str().unwrap_or("?")
                ),
                _ => "-".into(),
            };
            lines.push(format!(
                "{}  {:<20} {:<15} {:<10} {place}",
                c["mac"].as_str().unwrap_or("?"),
                c["name"].as_str().unwrap_or("-"),
                c["ipv4"][0].as_str().unwrap_or("-"),
                c["network"].as_str().unwrap_or("-"),
            ));
        }
    }
    if let Value::Object(devices) = &answer.devices {
        if devices.is_empty() {
            lines.push("No device has connected yet.".to_string());
        }
        for (serial, d) in devices {
            let connection = match (d["connected"].as_bool(), d["last_seen"].as_u64()) {
                (Some(true), _) => "connected".to_string(),
                (_, Some(t)) => format!("seen {} ago", ago(t)),
                _ => "offline".to_string(),
            };
            lines.push(format!(
                "{serial}  {:<9} {:<14} {}  {}",
                d["standing"].as_str().unwrap_or("?"),
                connection,
                d["model"].as_str().unwrap_or("-"),
                d["firmware"].as_str().unwrap_or("-"),
            ));
        }
    }
    if !answer.message.is_empty() {
        lines.push(answer.message.clone());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::ErrorKind;

    /// A reader that takes `room` bytes, then fails with `kind`.
    struct Reader {
        room: usize,
        kind: ErrorKind,
        got: Vec<u8>,
    }

    impl Write for Reader {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.got.len() + buf.len() > self.room {
                return Err(self.kind.into());
            }
            self.got.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn printing_stops_quietly_when_the_reader_goes() {
        let answer = Answer {
            clients: json!([
                { "mac": "00:00:5e:00:53:21", "name": "phone", "ipv4": ["192.0.2.21"], "network": "lan",
                  "connection": { "type": "wireless", "device": "00005e0053a0", "ssid": "Home",
                                  "band": "5G", "signal": -40 } },
                { "mac": "00:00:5e:00:53:31", "ipv4": [], "network": "lan",
                  "connection": { "type": "wired", "device": "00005e0053a0", "port": "lan1" } }
            ]),
            ..Answer::ok("")
        };
        let lines = listing(&answer);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].ends_with("00005e0053a0 Home 5G -40 dBm"),
            "{}",
            lines[0]
        );
        assert!(lines[1].ends_with("00005e0053a0 lan1"), "{}", lines[1]);
        // `clients | head -1`: the reader takes the first line and closes the pipe.
        let mut head = Reader {
            room: lines[0].len() + 1,
            kind: ErrorKind::BrokenPipe,
            got: vec![],
        };
        assert!(print(&mut head, &lines).is_ok());
        assert_eq!(head.got, format!("{}\n", lines[0]).into_bytes());
        // Any other failure is still one.
        let mut full = Reader {
            room: 0,
            kind: ErrorKind::Other,
            got: vec![],
        };
        assert!(print(&mut full, &lines).is_err());
    }
}
