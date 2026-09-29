//! steward-controller: adopts and manages the devices that run steward-agent,
//! and serves the web interface (steward-web).
//!
//! Devices connect over a WebSocket on port 15002 and speak uCentral's
//! protocol (`steward_proto`). The channel is TLS (`wss://`): on first start
//! the controller creates its own certificate authority and a server
//! certificate in `<state dir>/tls/`, and agents pin that CA (`steward_tls`).
//!
//! A device is managed only once it's adopted (`devices`): until then it stays
//! pending, connected but sent nothing. Adopting, forgetting and configuring
//! devices are operations of the `hub`, which the control socket (`control`:
//! `steward-controller adopt <serial>`) and the HTTPS API (`api`, port 8443)
//! both call. An adopted device's configuration lives in
//! `<config dir>/<serial>.json`, a uCentral configuration whose `uuid` numbers
//! it; a device reporting another uuid is sent it. Its latest state is kept in
//! memory and written to `<state dir>/state/` (`states`) when it disconnects,
//! hourly, and when the controller stops (SIGTERM, SIGINT).

macro_rules! log {
    ($($t:tt)*) => { eprintln!("steward-controller: {}", format_args!($($t)*)) };
}
pub(crate) use log;

mod api;
mod control;
mod devices;
mod hub;
mod states;
mod store;

use control::{Answer, Request};
use devices::{Admission, Devices};
use futures_util::{SinkExt, StreamExt};
use hub::{Device, Hub, Outgoing};
use serde_json::{Value, json};
use states::States;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use steward_proto::{self as proto, Message, Outcome, event};
use steward_tls::{ControllerIdentity, TlsAcceptor, fingerprint};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as Upgrade, Response as UpgradeResponse,
};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

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
/// The most of a device's capabilities kept, in bytes (serialized): a real device reports a
/// few KB. What 64 pending devices send stays bounded by it, not by [`MAX_MESSAGE`].
const MAX_CAPABILITIES: usize = 64 << 10;
/// A device that has sent nothing for this long is dropped: it went away without closing
/// (powered off, unplugged). Agents send their state every minute.
const SILENCE: Duration = Duration::from_secs(180);
/// The longest serial accepted, and the most of any other value from a device that goes into
/// the log, in bytes.
const LOG_TEXT: usize = 128;
/// Why a pending device is disconnected when newer ones take its place.
const TOO_MANY_PENDING: &str = "too many devices waiting for adoption";

const USAGE: &str = "usage: steward-controller [--listen <address:port>] [--web-listen <address:port>] [--state-dir <dir>] [--config-dir <dir>] [--control <socket>] [--plaintext] [--adopt-local]
       steward-controller [--control <socket>] [--json] devices | adopt <serial> | forget <serial>";

struct Args {
    listen: String,
    /// The HTTPS API (and later the web interface); empty: none.
    web_listen: String,
    state_dir: PathBuf,
    config_dir: Option<PathBuf>,
    /// The control socket (in /var/run: not on flash).
    control: PathBuf,
    /// Serve plain ws:// and http:// (development only).
    plaintext: bool,
    /// Adopt devices connecting over loopback: the controller's own host (the init script).
    adopt_local: bool,
    /// A control command instead of serving: `devices`, `adopt <serial>`, `forget <serial>`.
    command: Option<Request>,
    /// Print the control command's answer as JSON.
    json: bool,
}

impl Args {
    fn parse() -> Args {
        let mut a = Args {
            listen: format!("[::]:{}", proto::PORT),
            web_listen: "[::]:8443".into(),
            state_dir: PathBuf::from("/etc/steward"),
            config_dir: None,
            control: PathBuf::from("/var/run/steward-controller.sock"),
            plaintext: false,
            adopt_local: false,
            command: None,
            json: false,
        };
        let usage = || -> ! {
            eprintln!("{USAGE}");
            std::process::exit(2);
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--version" => {
                    println!("steward-controller {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--listen" => a.listen = it.next().unwrap_or_else(|| usage()),
                "--web-listen" => a.web_listen = it.next().unwrap_or_else(|| usage()),
                "--state-dir" => a.state_dir = it.next().unwrap_or_else(|| usage()).into(),
                "--config-dir" => a.config_dir = Some(it.next().unwrap_or_else(|| usage()).into()),
                "--control" => a.control = it.next().unwrap_or_else(|| usage()).into(),
                "--plaintext" => a.plaintext = true,
                "--adopt-local" => a.adopt_local = true,
                "--json" => a.json = true,
                "devices" => a.command = Some(Request::Devices),
                "adopt" => {
                    a.command = Some(Request::Adopt {
                        serial: it.next().unwrap_or_else(|| usage()),
                    })
                }
                "forget" => {
                    a.command = Some(Request::Forget {
                        serial: it.next().unwrap_or_else(|| usage()),
                    })
                }
                _ => usage(),
            }
        }
        a
    }
}

