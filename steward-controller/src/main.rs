//! steward-controller: adopts and manages the devices that run steward-agent,
//! and serves the web interface (steward-web).
//!
//! Devices connect over a WebSocket on port 15002 and speak uCentral's
//! protocol (`steward_proto`). The channel is TLS (`wss://`): on first start
//! the controller creates its own certificate authority and a server
//! certificate in `<state dir>/tls/`, and agents pin that CA (`steward_tls`).
//! A device's configuration lives in `<config dir>/<serial>.json`, a uCentral
//! configuration whose `uuid` numbers it; a device reporting another uuid is
//! sent it.

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use steward_proto::{self as proto, Message, Outcome, command, event};
use steward_tls::{ControllerIdentity, TlsAcceptor, fingerprint};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

macro_rules! log {
    ($($t:tt)*) => { eprintln!("steward-controller: {}", format_args!($($t)*)) };
}

/// How long a new connection has for its TLS handshake, then for its WebSocket upgrade, and
/// then for its `connect`.
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections that haven't sent `connect` yet, at most. Further ones wait in the listen
/// backlog, so connections that never get anywhere can't take every file descriptor.
const HANDSHAKES: usize = 32;
/// The wait after a failed accept (out of file descriptors, say), rather than spinning.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
/// The largest message, and frame, a device may send: a state with 30 stations is about
/// 23 KB. A bigger one ends the connection when its frame header arrives, before it's read.
const MAX_MESSAGE: usize = 1 << 20;
/// A device that has sent nothing for this long is dropped: it went away without closing
/// (powered off, unplugged). Agents send their state every minute.
const SILENCE: Duration = Duration::from_secs(180);
/// The longest serial accepted, and the most of any other value from a device that goes into
/// the log, in bytes.
const LOG_TEXT: usize = 128;

struct Args {
    listen: String,
    state_dir: PathBuf,
    config_dir: Option<PathBuf>,
    /// Serve plain ws:// (development only).
    plaintext: bool,
}

impl Args {
    fn parse() -> Args {
        let mut a = Args {
            listen: format!("[::]:{}", proto::PORT),
            state_dir: PathBuf::from("/etc/steward"),
            config_dir: None,
            plaintext: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--version" => {
                    println!("steward-controller {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--listen" => a.listen = it.next().expect("--listen <address:port>"),
                "--state-dir" => a.state_dir = it.next().expect("--state-dir <dir>").into(),
                "--config-dir" => {
                    a.config_dir = Some(it.next().expect("--config-dir <dir>").into())
                }
                "--plaintext" => a.plaintext = true,
                _ => {
                    eprintln!(
                        "usage: steward-controller [--listen <address:port>] [--state-dir <dir>] \
                         [--config-dir <dir>] [--plaintext]"
                    );
                    std::process::exit(2);
                }
            }
        }
        a
    }
}

/// A connected device.
struct Device {
    addr: SocketAddr,
    uuid: u64,
    state: Option<Value>,
    /// Commands to send it.
    tx: mpsc::Sender<Message>,
}

#[derive(Default)]
struct Registry {
    devices: HashMap<String, Device>,
    next_id: u64,
}

