//! The controller's local control socket, `/var/run/steward-controller.sock` (root only), and the
//! command-line side of it: `steward-controller devices | adopt <serial> | forget <serial>`.
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
}

impl Answer {
    pub fn ok(message: impl Into<String>) -> Answer {
        Answer {
            ok: true,
            message: message.into(),
            devices: Value::Null,
        }
    }
    pub fn error(message: impl Into<String>) -> Answer {
        Answer {
            ok: false,
            message: message.into(),
            devices: Value::Null,
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
    if json {
        let shown = if answer.devices.is_null() {
            serde_json::to_value(&answer).unwrap_or_default()
        } else {
            answer.devices.clone()
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&shown).unwrap_or_default()
        );
        return if answer.ok { 0 } else { 1 };
    }
    if let Value::Object(devices) = &answer.devices {
        if devices.is_empty() {
            println!("No device has connected yet.");
        }
        for (serial, d) in devices {
            let connection = match (d["connected"].as_bool(), d["last_seen"].as_u64()) {
                (Some(true), _) => "connected".to_string(),
                (_, Some(t)) => format!("seen {} ago", ago(t)),
                _ => "offline".to_string(),
            };
            println!(
                "{serial}  {:<9} {:<14} {}  {}",
                d["standing"].as_str().unwrap_or("?"),
                connection,
                d["model"].as_str().unwrap_or("-"),
                d["firmware"].as_str().unwrap_or("-"),
            );
        }
    }
    if !answer.message.is_empty() {
        println!("{}", answer.message);
    }
    if answer.ok { 0 } else { 1 }
}