fn fail(e: impl std::fmt::Display) -> ! {
    log!("{e}");
    std::process::exit(1);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    if let Some(req) = args.command {
        std::process::exit(control::client(&args.control, req, args.json));
    }
    let listener = TcpListener::bind(&args.listen)
        .await
        .unwrap_or_else(|e| fail(format!("cannot listen on {}: {e}", args.listen)));
    let (acceptor, web_acceptor) = if args.plaintext {
        log!("serving plain ws:// and http:// (--plaintext): development only");
        (None, None)
    } else {
        let (devices, web) = tls(&args.state_dir.join("tls")).unwrap_or_else(|e| fail(e));
        (Some(devices), Some(web))
    };
    let devices = Devices::load(&args.state_dir.join("devices.json")).unwrap_or_else(|e| fail(e));
    let config_dir = args
        .config_dir
        .clone()
        .unwrap_or_else(|| args.state_dir.join("configs"));
    let (states, skipped) = States::load(&args.state_dir.join("state"));
    for e in skipped {
        log!("skipped a stored state: {e}");
    }
    let hub = Arc::new(Hub::new(devices, states, config_dir));

    // States are written hourly when they changed, and when the controller stops.
    let h = hub.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(std::time::Duration::from_secs(3600));
        hourly.tick().await;
        loop {
            hourly.tick().await;
            h.flush_states().await;
        }
    });
    let h = hub.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            log!("can't catch SIGTERM and SIGINT: states are written hourly only");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        h.stop().await;
        log!("stopping");
        std::process::exit(0);
    });

    let (socket, h) = (args.control.clone(), hub.clone());
    tokio::spawn(async move {
        let handler = move |req| {
            let h = h.clone();
            async move { control_request(req, &h).await }
        };
        if let Err(e) = control::serve(&socket, handler).await {
            log!("control socket {}: {e}", socket.display());
        }
    });

    // --adopt-local adopts this host's own agent, which it knows by the host's serial.
    let adopt_local: Option<Arc<str>> = if args.adopt_local {
        let own = proto::host_serial();
        if own.is_none() {
            log!(
                "--adopt-local: this host has no serial (no label MAC or Ethernet address) to know its agent by; adopting nothing by itself"
            );
        }
        own.map(Arc::from)
    } else {
        None
    };
    if !args.web_listen.is_empty() {
        let web = TcpListener::bind(&args.web_listen)
            .await
            .unwrap_or_else(|e| fail(format!("cannot listen on {}: {e}", args.web_listen)));
        let scheme = if web_acceptor.is_some() {
            "https"
        } else {
            "http"
        };
        log!("serving the API on {scheme}://{}", args.web_listen);
        let app = api::router(hub.clone(), Arc::new(api::Rpcd));
        tokio::spawn(api::serve(web, web_acceptor, app));
    }

    let scheme = if acceptor.is_some() { "wss" } else { "ws" };
    log!("listening for devices on {scheme}://{}", args.listen);
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
                let (hub, acceptor) = (hub.clone(), acceptor.clone());
                let adopt_local = adopt_local.clone();
                tokio::spawn(async move {
                    let result = connection(tcp, addr, acceptor, hub, adopt_local, handshake).await;
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

/// The controller's TLS, from its identity (created on first start): the device channel's,
/// and the API's (the same certificate, speaking HTTP/1.1).
fn tls(dir: &Path) -> Result<(TlsAcceptor, TlsAcceptor), Error> {
    let id = ControllerIdentity::load_or_create(dir)?;
    log!(
        "{} the controller's CA in {}: {}",
        if id.created { "created" } else { "loaded" },
        dir.display(),
        fingerprint(&id.ca)
    );
    let mut web = (*id.server_config()?).clone();
    web.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((
        TlsAcceptor::from(id.server_config()?),
        TlsAcceptor::from(Arc::new(web)),
    ))
}

/// A new connection on the device port: the TLS handshake (none with `--plaintext`), within
/// [`TLS_TIMEOUT`], and then [`serve`].
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    addr: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    hub: Arc<Hub>,
    adopt_local: Option<Arc<str>>,
    handshake: OwnedSemaphorePermit,
) -> Result<(), Error> {
    let Some(acceptor) = acceptor else {
        return serve(stream, addr, hub, adopt_local, handshake).await;
    };
    match timeout(TLS_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => serve(tls, addr, hub, adopt_local, handshake).await,
        Ok(Err(e)) => Err(format!("TLS handshake: {e}").into()),
        Err(_) => Err("no TLS handshake in time".into()),
    }
}

/// One device connection, from its `connect` to its end. Until `connect` arrives it holds
/// `handshake`, its slot among the connections that haven't sent one, and it has
/// [`UPGRADE_TIMEOUT`] for the upgrade and then [`CONNECT_TIMEOUT`] to get there. Its messages
/// are at most [`MAX_MESSAGE`] bytes, and after `connect` it's dropped once it has sent
/// nothing for [`SILENCE`]. `adopt_local` is this host's serial with `--adopt-local`.
// tungstenite's upgrade callback returns its own (large) error response type.
#[allow(clippy::result_large_err)]
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    addr: SocketAddr,
    hub: Arc<Hub>,
    adopt_local: Option<Arc<str>>,
    handshake: OwnedSemaphorePermit,
) -> Result<(), Error> {
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    // The credential rides on the upgrade request: `Authorization: Bearer <credential>`.
    let credential = Arc::new(std::sync::Mutex::new(None::<String>));
    let seen = credential.clone();
    let upgrade = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        move |req: &Upgrade, resp: UpgradeResponse| {
            let bearer = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned);
            *seen.lock().unwrap() = bearer;
            Ok(resp)
        },
        Some(limits),
    );
    let mut ws = timeout(UPGRADE_TIMEOUT, upgrade)
        .await
        .map_err(|_| "no WebSocket upgrade in time")??;
    let credential = credential.lock().unwrap().take();

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
    if !store::valid_serial(&serial) {
        log!("{addr}: refused: {serial:?} isn't a serial (12 lower-case hex digits)");
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "not a serial".into(),
            }))
            .await;
        return Ok(());
    }
    let model = connect.capabilities.get("model").and_then(Value::as_str);
    // Kept while it's connected (and on flash once it's adopted), so bounded: more than a real
    // device reports isn't kept at all.
    let capabilities = if connect.capabilities.to_string().len() <= MAX_CAPABILITIES {
        connect.capabilities.clone()
    } else {
        log!("{serial}: capabilities over {MAX_CAPABILITIES} bytes aren't kept");
        Value::Null
    };
    let (tx, mut rx) = mpsc::channel::<Outgoing>(8);
    // Admitted and registered as connected under one lock, and a pending device dropped to
    // make room past MAX_PENDING (the oldest that isn't connected, or when all are, the
    // oldest) disconnected: records and connections go together, so connections that aren't
    // adopted stay bounded too.
    let admission = {
        let mut guard = hub.registry.lock().await;
        let reg = &mut *guard;
        let connected = &reg.connected;
        let mut admission = reg.devices.admit(
            &serial,
            credential.as_deref(),
            model,
            &connect.firmware,
            |s| connected.contains_key(s),
        )?;
        // The controller's own host is adopted without asking, and again if it lost its
        // credential: its agent connects over loopback (only processes on the host can) and
        // reports the host's serial. Any serial claimed over loopback isn't enough, and a device
        // adopted from the network is never taken over this way.
        if adopt_local.as_deref() == Some(serial.as_str())
            && is_loopback(addr.ip())
            && matches!(admission, Admission::Pending | Admission::Refused)
        {
            if reg.devices.readopt(&serial)? {
                if let Some(credential) = reg.devices.issue(&serial)? {
                    log!("{serial} is this controller's own host: adopting it");
                    admission = Admission::Deliver(credential);
                }
            } else {
                log!(
                    "{serial} from {addr}: adopted from the network, so not adopted again by itself"
                );
            }
        }
        for dropped in reg.devices.take_dropped() {
            if let Some(d) = reg.connected.get(&dropped) {
                let _ = d.tx.try_send(Outgoing::Close(TOO_MANY_PENDING));
            }
        }
        if admission != Admission::Refused {
            // Over the limit, an adopted device keeps what it reported before (its own MACs,
            // which the client list leaves out, among it).
            let capabilities = if capabilities.is_null() {
                let stored = reg.devices.all().get(&serial);
                stored
                    .and_then(|r| r.capabilities.clone())
                    .unwrap_or(Value::Null)
            } else {
                capabilities.clone()
            };
            let replaced = reg.connected.insert(
                serial.clone(),
                Device {
                    addr,
                    uuid: connect.uuid,
                    capabilities: capabilities.clone(),
                    seen: store::now(),
                    tx: tx.clone(),
                    adopt_id: None,
                    configure: None,
                },
            );
            if let Some(old) = replaced {
                let _ = old
                    .tx
                    .try_send(Outgoing::Close("the device connected again"));
            }
            let address = addr.ip().to_canonical().to_string();
            if let Err(e) = reg.devices.connected(&serial, &address, &capabilities) {
                log!("{serial}: {e}");
            }
        }
        admission
    };
    if admission == Admission::Refused {
        log!("{serial} from {addr}: refused: it's adopted, and it didn't present its credential");
        let _ = ws
            .close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "not this device's credential".into(),
            }))
            .await;
        return Ok(());
    }
    hub.emit(
        "connected",
        &serial,
        json!({ "address": addr.to_string(), "firmware": clip(&connect.firmware), "running": connect.uuid }),
    );
    match admission {
        Admission::Pending => log!(
            "{serial} connected from {addr}: {}; pending adoption (steward-controller adopt {serial})",
            clip(&connect.firmware)
        ),
        Admission::Deliver(credential) => {
            log!(
                "{serial} connected from {addr}: {}; adopted, sending its credential",
                clip(&connect.firmware)
            );
            hub.send_credential(&serial, credential).await;
        }
        Admission::Admitted => {
            log!(
                "{serial} connected from {addr}: {} (running configuration {})",
                clip(&connect.firmware),
                connect.uuid
            );
            hub.provision(&serial).await;
        }
        Admission::Refused => unreachable!(),
    }

    let mut heard = Instant::now();
    let result = loop {
        tokio::select! {
            m = timeout_at(heard + SILENCE, next_message(&mut ws)) => match m {
                Ok(Ok(Some(m))) => {
                    heard = Instant::now();
                    handle(&serial, m, &hub).await
                }
                Ok(Ok(None)) => break Ok(()),
                Ok(Err(e)) => break Err(e),
                Err(_) => {
                    let silent = SILENCE.as_secs();
                    break Err(format!("nothing from {serial} for {silent} s").into());
                }
            },
            Some(out) = rx.recv() => match out {
                Outgoing::Send(cmd) => {
                    if let Err(e) = ws.send(Frame::text(serde_json::to_string(&cmd)?)).await {
                        break Err(e.into());
                    }
                }
                Outgoing::Close(why) => {
                    log!("{serial}: closing: {why}");
                    let _ = ws.close(Some(CloseFrame { code: CloseCode::Policy, reason: why.into() })).await;
                    break Ok(());
                }
            }
        }
    };
    let mut reg = hub.registry.lock().await;
    if reg.connected.get(&serial).is_some_and(|d| d.addr == addr) {
        reg.connected.remove(&serial);
        // Its last state and when it was last seen are kept for when it's offline.
        if let Err(e) = reg.devices.seen(&[&serial]) {
            log!("{serial}: {e}");
        }
        if let Err(e) = reg.states.flush(&serial) {
            log!("{serial}: state: {e}");
        }
        drop(reg);
        hub.emit("disconnected", &serial, Value::Null);
    }
    log!("{serial} disconnected");
    result
}