type Shared = Arc<Mutex<Registry>>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    let listener = match TcpListener::bind(&args.listen).await {
        Ok(l) => l,
        Err(e) => {
            log!("cannot listen on {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    let acceptor = if args.plaintext {
        log!("serving plain ws:// (--plaintext): development only");
        None
    } else {
        match tls(&args.state_dir.join("tls")) {
            Ok(a) => Some(a),
            Err(e) => {
                log!("{e}");
                std::process::exit(1);
            }
        }
    };
    let scheme = if acceptor.is_some() { "wss" } else { "ws" };
    log!("listening for devices on {scheme}://{}", args.listen);
    let registry = Shared::default();
    let config_dir = Arc::new(
        args.config_dir
            .unwrap_or_else(|| args.state_dir.join("configs")),
    );
    let handshakes = Arc::new(Semaphore::new(HANDSHAKES));
    loop {
        // A slot for the next connection, until it sends `connect`.
        let handshake = handshakes
            .clone()
            .acquire_owned()
            .await
            .expect("never closed");
        match listener.accept().await {
            Ok((tcp, addr)) => {
                let (registry, config_dir, acceptor) =
                    (registry.clone(), config_dir.clone(), acceptor.clone());
                tokio::spawn(async move {
                    let result =
                        connection(tcp, addr, acceptor, registry, config_dir, handshake).await;
                    if let Err(e) = result {
                        // It can quote the device (serde's errors do).
                        log!("{addr}: {}", clip(&e.to_string()));
                    }
                });
            }
            Err(e) => {
                log!("accept: {e}");
                sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// The device channel's TLS: the controller's identity, created on first start.
fn tls(dir: &Path) -> Result<TlsAcceptor, Error> {
    let id = ControllerIdentity::load_or_create(dir)?;
    log!(
        "{} the controller's CA in {}: {}",
        if id.created { "created" } else { "loaded" },
        dir.display(),
        fingerprint(&id.ca)
    );
    Ok(TlsAcceptor::from(id.server_config()?))
}

/// A new connection on the device port: the TLS handshake (none with `--plaintext`), within
/// [`TLS_TIMEOUT`], and then [`serve`].
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    addr: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    registry: Shared,
    config_dir: Arc<PathBuf>,
    handshake: OwnedSemaphorePermit,
) -> Result<(), Error> {
    let Some(acceptor) = acceptor else {
        return serve(stream, addr, registry, config_dir, handshake).await;
    };
    match timeout(TLS_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => serve(tls, addr, registry, config_dir, handshake).await,
        Ok(Err(e)) => Err(format!("TLS handshake: {e}").into()),
        Err(_) => Err("no TLS handshake in time".into()),
    }
}

/// One device connection, from its `connect` to its end. Until `connect` arrives it holds
/// `handshake`, its slot among the connections that haven't sent one, and it has
/// [`UPGRADE_TIMEOUT`] for the upgrade and then [`CONNECT_TIMEOUT`] to get there. Its messages
/// are at most [`MAX_MESSAGE`] bytes, and after `connect` it's dropped once it has sent
/// nothing for [`SILENCE`].
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    addr: SocketAddr,
    registry: Shared,
    config_dir: Arc<PathBuf>,
    handshake: OwnedSemaphorePermit,
) -> Result<(), Error> {
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let upgrade = tokio_tungstenite::accept_async_with_config(stream, Some(limits));
    let mut ws = timeout(UPGRADE_TIMEOUT, upgrade)
        .await
        .map_err(|_| "no WebSocket upgrade in time")??;

    // The first message names the device.
    let first = async {
        loop {
            match next_message(&mut ws).await? {
                Some(Message::Notification { method, params, .. }) if method == event::CONNECT => {
                    return Ok::<_, Error>(Some(serde_json::from_value::<proto::Connect>(params)?));
                }
                Some(other) => log!("{addr}: before connect: {}", clip(&format!("{other:?}"))),
                None => return Ok(None),
            }
        }
    };
    let Some(connect) = timeout(CONNECT_TIMEOUT, first)
        .await
        .map_err(|_| "no connect in time")??
    else {
        return Ok(());
    };
    drop(handshake);
    let serial = connect.serial.clone();
    if serial.len() > LOG_TEXT {
        return Err(format!("a serial of {} bytes", serial.len()).into());
    }
    log!(
        "{serial} connected from {addr}: {} (running configuration {})",
        clip(&connect.firmware),
        connect.uuid
    );
    let (tx, mut rx) = mpsc::channel::<Message>(8);
    registry.lock().await.devices.insert(
        serial.clone(),
        Device {
            addr,
            uuid: connect.uuid,
            state: None,
            tx: tx.clone(),
        },
    );
    provision(&serial, &registry, &config_dir).await;

    let mut heard = Instant::now();
    let result = loop {
        tokio::select! {
            m = timeout_at(heard + SILENCE, next_message(&mut ws)) => match m {
                Ok(Ok(Some(m))) => {
                    heard = Instant::now();
                    handle(&serial, m, &registry, &config_dir).await
                }
                Ok(Ok(None)) => break Ok(()),
                Ok(Err(e)) => break Err(e),
                Err(_) => {
                    let silent = SILENCE.as_secs();
                    break Err(format!("nothing from {serial} for {silent} s").into());
                }
            },
            Some(cmd) = rx.recv() => {
                if let Err(e) = ws.send(Frame::text(serde_json::to_string(&cmd)?)).await {
                    break Err(e.into());
                }
            }
        }
    };
    let mut reg = registry.lock().await;
    if reg.devices.get(&serial).is_some_and(|d| d.addr == addr) {
        reg.devices.remove(&serial);
    }
    log!("{serial} disconnected");
    result
}

async fn next_message<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<Option<Message>, Error> {
    while let Some(frame) = ws.next().await {
        match frame? {
            Frame::Text(t) => return Ok(Some(serde_json::from_str(&t)?)),
            Frame::Binary(b) => return Ok(Some(serde_json::from_slice(&b)?)),
            Frame::Close(_) => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}

async fn handle(serial: &str, m: Message, registry: &Shared, config_dir: &Path) {
    match m {
        Message::Notification { method, params, .. } => match method.as_str() {
            event::STATE => {
                let uuid = params.get("uuid").and_then(Value::as_u64);
                let mut reg = registry.lock().await;
                if let Some(d) = reg.devices.get_mut(serial) {
                    d.state = params.get("state").cloned();
                    let stale = uuid.is_some_and(|u| u != d.uuid);
                    d.uuid = uuid.unwrap_or(d.uuid);
                    let load = d
                        .state
                        .as_ref()
                        .and_then(|s| s.pointer("/unit/load"))
                        .cloned();
                    log!(
                        "{serial}: state (configuration {}, load {})",
                        d.uuid,
                        clip(&load.unwrap_or_default().to_string())
                    );
                    drop(reg);
                    if stale {
                        provision(serial, registry, config_dir).await;
                    }
                }
            }
            event::PING | event::HEALTHCHECK => {}
            other => log!(
                "{serial}: event {}: {}",
                clip(other),
                clip(&params.to_string())
            ),
        },
        Message::Response { id, outcome, .. } => match outcome {
            Outcome::Result(r) => match serde_json::from_value::<proto::CommandResult>(r.clone()) {
                Ok(r) => log!(
                    "{serial}: command {id}: {} {}",
                    r.status.error,
                    clip(&r.status.text)
                ),
                Err(_) => log!("{serial}: command {id}: {}", clip(&r.to_string())),
            },
            Outcome::Error(e) => log!(
                "{serial}: command {id} failed: {} {}",
                e.code,
                clip(&e.message)
            ),
        },
        Message::Request { method, .. } => {
            log!("{serial}: unexpected request {}", clip(&method))
        }
    }
}

/// Text from a device, for the log: at most [`LOG_TEXT`] bytes (cut at a character
/// boundary), and marked where it was cut.
fn clip(text: &str) -> String {
    if text.len() <= LOG_TEXT {
        return text.to_owned();
    }
    let mut end = LOG_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

/// Sends the device its stored configuration, if it runs another.
async fn provision(serial: &str, registry: &Shared, config_dir: &Path) {
    let path = config_dir.join(format!("{serial}.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let config: Value = match serde_json::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            log!("{}: {e}", path.display());
            return;
        }
    };
    let Some(uuid) = config.get("uuid").and_then(Value::as_u64) else {
        log!("{}: no uuid", path.display());
        return;
    };
    let mut reg = registry.lock().await;
    reg.next_id += 1;
    let id = reg.next_id;
    let Some(d) = reg.devices.get(serial) else {
        return;
    };
    if d.uuid == uuid {
        return;
    }
    let params = proto::Configure {
        serial: serial.into(),
        uuid,
        when: 0,
        config,
    };
    match Message::request(id, command::CONFIGURE, &params) {
        Ok(m) => {
            log!("{serial}: sending configuration {uuid} (command {id})");
            let _ = d.tx.try_send(m);
        }
        Err(e) => log!("{serial}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use steward_tls::{PinnedCa, ServerName, TlsConnector};
    use tokio::io::DuplexStream;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;
    use tokio_tungstenite::WebSocketStream;

    /// A connection from a device, over TLS with `acceptor` or plain, holding one of `slots`:
    /// the device's end.
    fn open(
        registry: &Shared,
        slots: &Arc<Semaphore>,
        acceptor: Option<TlsAcceptor>,
    ) -> (DuplexStream, JoinHandle<Result<(), String>>) {
        let (device, controller) = tokio::io::duplex(1 << 16);
        let handshake = slots.clone().try_acquire_owned().unwrap();
        let (registry, config_dir) = (registry.clone(), Arc::new(PathBuf::from("/nonexistent")));
        let task = tokio::spawn(async move {
            let addr = "192.0.2.10:40000".parse().unwrap();
            connection(controller, addr, acceptor, registry, config_dir, handshake)
                .await
                .map_err(|e| e.to_string())
        });
        (device, task)
    }

    /// A controller's TLS, in a directory of its own.
    fn acceptor(name: &str) -> (PathBuf, TlsAcceptor) {
        let dir = std::env::temp_dir().join(format!("steward-conn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = ControllerIdentity::load_or_create(&dir).unwrap();
        (dir, TlsAcceptor::from(id.server_config().unwrap()))
    }

    /// The device's side of the TLS handshake (trusting the controller on first use).
    async fn tls(device: DuplexStream) -> impl AsyncRead + AsyncWrite + Unpin {
        let connector = TlsConnector::from(PinnedCa::new(None).client_config().unwrap());
        let name = ServerName::try_from("192.0.2.1").unwrap();
        connector.connect(name, device).await.unwrap()
    }

    async fn upgrade<S: AsyncRead + AsyncWrite + Unpin>(device: S) -> WebSocketStream<S> {
        tokio_tungstenite::client_async("ws://controller/", device)
            .await
            .unwrap()
            .0
    }

    async fn send<S: AsyncRead + AsyncWrite + Unpin>(
        ws: &mut WebSocketStream<S>,
        method: &str,
        params: Value,
    ) {
        let m = Message::notification(method, params).unwrap();
        ws.send(Frame::text(serde_json::to_string(&m).unwrap()))
            .await
            .unwrap();
    }

    fn hello() -> Value {
        let hello = proto::Connect {
            serial: "00005e005301".into(),
            uuid: 0,
            firmware: "OpenWrt".into(),
            wanip: vec![],
            capabilities: json!({}),
        };
        serde_json::to_value(hello).unwrap()
    }

    /// Asserts that `task` ended with `error` after `limit` (the clock is paused: exactly).
    async fn ends_after(task: JoinHandle<Result<(), String>>, limit: Duration, error: &str) {
        let start = Instant::now();
        let err = task.await.unwrap().unwrap_err();
        assert!(err.contains(error), "{err}");
        let took = start.elapsed();
        assert!(
            took >= limit && took < limit + Duration::from_secs(1),
            "{took:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_upgrades_is_dropped_and_frees_its_slot() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        let (_device, task) = open(&registry, &slots, None);
        assert_eq!(slots.available_permits(), 0);
        ends_after(task, UPGRADE_TIMEOUT, "no WebSocket upgrade in time").await;
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_sends_connect_is_dropped_and_frees_its_slot() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        // Anything but `connect` doesn't count.
        send(
            &mut ws,
            event::STATE,
            json!({ "serial": "00005e005301", "uuid": 0, "state": {} }),
        )
        .await;
        ends_after(task, CONNECT_TIMEOUT, "no connect in time").await;
        assert_eq!(slots.available_permits(), 1);
        assert!(matches!(ws.next().await, None | Some(Err(_))), "closed");
        assert!(registry.lock().await.devices.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn connect_frees_the_slot_and_the_device_stays_connected() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        send(&mut ws, event::CONNECT, hello()).await;
        sleep(Duration::from_millis(1)).await;
        assert_eq!(slots.available_permits(), 1);
        // Well past both time limits, it's still there.
        sleep(UPGRADE_TIMEOUT + CONNECT_TIMEOUT + Duration::from_secs(60)).await;
        assert!(!task.is_finished());
        assert!(registry.lock().await.devices.contains_key("00005e005301"));
        ws.close(None).await.unwrap();
        task.await.unwrap().unwrap();
        assert!(registry.lock().await.devices.is_empty());
    }

    /// A `connect` from `serial`, running `firmware`.
    fn connect_as(serial: &str, firmware: String) -> Value {
        json!({ "serial": serial, "uuid": 0, "firmware": firmware, "capabilities": {} })
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_over_the_limit_ends_the_connection_unread() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        // Just under the limit: read, and the device is in.
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        let big = "f".repeat(MAX_MESSAGE - 1000);
        send(&mut ws, event::CONNECT, connect_as("00005e005301", big)).await;
        sleep(Duration::from_millis(1)).await;
        assert!(registry.lock().await.devices.contains_key("00005e005301"));
        ws.close(None).await.unwrap();
        task.await.unwrap().unwrap();

        // Over it: the connection ends at the frame's header, while the device is still
        // sending, and nothing is registered.
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        let huge = connect_as("00005e005302", "f".repeat(2 * MAX_MESSAGE));
        let huge = Message::notification(event::CONNECT, huge).unwrap();
        let sent = ws.send(Frame::text(serde_json::to_string(&huge).unwrap()));
        assert!(sent.await.is_err(), "the whole message was taken");
        let err = task.await.unwrap().unwrap_err();
        assert!(err.contains("Message too long"), "{err}");
        assert!(registry.lock().await.devices.is_empty());
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_serial_too_long_to_log_is_refused() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        let serial = "0".repeat(LOG_TEXT + 1);
        send(
            &mut ws,
            event::CONNECT,
            connect_as(&serial, "OpenWrt".into()),
        )
        .await;
        let err = task.await.unwrap().unwrap_err();
        assert!(err.contains("a serial of 129 bytes"), "{err}");
        assert!(registry.lock().await.devices.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_goes_silent_is_dropped() {
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        send(
            &mut ws,
            event::CONNECT,
            connect_as("00005e005301", "OpenWrt".into()),
        )
        .await;
        // Each message starts the time over.
        for _ in 0..3 {
            sleep(SILENCE - Duration::from_secs(1)).await;
            assert!(!task.is_finished());
            let state = json!({ "serial": "00005e005301", "uuid": 0, "state": {} });
            send(&mut ws, event::STATE, state).await;
        }
        // Then nothing, with the connection still open (gone without closing): dropped.
        ends_after(task, SILENCE, "nothing from 00005e005301 for 180 s").await;
        assert!(registry.lock().await.devices.is_empty());
        assert!(matches!(ws.next().await, None | Some(Err(_))), "closed");
    }

    #[test]
    fn what_a_device_sends_is_cut_to_size_in_the_log() {
        assert_eq!(clip("OpenWrt"), "OpenWrt");
        let exact = "x".repeat(LOG_TEXT);
        assert_eq!(clip(&exact), exact);
        // Two bytes a character, one byte off: cut at a character boundary.
        let long = format!("v{}", "é".repeat(1000));
        assert_eq!(clip(&long), format!("v{}… (2001 bytes)", "é".repeat(63)));
    }

    #[tokio::test(start_paused = true)]
    async fn over_tls_the_handshake_has_a_time_limit_too() {
        let (dir, acceptor) = acceptor("tls");
        let (registry, slots) = (Shared::default(), Arc::new(Semaphore::new(1)));
        // Nothing at all: dropped after the TLS handshake's limit.
        let (_device, task) = open(&registry, &slots, Some(acceptor.clone()));
        ends_after(task, TLS_TIMEOUT, "no TLS handshake in time").await;
        assert_eq!(slots.available_permits(), 1);
        // A handshake, then nothing: dropped after the upgrade's.
        let (device, task) = open(&registry, &slots, Some(acceptor.clone()));
        let _device = tls(device).await;
        ends_after(task, UPGRADE_TIMEOUT, "no WebSocket upgrade in time").await;
        assert_eq!(slots.available_permits(), 1);
        // All the way to `connect`: the device is in, and its slot is free.
        let (device, task) = open(&registry, &slots, Some(acceptor));
        let mut ws = upgrade(tls(device).await).await;
        send(&mut ws, event::CONNECT, hello()).await;
        sleep(Duration::from_millis(1)).await;
        assert_eq!(slots.available_permits(), 1);
        assert!(registry.lock().await.devices.contains_key("00005e005301"));
        ws.close(None).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