/// Loopback, including IPv4 loopback seen through a dual-stack `[::]` socket (::ffff:127.0.0.1).
fn is_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(v6.is_loopback(), |v4| v4.is_loopback()),
    }
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

async fn handle(serial: &str, m: Message, hub: &Hub) {
    if let Some(d) = hub.registry.lock().await.connected.get_mut(serial) {
        d.seen = store::now();
    }
    match m {
        Message::Notification { method, params, .. } => match method.as_str() {
            event::STATE => {
                let uuid = params.get("uuid").and_then(Value::as_u64);
                let state = params.get("state").cloned().unwrap_or(Value::Null);
                let mut reg = hub.registry.lock().await;
                let adopted = reg.devices.is_adopted(serial);
                let Some(d) = reg.connected.get_mut(serial) else {
                    return;
                };
                let stale = uuid.is_some_and(|u| u != d.uuid);
                d.uuid = uuid.unwrap_or(d.uuid);
                let running = d.uuid;
                log!(
                    "{serial}: state (configuration {running}, load {})",
                    clip(
                        &state
                            .pointer("/unit/load")
                            .cloned()
                            .unwrap_or_default()
                            .to_string()
                    )
                );
                // Anything that connects is pending: what a pending device sends is neither
                // kept nor passed on, and its page shows it's pending. An adopted device's
                // state is kept in memory, and written later.
                if !adopted {
                    return;
                }
                reg.states.record(serial, running, state.clone(), adopted);
                drop(reg);
                hub.emit("state", serial, state);
                if stale {
                    hub.provision(serial).await;
                }
            }
            event::PING | event::HEALTHCHECK => {}
            other => log!(
                "{serial}: event {}: {}",
                clip(other),
                clip(&params.to_string())
            ),
        },
        Message::Response { id, outcome, .. } => {
            let result = match &outcome {
                Outcome::Result(r) => {
                    serde_json::from_value::<proto::CommandResult>(r.clone()).ok()
                }
                Outcome::Error(_) => None,
            };
            let mut reg = hub.registry.lock().await;
            let is_adoption = reg.connected.get(serial).and_then(|d| d.adopt_id) == Some(id);
            if is_adoption {
                let mut over_loopback = false;
                let mut seen = None;
                if let Some(d) = reg.connected.get_mut(serial) {
                    d.adopt_id = None;
                    over_loopback = is_loopback(d.addr.ip());
                    seen = Some((
                        d.addr.ip().to_canonical().to_string(),
                        d.capabilities.clone(),
                    ));
                }
                if result.as_ref().is_some_and(|r| r.status.error == 0) {
                    match reg.devices.delivered(serial, over_loopback) {
                        Ok(()) => log!("{serial}: adopted"),
                        Err(e) => log!("{serial}: {e}"),
                    }
                    // Now adopted, its connection is recorded as an adopted device's.
                    if let Some((address, capabilities)) = seen
                        && let Err(e) = reg.devices.connected(serial, &address, &capabilities)
                    {
                        log!("{serial}: {e}");
                    }
                    drop(reg);
                    hub.emit("adopted", serial, Value::Null);
                    hub.provision(serial).await;
                } else {
                    log!(
                        "{serial}: didn't take its credential: {}",
                        clip(&format!("{outcome:?}"))
                    );
                }
                return;
            }
            // The answer to a configuration: kept with the device.
            let configured = reg.connected.get_mut(serial).and_then(|d| {
                d.configure
                    .filter(|(cid, _)| *cid == id)
                    .inspect(|_| d.configure = None)
            });
            if let (Some((_, uuid)), Some(r)) = (configured, &result)
                && let Err(e) = reg.devices.answered(serial, uuid, r.status.clone())
            {
                log!("{serial}: {e}");
            }
            drop(reg);
            match (&result, &outcome) {
                (Some(r), _) => log!(
                    "{serial}: command {id}: {} {}",
                    r.status.error,
                    clip(&r.status.text)
                ),
                (None, Outcome::Error(e)) => log!(
                    "{serial}: command {id} failed: {} {}",
                    e.code,
                    clip(&e.message)
                ),
                (None, Outcome::Result(r)) => {
                    log!("{serial}: command {id}: {}", clip(&r.to_string()))
                }
            }
            hub.emit(
                "answer",
                serial,
                json!({ "id": id, "result": result.map(|r| json!(r.status)) }),
            );
        }
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

/// A request on the control socket: the hub's operations.
async fn control_request(req: Request, hub: &Hub) -> Answer {
    let answer = |r: Result<String, hub::OpError>| match r {
        Ok(m) => Answer::ok(m),
        Err(e) => Answer::error(e.to_string()),
    };
    match req {
        Request::Devices => Answer {
            ok: true,
            message: String::new(),
            devices: hub.devices().await,
        },
        Request::Adopt { serial } => answer(hub.adopt(&serial).await),
        Request::Forget { serial } => answer(hub.forget(&serial).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use steward_proto::command;
    use steward_tls::{PinnedCa, ServerName, TlsConnector};
    use tokio::io::DuplexStream;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;
    use tokio_tungstenite::WebSocketStream;

    /// A hub on a state directory of its own, its configurations in `configs/`.
    fn registry(name: &str) -> (PathBuf, Arc<Hub>) {
        let dir = std::env::temp_dir().join(format!("steward-conn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let devices = Devices::load(&dir.join("devices.json")).unwrap();
        let (states, _) = crate::states::States::load(&dir.join("state"));
        let hub = Hub::new(devices, states, dir.join("configs"));
        (dir, Arc::new(hub))
    }

    /// A connection from a device, over TLS with `acceptor` or plain, holding one of `slots`:
    /// the device's end.
    fn open(
        registry: &Arc<Hub>,
        slots: &Arc<Semaphore>,
        acceptor: Option<TlsAcceptor>,
    ) -> (DuplexStream, JoinHandle<Result<(), String>>) {
        let (device, controller) = tokio::io::duplex(1 << 16);
        let handshake = slots.clone().try_acquire_owned().unwrap();
        let registry = registry.clone();
        let task = tokio::spawn(async move {
            let addr = "192.0.2.10:40000".parse().unwrap();
            connection(controller, addr, acceptor, registry, None, handshake)
                .await
                .map_err(|e| e.to_string())
        });
        (device, task)
    }

    /// A controller's TLS, created in `dir`.
    fn acceptor(dir: &Path) -> TlsAcceptor {
        let id = ControllerIdentity::load_or_create(dir).unwrap();
        TlsAcceptor::from(id.server_config().unwrap())
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
        let ((dir, registry), slots) = (registry("upgrade"), Arc::new(Semaphore::new(1)));
        let (_device, task) = open(&registry, &slots, None);
        assert_eq!(slots.available_permits(), 0);
        ends_after(task, UPGRADE_TIMEOUT, "no WebSocket upgrade in time").await;
        assert_eq!(slots.available_permits(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_sends_connect_is_dropped_and_frees_its_slot() {
        let ((dir, registry), slots) = (registry("connect"), Arc::new(Semaphore::new(1)));
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
        assert!(registry.registry.lock().await.connected.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_frees_the_slot_and_the_device_stays_connected() {
        let ((dir, registry), slots) = (registry("stays"), Arc::new(Semaphore::new(1)));
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        send(&mut ws, event::CONNECT, hello()).await;
        sleep(Duration::from_millis(1)).await;
        assert_eq!(slots.available_permits(), 1);
        // Well past both time limits, it's still there.
        sleep(UPGRADE_TIMEOUT + CONNECT_TIMEOUT + Duration::from_secs(60)).await;
        assert!(!task.is_finished());
        assert!(
            registry
                .registry
                .lock()
                .await
                .connected
                .contains_key("00005e005301")
        );
        ws.close(None).await.unwrap();
        task.await.unwrap().unwrap();
        assert!(registry.registry.lock().await.connected.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `connect` from `serial`, running `firmware`.
    fn connect_as(serial: &str, firmware: String) -> Value {
        json!({ "serial": serial, "uuid": 0, "firmware": firmware, "capabilities": {} })
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_over_the_limit_ends_the_connection_unread() {
        let ((dir, registry), slots) = (registry("big"), Arc::new(Semaphore::new(1)));
        // Just under the limit: read, and the device is in.
        let (device, task) = open(&registry, &slots, None);
        let mut ws = upgrade(device).await;
        let big = "f".repeat(MAX_MESSAGE - 1000);
        send(&mut ws, event::CONNECT, connect_as("00005e005301", big)).await;
        sleep(Duration::from_millis(1)).await;
        assert!(
            registry
                .registry
                .lock()
                .await
                .connected
                .contains_key("00005e005301")
        );
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
        assert!(registry.registry.lock().await.connected.is_empty());
        assert_eq!(slots.available_permits(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn capabilities_past_the_limit_are_not_kept() {
        let ((dir, registry), slots) = (registry("caps"), Arc::new(Semaphore::new(1)));
        for (serial, size, kept) in [
            ("00005e005301", 100, true),
            ("00005e005302", MAX_CAPABILITIES + 1, false),
        ] {
            let (device, task) = open(&registry, &slots, None);
            let mut ws = upgrade(device).await;
            let capabilities = json!({ "model": "E8450", "x": "f".repeat(size) });
            let connect = json!({ "serial": serial, "uuid": 0, "firmware": "OpenWrt",
                                  "capabilities": capabilities });
            send(&mut ws, event::CONNECT, connect).await;
            sleep(Duration::from_millis(1)).await;
            let reg = registry.registry.lock().await;
            assert_eq!(
                reg.connected[serial].capabilities.is_null(),
                !kept,
                "{serial}"
            );
            drop(reg);
            ws.close(None).await.unwrap();
            task.await.unwrap().unwrap();
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_serial_too_long_to_log_is_refused() {
        let ((dir, registry), slots) = (registry("long"), Arc::new(Semaphore::new(1)));
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
        assert!(registry.registry.lock().await.connected.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_that_goes_silent_is_dropped() {
        let ((dir, registry), slots) = (registry("silent"), Arc::new(Semaphore::new(1)));
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
        assert!(registry.registry.lock().await.connected.is_empty());
        assert!(matches!(ws.next().await, None | Some(Err(_))), "closed");
        let _ = std::fs::remove_dir_all(dir);
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
        let ((dir, registry), slots) = (registry("tls"), Arc::new(Semaphore::new(1)));
        let acceptor = acceptor(&dir.join("tls"));
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
        assert!(
            registry
                .registry
                .lock()
                .await
                .connected
                .contains_key("00005e005301")
        );
        ws.close(None).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A device connected as `serial`, with no credential: pending.
    async fn pending(
        registry: &Arc<Hub>,
        slots: &Arc<Semaphore>,
        serial: &str,
    ) -> (
        WebSocketStream<DuplexStream>,
        JoinHandle<Result<(), String>>,
    ) {
        let (device, task) = open(registry, slots, None);
        let mut ws = upgrade(device).await;
        let mut hello = hello();
        hello["serial"] = json!(serial);
        send(&mut ws, event::CONNECT, hello).await;
        sleep(Duration::from_millis(1)).await;
        (ws, task)
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_device_is_disconnected_with_its_record() {
        use devices::{MAX_PENDING, Standing};
        let ((dir, registry), slots) = (registry("bound"), Arc::new(Semaphore::new(1)));
        let serial = |i: usize| format!("00005e{i:06x}");
        // As many pending devices as are kept, all connected.
        let mut held = std::collections::VecDeque::new();
        for i in 0..MAX_PENDING {
            held.push_back(pending(&registry, &slots, &serial(i)).await);
        }
        // One more: the oldest's record is dropped, and its connection is closed with it.
        let newest = pending(&registry, &slots, &serial(MAX_PENDING)).await;
        let (mut oldest, task) = held.pop_front().unwrap();
        match timeout(Duration::from_secs(1), oldest.next()).await {
            Ok(Some(Ok(Frame::Close(Some(f))))) => {
                assert_eq!(
                    (f.code, f.reason.as_str()),
                    (CloseCode::Policy, TOO_MANY_PENDING)
                )
            }
            other => panic!("not closed: {other:?}"),
        }
        task.await.unwrap().unwrap();
        held.push_back(newest);
        {
            let reg = registry.registry.lock().await;
            assert_eq!(reg.connected.len(), MAX_PENDING);
            assert!(!reg.devices.all().contains_key(&serial(0)));
            assert!(
                reg.connected
                    .keys()
                    .all(|s| reg.devices.all().contains_key(s))
            );
        }
        // A connected device is listed, so it can be adopted: its credential goes out.
        let adopt = Request::Adopt { serial: serial(1) };
        let answer = control_request(adopt, &registry).await;
        assert!(answer.ok, "{}", answer.message);
        match timeout(Duration::from_secs(1), held[0].0.next()).await {
            Ok(Some(Ok(Frame::Text(t)))) => assert!(t.contains(command::ADOPT), "{t}"),
            other => panic!("no credential: {other:?}"),
        }
        // The dropped device is pending again when it reconnects. (The adopted one made room.)
        held.push_back(pending(&registry, &slots, &serial(0)).await);
        {
            let reg = registry.registry.lock().await;
            assert_eq!(reg.devices.all()[&serial(0)].standing, Standing::Pending);
            assert!(reg.connected.contains_key(&serial(0)));
            assert_eq!(reg.connected.len(), MAX_PENDING + 1);
        }
        for (mut ws, task) in held {
            ws.close(None).await.unwrap();
            task.await.unwrap().unwrap();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_connected_pending_device_outlasts_devices_that_hang_up() {
        let ((dir, registry), slots) = (registry("outlast"), Arc::new(Semaphore::new(1)));
        let (mut there, task) = pending(&registry, &slots, "00005e005301").await;
        // 100 made-up serials, each connecting and hanging up: they make room for each other.
        for i in 0..100 {
            let (mut ws, task) = pending(&registry, &slots, &format!("00005e1{i:05x}")).await;
            ws.close(None).await.unwrap();
            task.await.unwrap().unwrap();
        }
        {
            let reg = registry.registry.lock().await;
            assert!(reg.connected.contains_key("00005e005301"));
            let pending = reg.devices.all().len();
            assert_eq!(pending, devices::MAX_PENDING);
        }
        assert!(
            timeout(Duration::from_millis(10), there.next())
                .await
                .is_err(),
            "closed"
        );
        // Still listed, so it can be adopted.
        let adopt = Request::Adopt {
            serial: "00005e005301".into(),
        };
        let answer = control_request(adopt, &registry).await;
        assert!(answer.ok, "{}", answer.message);
        there.close(None).await.unwrap();
        task.await.unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loopback_includes_ipv4_seen_on_a_dual_stack_socket() {
        for (ip, local) in [
            ("127.0.0.1", true),
            ("::1", true),
            ("::ffff:127.0.0.1", true),
            ("192.0.2.10", false),
            ("::ffff:192.0.2.10", false),
            ("2001:db8::1", false),
        ] {
            assert_eq!(is_loopback(ip.parse().unwrap()), local, "{ip}");
        }
    }

    const HOST: &str = "00005e005301";
    const AP: &str = "00005e005302";
    const REMOTE: &str = "192.0.2.10:40000";
    const LOOPBACK: &str = "[::ffff:127.0.0.1]:40000";

    type Device = (
        WebSocketStream<DuplexStream>,
        JoinHandle<Result<(), String>>,
    );

    /// A device connecting from `addr` as `serial`, presenting `credential`, to a controller
    /// whose own host is `own` (`--adopt-local`): its WebSocket, once `connect` is sent.
    async fn device(
        registry: &Arc<Hub>,
        config_dir: &Path,
        addr: &str,
        own: Option<&str>,
        credential: Option<&str>,
        serial: &str,
    ) -> Device {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (device, controller) = tokio::io::duplex(1 << 16);
        let handshake = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        assert_eq!(config_dir, registry.config_dir, "the hub's configurations");
        let registry = registry.clone();
        let (addr, own) = (addr.parse().unwrap(), own.map(Arc::from));
        let task = tokio::spawn(async move {
            serve(controller, addr, registry, own, handshake)
                .await
                .map_err(|e| e.to_string())
        });
        let mut request = "ws://controller/".into_client_request().unwrap();
        if let Some(c) = credential {
            let bearer = format!("Bearer {c}").parse().unwrap();
            request.headers_mut().insert("authorization", bearer);
        }
        let (mut ws, _) = tokio_tungstenite::client_async(request, device)
            .await
            .unwrap();
        let hello = proto::Connect {
            serial: serial.into(),
            uuid: 0,
            firmware: "OpenWrt".into(),
            wanip: vec![],
            capabilities: json!({}),
        };
        send(
            &mut ws,
            event::CONNECT,
            serde_json::to_value(hello).unwrap(),
        )
        .await;
        (ws, task)
    }

    #[derive(Debug, PartialEq)]
    enum Got {
        Command(u64, String, Value),
        /// With the close frame's code, if there was one.
        Closed(Option<u16>),
        Nothing,
    }

    /// What the controller sends next, within a second.
    async fn next(ws: &mut WebSocketStream<DuplexStream>) -> Got {
        match timeout(Duration::from_secs(1), ws.next()).await {
            Err(_) => Got::Nothing,
            Ok(Some(Ok(Frame::Text(t)))) => match serde_json::from_str(&t).unwrap() {
                Message::Request {
                    id, method, params, ..
                } => Got::Command(id, method, params),
                other => panic!("{other:?}"),
            },
            Ok(Some(Ok(Frame::Close(f)))) => Got::Closed(f.map(|f| u16::from(f.code))),
            Ok(None | Some(Err(_))) => Got::Closed(None),
            Ok(Some(Ok(other))) => panic!("{other:?}"),
        }
    }

    async fn answer(ws: &mut WebSocketStream<DuplexStream>, id: u64, serial: &str) {
        let r = proto::CommandResult {
            serial: serial.into(),
            uuid: None,
            status: proto::CommandStatus {
                error: 0,
                text: String::new(),
                when: None,
                rejected: vec![],
            },
        };
        let m = Message::result(id, r).unwrap();
        ws.send(Frame::text(serde_json::to_string(&m).unwrap()))
            .await
            .unwrap();
    }

    /// Takes the credential `steward.adopt` brings, and confirms it.
    async fn take_credential(ws: &mut WebSocketStream<DuplexStream>, serial: &str) -> String {
        let Got::Command(id, method, params) = next(ws).await else {
            panic!("no credential for {serial}")
        };
        assert_eq!(method, command::ADOPT);
        answer(ws, id, serial).await;
        params["credential"].as_str().unwrap().to_owned()
    }

    async fn hang_up((mut ws, task): Device) {
        let _ = ws.close(None).await;
        let _ = task.await;
    }

    #[tokio::test(start_paused = true)]
    async fn adopt_local_adopts_only_this_hosts_own_serial_over_loopback() {
        let (dir, registry) = registry("own");
        let cfg = dir.join("configs");
        // Another serial over loopback, the host's from the network, or the host's without
        // --adopt-local: pending, like anything else.
        for (addr, own, serial) in [
            (LOOPBACK, Some(HOST), AP),
            (REMOTE, Some(HOST), HOST),
            (LOOPBACK, None, HOST),
        ] {
            let mut d = device(&registry, &cfg, addr, own, None, serial).await;
            assert_eq!(
                next(&mut d.0).await,
                Got::Nothing,
                "{addr} {own:?} {serial}"
            );
            hang_up(d).await;
        }
        // The host's own agent over loopback, with it: adopted by itself.
        let mut d = device(&registry, &cfg, LOOPBACK, Some(HOST), None, HOST).await;
        let first = take_credential(&mut d.0, HOST).await;
        assert_eq!(next(&mut d.0).await, Got::Nothing);
        hang_up(d).await;
        {
            let reg = registry.registry.lock().await;
            assert!(reg.devices.is_adopted(HOST));
            assert!(reg.devices.all()[HOST].adopted_over_loopback);
            assert_eq!(reg.devices.all()[AP].standing, devices::Standing::Pending);
        }
        // Having lost its credential, it's adopted again, and the old one stops working.
        let mut d = device(&registry, &cfg, "127.0.0.1:40001", Some(HOST), None, HOST).await;
        let second = take_credential(&mut d.0, HOST).await;
        hang_up(d).await;
        let mut d = device(&registry, &cfg, REMOTE, Some(HOST), Some(&first), HOST).await;
        assert_eq!(next(&mut d.0).await, Got::Closed(Some(1008)));
        hang_up(d).await;
        let mut d = device(
            &registry,
            &cfg,
            "[::1]:40002",
            Some(HOST),
            Some(&second),
            HOST,
        )
        .await;
        assert_eq!(next(&mut d.0).await, Got::Nothing);
        hang_up(d).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Anything that connects is pending, so what a pending device sends isn't kept (in
    /// memory or on flash) or passed on, however many connect and however big their states:
    /// 64 records, no states. An adopted device's state is kept, listed and streamed, and
    /// goes when it's forgotten, with one that arrives before its connection closes.
    #[tokio::test(start_paused = true)]
    async fn only_an_adopted_devices_state_is_kept() {
        let (dir, registry) = registry("states");
        let cfg = dir.join("configs");
        let mut events = registry.subscribe();
        // The serials of the states streamed since last asked.
        let drain = |events: &mut tokio::sync::broadcast::Receiver<Value>| {
            let mut serials = vec![];
            while let Ok(e) = events.try_recv() {
                if e["event"] == "state" {
                    serials.push(e["serial"].as_str().unwrap().to_owned());
                }
            }
            serials
        };
        let mut streamed = vec![];
        let serial = |i: usize| format!("00005e{i:06x}");
        let big = "x".repeat(64 * 1024);
        let n = devices::MAX_PENDING + 16;
        for i in 0..n {
            let mut d = device(&registry, &cfg, REMOTE, None, None, &serial(i)).await;
            let state = json!({ "serial": serial(i), "uuid": 0, "state": { "junk": big } });
            send(&mut d.0, event::STATE, state).await;
            sleep(Duration::from_millis(1)).await;
            hang_up(d).await;
            streamed.extend(drain(&mut events));
        }
        {
            let reg = registry.registry.lock().await;
            assert_eq!(reg.devices.all().len(), devices::MAX_PENDING);
            let kept = (0..n).filter(|i| reg.states.get(&serial(*i)).is_some());
            assert_eq!(kept.count(), 0, "pending devices' states kept");
        }
        assert!(streamed.is_empty(), "{streamed:?}");
        assert!(!dir.join("state").exists());

        let mut ap = device(&registry, &cfg, REMOTE, None, None, AP).await;
        assert_eq!(next(&mut ap.0).await, Got::Nothing);
        control_request(Request::Adopt { serial: AP.into() }, &registry).await;
        take_credential(&mut ap.0, AP).await;
        let state = json!({ "serial": AP, "uuid": 0, "state": { "unit": { "load": [0.5] } } });
        send(&mut ap.0, event::STATE, state).await;
        sleep(Duration::from_millis(1)).await;
        assert_eq!(
            registry.devices().await[AP]["state"]["unit"]["load"][0],
            0.5
        );
        assert_eq!(drain(&mut events), [AP]);

        control_request(Request::Forget { serial: AP.into() }, &registry).await;
        let late = json!({ "serial": AP, "uuid": 0, "state": { "late": true } });
        let late = Message::notification(event::STATE, late).unwrap();
        let _ =
            ap.0.send(Frame::text(serde_json::to_string(&late).unwrap()))
                .await;
        hang_up(ap).await;
        assert!(registry.registry.lock().await.states.get(AP).is_none());
        registry.stop().await;
        assert!(!dir.join("state").join(format!("{AP}.json")).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A device connecting as `serial`, presenting `credential`, reporting `firmware` and
    /// `model`: its WebSocket, once `connect` is sent.
    async fn claim(
        registry: &Arc<Hub>,
        serial: &str,
        credential: Option<&str>,
        firmware: &str,
        model: &str,
    ) -> Device {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (device, controller) = tokio::io::duplex(1 << 16);
        let handshake = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let registry = registry.clone();
        let task = tokio::spawn(async move {
            serve(
                controller,
                REMOTE.parse().unwrap(),
                registry,
                None,
                handshake,
            )
            .await
            .map_err(|e| e.to_string())
        });
        let mut request = "ws://controller/".into_client_request().unwrap();
        if let Some(c) = credential {
            let bearer = format!("Bearer {c}").parse().unwrap();
            request.headers_mut().insert("authorization", bearer);
        }
        let (mut ws, _) = tokio_tungstenite::client_async(request, device)
            .await
            .unwrap();
        let hello = proto::Connect {
            serial: serial.into(),
            uuid: 0,
            firmware: firmware.into(),
            wanip: vec![],
            capabilities: json!({ "model": model }),
        };
        let hello = serde_json::to_value(hello).unwrap();
        send(&mut ws, event::CONNECT, hello).await;
        (ws, task)
    }

    /// Anyone who sees an adopted device's MAC can claim its serial: without its credential,
    /// the connection is closed and changes nothing, not the record and not devices.json.
    #[tokio::test(start_paused = true)]
    async fn a_refused_connection_leaves_the_record_and_flash_alone() {
        let (dir, registry) = registry("refused");
        let mut ap = claim(&registry, AP, None, "OpenWrt 25.12.0", "E8450").await;
        assert_eq!(next(&mut ap.0).await, Got::Nothing);
        control_request(Request::Adopt { serial: AP.into() }, &registry).await;
        let credential = take_credential(&mut ap.0, AP).await;
        sleep(Duration::from_millis(1)).await;
        hang_up(ap).await;
        let path = dir.join("devices.json");
        let listed = registry.devices().await[AP].clone();
        assert_eq!(listed["firmware"], "OpenWrt 25.12.0");
        // Any write would put it back.
        std::fs::remove_file(&path).unwrap();
        for presented in [None, Some("forged")] {
            let mut d = claim(&registry, AP, presented, "IMPOSTOR", "IMPOSTOR").await;
            assert_eq!(
                next(&mut d.0).await,
                Got::Closed(Some(1008)),
                "{presented:?}"
            );
            hang_up(d).await;
        }
        assert!(
            !path.exists(),
            "devices.json written for a refused connection"
        );
        assert_eq!(registry.devices().await[AP], listed);
        // The device itself: admitted, and what it reports now is kept.
        let ap = claim(&registry, AP, Some(&credential), "OpenWrt 25.12.1", "E8450").await;
        sleep(Duration::from_millis(1)).await;
        hang_up(ap).await;
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("OpenWrt 25.12.1") && !text.contains("IMPOSTOR"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The agent sends its serial in lower case: upper case would make one MAC two devices.
    #[tokio::test(start_paused = true)]
    async fn an_upper_case_serial_is_refused() {
        let (dir, registry) = registry("case");
        let cfg = dir.join("configs");
        for serial in ["00005E005302", "00005e00530A"] {
            let mut d = device(&registry, &cfg, REMOTE, None, None, serial).await;
            assert_eq!(next(&mut d.0).await, Got::Closed(Some(1008)), "{serial}");
            hang_up(d).await;
        }
        assert!(registry.devices().await.as_object().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(start_paused = true)]
    async fn adopt_local_never_takes_over_a_device_adopted_from_the_network() {
        let (dir, registry) = registry("takeover");
        let cfg = dir.join("configs");
        std::fs::create_dir_all(&cfg).unwrap();
        let config = json!({ "uuid": 9, "interfaces": [] }).to_string();
        std::fs::write(cfg.join(format!("{AP}.json")), config).unwrap();
        // An access point connects from the network, and is adopted and provisioned.
        let mut ap = device(&registry, &cfg, REMOTE, Some(HOST), None, AP).await;
        assert_eq!(next(&mut ap.0).await, Got::Nothing);
        control_request(Request::Adopt { serial: AP.into() }, &registry).await;
        let credential = take_credential(&mut ap.0, AP).await;
        let Got::Command(id, method, params) = next(&mut ap.0).await else {
            panic!("no configuration")
        };
        assert_eq!(
            (method.as_str(), params["uuid"].as_u64()),
            (command::CONFIGURE, Some(9))
        );
        answer(&mut ap.0, id, AP).await;
        hang_up(ap).await;

        // A local process claims its serial, without its credential: refused, and sent
        // nothing. So it would be even were the AP's serial taken for the host's own.
        for own in [HOST, AP] {
            let mut local = device(&registry, &cfg, LOOPBACK, Some(own), None, AP).await;
            assert_eq!(next(&mut local.0).await, Got::Closed(Some(1008)), "{own}");
            hang_up(local).await;
        }
        // The access point keeps its credential, and its configuration.
        let mut ap = device(&registry, &cfg, REMOTE, Some(HOST), Some(&credential), AP).await;
        let Got::Command(_, method, params) = next(&mut ap.0).await else {
            panic!("the AP wasn't admitted")
        };
        assert_eq!(
            (method.as_str(), params["uuid"].as_u64()),
            (command::CONFIGURE, Some(9))
        );
        hang_up(ap).await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
