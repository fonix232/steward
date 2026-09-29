//! steward-agent: connects this device to a Steward controller and applies
//! the configuration it sends (UCI and ubus on the device's side, uCentral's
//! protocol on the controller's).
//!
//! The channel is TLS (`wss://`). The first controller the agent reaches is
//! trusted on first use: its certificate authority is pinned in
//! `<state dir>/controller-ca.pem`, and from then on only a controller whose
//! certificate chains to that CA is accepted. Plain `ws://` needs
//! `--allow-plaintext` (development only).
//!
//! Without `--controller`, the agent uses the controller on its own device when
//! one is enabled there (the router that hosts it), and otherwise looks on the
//! default gateway.
//!
//! A configuration (`configure`) is rendered, staged in an rpcd session of the
//! agent's own and applied with rpcd's rollback (`apply`). It's confirmed only once
//! the controller can be reached again, so one that cuts the device off undoes
//! itself; the device then refuses that configuration until it gets another.
//!
//! Until the controller adopts the device, the agent has no credential and the
//! controller sends it nothing. Adoption delivers one (`steward.adopt`), kept in
//! `<state dir>/credential`; the agent presents it on every connection, as
//! `Authorization: Bearer` on the WebSocket upgrade.

mod apply;
mod device;
mod state;

use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use steward_proto::{self as proto, Message, command, event};
use steward_tls::{PinFile, PinnedCa, ServerName, TlsConnector, fingerprint};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, interval, sleep, sleep_until, timeout, timeout_at};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri, header::AUTHORIZATION};

macro_rules! log {
    ($($t:tt)*) => { eprintln!("steward-agent: {}", format_args!($($t)*)) };
}

const STATE_INTERVAL: Duration = Duration::from_secs(60);
/// A session that stays up this long after `connect` worked: the backoff starts over.
const ESTABLISHED: Duration = Duration::from_secs(10);
/// How long the controller has to take a connection, TCP, TLS and the WebSocket upgrade
/// together.
/// One that accepts it and never answers (stopped, or not a controller) is given up on, and
/// the backoff goes on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// A session that has heard nothing from the controller for this long is over (it went away
/// without closing, or hangs), and the backoff takes over. The agent pings it with every
/// state, so a controller that's there answers well within it.
const SILENCE: Duration = Duration::from_secs(180);
/// How long rpcd waits for the confirmation before it reverts an applied configuration.
const ROLLBACK: Duration = Duration::from_secs(60);
/// When to try the controller again, counted from the apply: at 5, 15 and 30 s.
const PROBES: [u64; 3] = [5, 15, 30];
/// How long one try may take. However slowly the tries fail, the last ends by 40 s, 20 s
/// before rpcd reverts, which leaves the confirmation time to get through.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// While another change waits for confirmation on the device (a LuCI Save & Apply, which
/// rpcd confirms or reverts within 90 s by default), try staging again this often...
const BUSY_RETRY: Duration = Duration::from_secs(10);
/// ...for this long, before answering 2.
const BUSY_PATIENCE: Duration = Duration::from_secs(120);

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut controller = None;
    let mut state_dir = PathBuf::from("/etc/steward-agent");
    let mut allow_plaintext = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => {
                println!("steward-agent {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--controller" => controller = it.next(),
            "--state-dir" => state_dir = it.next().expect("--state-dir <dir>").into(),
            "--allow-plaintext" => allow_plaintext = true,
            _ => {
                eprintln!(
                    "usage: steward-agent [--controller <wss://host:port>] [--state-dir <dir>] [--allow-plaintext]"
                );
                std::process::exit(2);
            }
        }
    }
    if let Some(url) = &controller {
        match url
            .parse::<Uri>()
            .ok()
            .and_then(|u| u.scheme_str().map(str::to_owned))
            .as_deref()
        {
            Some("wss") => {}
            Some("ws") if allow_plaintext => {
                log!("plain ws:// to {url} (--allow-plaintext): development only")
            }
            Some("ws") => {
                eprintln!(
                    "steward-agent: {url} is plain ws://; the channel is wss://, or pass --allow-plaintext"
                );
                std::process::exit(2);
            }
            _ => {
                eprintln!("steward-agent: {url} is not a wss:// URL");
                std::process::exit(2);
            }
        }
    }
    let pin = PinFile(state_dir.join("controller-ca.pem"));
    let credential = Credential(state_dir.join("credential"));
    let state = apply::State::new(&state_dir);
    // Kept across sessions, so the first report after a reconnect has rates too.
    let previous = std::sync::Mutex::new(state::Previous::default());

    // Reconnect for ever, backing off to a minute. Without a controller
    // given, it is on the default gateway (the router, which usually hosts
    // it), looked up again for every attempt.
    let mut backoff = Backoff::default();
    loop {
        let url = match &controller {
            Some(url) => url.clone(),
            None => match (
                tokio::task::spawn_blocking(device::local_controller_port)
                    .await
                    .ok()
                    .flatten(),
                device::default_gateway(),
            ) {
                // The router hosting the controller: its own, not its default gateway (the ISP).
                (Some(port), _) => format!("wss://127.0.0.1:{port}"),
                (None, Some(gw)) => format!("wss://{gw}:{}", proto::PORT),
                (None, None) => {
                    log!(
                        "no controller given, none on this device, and no default gateway to look for one on"
                    );
                    sleep(backoff.after(None)).await;
                    continue;
                }
            },
        };
        let ctx = Ctx {
            url: &url,
            pin: &pin,
            credential: &credential,
            state: &state,
            previous: &previous,
        };
        let mut connected = None;
        match session(&ctx, &mut connected).await {
            Ok(()) => log!("controller closed the connection"),
            Err(e) => log!("{url}: {e}"),
        }
        sleep(backoff.after(connected.map(|t: Instant| t.elapsed()))).await;
    }
}

/// How long to wait before the next attempt: a second, doubling to a minute. Every end
/// counts, the controller closing the connection too, so a controller that refuses the
/// device right after `connect` isn't asked again every second. Only a session that stayed
/// up for [`ESTABLISHED`] after `connect` starts it over: a controller that restarts is
/// rejoined within a second, however long it was unreachable before.
struct Backoff(Duration);

impl Backoff {
    const FIRST: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(60);

    /// The wait after an attempt that stayed up for `connected` after `connect` (`None`: it
    /// never got that far).
    fn after(&mut self, connected: Option<Duration>) -> Duration {
        if connected.is_some_and(|d| d >= ESTABLISHED) {
            self.0 = Backoff::FIRST;
        }
        let wait = self.0;
        self.0 = (self.0 * 2).min(Backoff::MAX);
        wait
    }
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff(Backoff::FIRST)
    }
}

/// What a session needs: where the controller is, and the device's own state.
struct Ctx<'a> {
    url: &'a str,
    pin: &'a PinFile,
    credential: &'a Credential,
    state: &'a apply::State,
    /// What the next state report's rates start from.
    previous: &'a std::sync::Mutex<state::Previous>,
}

/// A stream the WebSocket runs over: TCP, or TLS on TCP.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Opens the WebSocket to the controller, all within [`OPEN_TIMEOUT`]: TLS checked against the
/// pinned CA (or, with no pin yet, trusted on first use: the CA to pin comes back too), the
/// credential presented. A credential without a pin stops it before it connects: the
/// credential is only for the controller the lost pin named, and without the pin any
/// controller would be trusted and handed it.
async fn open(ctx: &Ctx<'_>) -> Result<(WebSocketStream<Box<dyn Io>>, Option<Vec<u8>>), Error> {
    let uri: Uri = ctx.url.parse()?;
    let tls = uri.scheme_str() != Some("ws");
    let pinned = if tls { ctx.pin.load()? } else { None };
    let mut request = ctx.url.into_client_request()?;
    if let Some(c) = ctx.credential.load()? {
        if tls && pinned.is_none() {
            return Err(format!(
                "not connecting: this device has a credential ({}) but no pinned controller ({} is missing), and the credential is only for the controller that pin named. Restore the pin, or remove both files to move this device to another controller",
                ctx.credential.path().display(),
                ctx.pin.0.display()
            )
            .into());
        }
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {c}"))?,
        );
    }
    let host = uri.host().ok_or("no host in the controller URL")?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(proto::PORT);
    let late = || format!("no answer in {} s", OPEN_TIMEOUT.as_secs());
    if !tls {
        let (ws, _) = timeout(OPEN_TIMEOUT, async {
            let tcp = TcpStream::connect((host.as_str(), port)).await?;
            let tcp = Box::new(tcp) as Box<dyn Io>;
            Ok::<_, Error>(tokio_tungstenite::client_async(request, tcp).await?)
        })
        .await
        .map_err(|_| late())??;
        return Ok((ws, None));
    }
    let verifier = PinnedCa::new(pinned.clone());
    let connector = TlsConnector::from(verifier.client_config()?);
    let name = ServerName::try_from(host.clone())?;
    let (ws, _) = timeout(OPEN_TIMEOUT, async {
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        let tls = Box::new(connector.connect(name, tcp).await?) as Box<dyn Io>;
        Ok::<_, Error>(tokio_tungstenite::client_async(request, tls).await?)
    })
    .await
    .map_err(|_| late())??;
    if let Some(ca) = &pinned {
        log!("controller's CA matches the pin {}", fingerprint(ca));
    }
    Ok((ws, verifier.first_use().map(|ca| ca.as_ref().to_vec())))
}

/// Connects to the controller, pinning its CA on first use, and runs the session: the
/// device's identity, then [`talk`]. `connected` is set once `connect` has gone out.
async fn session(ctx: &Ctx<'_>, connected: &mut Option<Instant>) -> Result<(), Error> {
    let (ws, first_use) = open(ctx).await?;
    if let Some(ca) = first_use {
        let ca = steward_tls::CertificateDer::from(ca);
        ctx.pin.save(&ca)?;
        log!(
            "pinned the controller's CA {} ({})",
            fingerprint(&ca),
            ctx.pin.0.display()
        );
    }
    let mut info = tokio::task::spawn_blocking(device::identity).await??;
    info.uuid = ctx.state.running().uuid;
    log!("connected to {} as {}", ctx.url, info.serial);
    talk(ws, info, report, ctx, connected).await
}

/// The device's state report: what it gathers now, with rates since the `previous` one.
fn report(previous: &mut state::Previous) -> Result<serde_json::Value, steward_ubus::Error> {
    let sources = state::gather()?;
    Ok(state::document(&sources, previous))
}

/// Whether the controller can be reached right now, within `timeout`: a fresh connection,
/// opened and closed without `connect`, so it doesn't register as the device.
async fn reachable(ctx: &Ctx<'_>, timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, open(ctx)).await {
        Ok(Ok((mut ws, _))) => {
            let _ = ws.close(None).await;
            true
        }
        _ => false,
    }
}

/// The credential the controller issued when it adopted this device.
struct Credential(PathBuf);

impl Credential {
    fn load(&self) -> Result<Option<String>, Error> {
        match std::fs::read_to_string(&self.0) {
            Ok(c) => Ok(Some(c.trim().to_owned()).filter(|c| !c.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", self.0.display()).into()),
        }
    }

    /// Stores it readable by root only, replacing any older one.
    fn save(&self, credential: &str) -> Result<(), Error> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.0.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.0.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(credential.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.0)?;
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

/// The session's messages: `connect`, then the device's `state` (made by `state`) and a ping
/// every [`STATE_INTERVAL`], and answers to the controller's commands. It ends when the
/// controller closes the connection, or when it has sent nothing for [`SILENCE`].
async fn talk<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    info: proto::Connect,
    state: fn(&mut state::Previous) -> Result<serde_json::Value, steward_ubus::Error>,
    ctx: &Ctx<'_>,
    connected: &mut Option<Instant>,
) -> Result<(), Error> {
    let serial = info.serial.clone();
    send(&mut ws, Message::notification(event::CONNECT, &info)?).await?;
    *connected = Some(Instant::now());

    let mut tick = interval(STATE_INTERVAL);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            frame = timeout_at(heard + SILENCE, ws.next()) => {
                let Ok(frame) = frame else {
                    let silent = SILENCE.as_secs();
                    return Err(format!("nothing from the controller for {silent} s").into());
                };
                heard = Instant::now();
                let text = match frame {
                    None => return Ok(()),
                    Some(frame) => match frame? {
                        Frame::Text(t) => t.to_string(),
                        Frame::Close(frame) => {
                            if let Some(f) = frame.filter(|f| !f.reason.is_empty()) {
                                log!("the controller closed the connection: {}", f.reason);
                            }
                            return Ok(());
                        }
                        _ => continue,
                    },
                };
                match serde_json::from_str::<Message>(&text) {
                    Ok(Message::Request { id, method, params, .. }) => {
                        let answer = handle(&serial, &method, params, ctx).await;
                        send(&mut ws, Message::result(id, answer)?).await?;
                    }
                    Ok(other) => log!("unexpected {other:?}"),
                    Err(e) => log!("unreadable message: {e}"),
                }
            }
            _ = tick.tick() => {
                let mut previous = std::mem::take(&mut *ctx.previous.lock().unwrap());
                let (state, previous) = tokio::task::spawn_blocking(move || {
                    let doc = state(&mut previous)?;
                    Ok::<_, steward_ubus::Error>((doc, previous))
                })
                .await??;
                *ctx.previous.lock().unwrap() = previous;
                let uuid = ctx.state.running().uuid;
                let params = proto::State { serial: serial.clone(), uuid, request_uuid: None, state };
                send(&mut ws, Message::notification(event::STATE, &params)?).await?;
                // Answered (a pong) by a controller that's there, even with nothing to send.
                ws.send(Frame::Ping(Default::default())).await?;
            }
        }
    }
}

async fn send<S>(ws: &mut S, m: Message) -> Result<(), Error>
where
    S: SinkExt<Frame> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    ws.send(Frame::text(serde_json::to_string(&m)?)).await?;
    Ok(())
}

/// A controller command's answer.
async fn handle(
    serial: &str,
    method: &str,
    params: serde_json::Value,
    ctx: &Ctx<'_>,
) -> proto::CommandResult {
    let status = |error, text: &str| proto::CommandStatus {
        error,
        text: text.into(),
        when: None,
        rejected: vec![],
    };
    match method {
        command::ADOPT => {
            let result = serde_json::from_value::<proto::Adopt>(params)
                .map_err(Error::from)
                .and_then(|a| {
                    if a.serial == serial {
                        ctx.credential.save(&a.credential)
                    } else {
                        Err(format!("a credential for {}, not this device", a.serial).into())
                    }
                });
            let (error, text) = match result {
                Ok(()) => {
                    log!(
                        "adopted by the controller; credential saved in {}",
                        ctx.credential.path().display()
                    );
                    (0, "adopted".to_string())
                }
                Err(e) => {
                    log!("couldn't take the credential: {e}");
                    (1, e.to_string())
                }
            };
            proto::CommandResult {
                serial: serial.into(),
                uuid: None,
                status: status(error, &text),
            }
        }
        command::CONFIGURE => {
            let uuid = params.get("uuid").and_then(|u| u.as_u64());
            proto::CommandResult {
                serial: serial.into(),
                uuid,
                status: configure(params, ctx).await,
            }
        }
        _ => proto::CommandResult {
            serial: serial.into(),
            uuid: None,
            status: status(1, &format!("{method}: not supported")),
        },
    }
}

/// Tries `probe` (given how long it may take) at each of [`PROBES`] until one succeeds. They're
/// counted from `applied`, so a slow try delays the next one instead of pushing the last one
/// past rpcd's window.
async fn tries(applied: Instant, mut probe: impl AsyncFnMut(Duration) -> bool) -> bool {
    for at in PROBES {
        sleep_until(applied + Duration::from_secs(at)).await;
        if probe(PROBE_TIMEOUT).await {
            return true;
        }
    }
    false
}

/// Stages with `stage` (blocking), and again every `every` while another change waits for
/// confirmation on the device, for up to `patience`: rpcd applies one at a time. Still
/// [`apply::Staged::Busy`] after that.
async fn stage_when_free<F>(
    stage: F,
    every: Duration,
    patience: Duration,
) -> Result<apply::Staged, String>
where
    F: Fn() -> Result<apply::Staged, String> + Clone + Send + 'static,
{
    let start = Instant::now();
    loop {
        match tokio::task::spawn_blocking(stage.clone()).await {
            Ok(Ok(apply::Staged::Busy)) if start.elapsed() + every <= patience => {
                log!(
                    "another change is waiting for confirmation on the device; trying again in {every:?}"
                );
                sleep(every).await;
            }
            Ok(staged) => return staged,
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `configure`: render, stage, apply with a rollback, and confirm once the controller answers.
async fn configure(params: serde_json::Value, ctx: &Ctx<'_>) -> proto::CommandStatus {
    let refuse = |text: String| {
        log!("configuration refused: {text}");
        apply::status(vec![], false, &text)
    };
    let c: proto::Configure = match serde_json::from_value(params) {
        Ok(c) => c,
        Err(e) => return refuse(format!("unreadable configuration: {e}")),
    };
    let mut running = ctx.state.running();
    if running.rolled_back == Some(c.uuid) {
        return refuse(format!(
            "configuration {} rolled back on this device before; send a new one",
            c.uuid
        ));
    }
    if running.uuid == c.uuid {
        return apply::status(vec![], true, "already running");
    }
    let (dir, config) = (ctx.state.dir().to_owned(), c.config.clone());
    let stage = move || apply::stage(&apply::State::new(&dir), &config, ROLLBACK);
    let staged = match stage_when_free(stage, BUSY_RETRY, BUSY_PATIENCE).await {
        Ok(s) => s,
        Err(e) => return refuse(e),
    };
    let (plan, transaction) = match staged {
        apply::Staged::Nothing(plan) => {
            running.uuid = c.uuid;
            running.rolled_back = None;
            if let Err(e) = ctx.state.set_running(&running) {
                return refuse(e);
            }
            log!("configuration {}: nothing to change", c.uuid);
            return apply::status(plan.rejected, true, "nothing to change");
        }
        apply::Staged::Applied(plan, t) => (plan, t),
        apply::Staged::Busy => {
            return refuse(format!(
                "another change is still waiting for confirmation on the device after {BUSY_PATIENCE:?} (LuCI's Save & Apply?): try again"
            ));
        }
    };
    log!(
        "configuration {} applied ({} UCI operations); confirming once the controller answers",
        c.uuid,
        plan.ops.len()
    );
    let confirmed = tries(Instant::now(), async |timeout| {
        reachable(ctx, timeout).await
    })
    .await;
    let rejected = plan.rejected;
    // Confirmed, or else reverted before the answer goes out (apply::settle).
    let settled = tokio::task::spawn_blocking(move || {
        let mut t = transaction;
        let settled = apply::settle(confirmed, &mut t);
        (settled, t)
    })
    .await;
    let settled = match settled {
        Ok(((settled, problems), t)) => {
            for p in problems {
                log!("configuration {}: {p}", c.uuid);
            }
            if settled == apply::Settled::Pending {
                // Keep the session until the window has passed, so nothing can confirm it,
                // then let it go.
                tokio::spawn(async move {
                    sleep(ROLLBACK + Duration::from_secs(5)).await;
                    let _ = tokio::task::spawn_blocking(move || drop(t)).await;
                });
            }
            settled
        }
        Err(e) => {
            log!("configuration {}: {e}", c.uuid);
            apply::Settled::Pending
        }
    };
    if settled == apply::Settled::Kept {
        running.uuid = c.uuid;
        running.rolled_back = None;
        if let Err(e) = ctx.state.set_running(&running) {
            log!("{e}");
        }
        log!("configuration {} confirmed", c.uuid);
        return apply::status(rejected, true, "applied");
    }
    running.rolled_back = Some(c.uuid);
    if let Err(e) = ctx.state.set_running(&running) {
        log!("{e}");
    }
    let why = if confirmed {
        "confirming it failed"
    } else {
        "the controller was unreachable after applying it"
    };
    let text = match settled {
        apply::Settled::Reverted => format!("rolled back: {why}"),
        _ => format!("rolling back when rpcd's window ends: {why}"),
    };
    log!("configuration {}: {text}", c.uuid);
    apply::status(rejected, false, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The next `n` waits after attempts that stayed up for `connected` after `connect`.
    fn waits(b: &mut Backoff, connected: Option<Duration>, n: usize) -> Vec<u64> {
        (0..n).map(|_| b.after(connected).as_secs()).collect()
    }

    #[test]
    fn the_backoff_starts_over_only_after_a_session_that_worked() {
        // Nothing answers: 1, 2, 4 ... up to a minute.
        let mut b = Backoff::default();
        assert_eq!(waits(&mut b, None, 8), [1, 2, 4, 8, 16, 32, 60, 60]);
        // A long session, then the controller restarts: tried again within a second, and
        // backing off from there while it's still away.
        assert_eq!(waits(&mut b, Some(Duration::from_secs(3600)), 1), [1]);
        assert_eq!(waits(&mut b, None, 3), [2, 4, 8]);
        assert_eq!(waits(&mut b, Some(ESTABLISHED), 1), [1]);

        // A controller that closes every connection right after `connect` (it refuses the
        // device, say) is a failure like any other.
        let mut b = Backoff::default();
        let refused = Some(Duration::from_millis(50));
        assert_eq!(waits(&mut b, refused, 8), [1, 2, 4, 8, 16, 32, 60, 60]);
        let almost = ESTABLISHED - Duration::from_millis(1);
        assert_eq!(waits(&mut b, Some(almost), 1), [60]);
    }

    fn identity() -> proto::Connect {
        proto::Connect {
            serial: "00005e005301".into(),
            uuid: 0,
            firmware: "OpenWrt".into(),
            wanip: vec![],
            capabilities: serde_json::json!({}),
        }
    }

    /// A context for a device not adopted yet, with no pin, running nothing: leaked, so a
    /// spawned session can hold it.
    fn ctx(url: &str) -> Ctx<'static> {
        let dir = PathBuf::from("/nonexistent/steward-agent");
        Ctx {
            url: Box::leak(url.to_owned().into_boxed_str()),
            pin: Box::leak(Box::new(PinFile(dir.join("controller-ca.pem")))),
            credential: Box::leak(Box::new(Credential(dir.join("credential")))),
            state: Box::leak(Box::new(apply::State::new(&dir))),
            previous: Box::leak(Box::default()),
        }
    }

    fn state(_: &mut state::Previous) -> Result<serde_json::Value, steward_ubus::Error> {
        Ok(serde_json::json!({ "unit": {} }))
    }

    /// A WebSocket in memory: the agent's end and the controller's.
    async fn pair() -> (
        WebSocketStream<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (agent, controller) = tokio::io::duplex(1 << 16);
        let controller = tokio::spawn(tokio_tungstenite::accept_async(controller));
        let (agent, _) = tokio_tungstenite::client_async("ws://controller/", agent)
            .await
            .unwrap();
        (agent, controller.await.unwrap().unwrap())
    }

    /// Asserts that `took` is `limit` (the clock is paused: exactly, give or take a tick).
    fn about(took: Duration, limit: Duration) {
        assert!(
            took >= limit && took < limit + Duration::from_secs(1),
            "{took:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_controller_that_takes_the_connection_and_never_answers_is_given_up_on() {
        // The listen backlog takes the connection; nothing ever answers the upgrade, nor, over
        // TLS, the handshake (no pin here: it would be the first use).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        for url in [format!("ws://{addr}"), format!("wss://{addr}")] {
            let start = Instant::now();
            let mut connected = None;
            let ended = timeout(
                Duration::from_secs(3600),
                session(&ctx(&url), &mut connected),
            )
            .await;
            let err = ended.expect("still waiting after an hour").unwrap_err();
            assert!(
                err.to_string().contains("no answer in 30 s"),
                "{url}: {err}"
            );
            about(start.elapsed(), OPEN_TIMEOUT);
            assert!(connected.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_controller_that_goes_silent_ends_the_session() {
        // The controller's end is never read: nothing comes back, not even a pong.
        let (agent, _controller) = pair().await;
        let start = Instant::now();
        let mut connected = None;
        let ctx = ctx("ws://controller/");
        let talking = talk(agent, identity(), state, &ctx, &mut connected);
        let ended = timeout(Duration::from_secs(24 * 3600), talking).await;
        let err = ended.expect("still talking after a day").unwrap_err();
        assert!(
            err.to_string().contains("nothing from the controller"),
            "{err}"
        );
        about(start.elapsed(), SILENCE);
        assert!(connected.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_controller_with_nothing_to_send_keeps_the_session() {
        let (agent, mut controller) = pair().await;
        let ctx = ctx("ws://controller/");
        let mut connected = None;
        let talking = talk(agent, identity(), state, &ctx, &mut connected);
        tokio::pin!(talking);
        // The controller only reads, which answers the agent's pings, for a day.
        let day = Instant::now() + Duration::from_secs(24 * 3600);
        let (mut states, mut pings) = (0, 0);
        loop {
            tokio::select! {
                ended = &mut talking => panic!("the session ended: {:?}", ended.map_err(|e| e.to_string())),
                frame = timeout_at(day, controller.next()) => match frame {
                    Ok(Some(frame)) => match frame.unwrap() {
                        Frame::Text(t) if t.contains(r#""method":"state""#) => states += 1,
                        Frame::Ping(_) => pings += 1,
                        _ => {}
                    },
                    _ => break,
                },
            }
        }
        assert!(
            states >= 24 * 60 && pings >= 24 * 60,
            "{states} states, {pings} pings"
        );
        // It ends when the controller closes the connection.
        controller.close(None).await.unwrap();
        talking.await.unwrap();
    }

    /// A controller on loopback with its own CA (created in `dir`): its URL, its identity, and
    /// what the first upgrade within `wait` carried as `Authorization` (`None`: nothing
    /// connected).
    // tungstenite's upgrade callback returns its own (large) error response type.
    #[allow(clippy::result_large_err)]
    async fn controller(
        dir: &Path,
        wait: Duration,
    ) -> (
        String,
        steward_tls::ControllerIdentity,
        tokio::task::JoinHandle<Option<Option<String>>>,
    ) {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let id = steward_tls::ControllerIdentity::load_or_create(dir).unwrap();
        let acceptor = steward_tls::TlsAcceptor::from(id.server_config().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("wss://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (tcp, _) = tokio::time::timeout(wait, listener.accept())
                .await
                .ok()?
                .ok()?;
            let tls = acceptor.accept(tcp).await.ok()?;
            let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
            let s = seen.clone();
            let _ws =
                tokio_tungstenite::accept_hdr_async(tls, move |req: &Request, resp: Response| {
                    let bearer = req.headers().get("authorization");
                    *s.lock().unwrap() = bearer.and_then(|v| v.to_str().ok()).map(str::to_owned);
                    Ok(resp)
                })
                .await;
            let header = seen.lock().unwrap().take();
            Some(header)
        });
        (url, id, task)
    }

    #[tokio::test]
    async fn the_credential_goes_only_to_the_pinned_controller() {
        let dir = std::env::temp_dir().join(format!("steward-agent-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pin = PinFile(dir.join("agent/controller-ca.pem"));
        let credential = Credential(dir.join("agent/credential"));
        let state = apply::State::new(&dir.join("agent"));
        let previous = std::sync::Mutex::default();
        let session = async |url: &str| {
            let mut connected = None;
            let ctx = Ctx {
                url,
                pin: &pin,
                credential: &credential,
                state: &state,
                previous: &previous,
            };
            let s = session(&ctx, &mut connected);
            tokio::time::timeout(Duration::from_secs(10), s)
                .await
                .expect("a session that doesn't end")
        };

        // A credential and no pin (lost): refused before connecting, and nothing pinned.
        credential.save("the-credential").unwrap();
        let (url, a, seen) = controller(&dir.join("a"), Duration::from_millis(500)).await;
        let err = session(&url).await.unwrap_err().to_string();
        assert!(err.contains("no pinned controller"), "{err}");
        assert_eq!(seen.await.unwrap(), None, "it connected");
        assert!(pin.load().unwrap().is_none());

        // With the pin of that controller, the credential goes with the upgrade. (The session
        // then ends: this controller hangs up after the upgrade.)
        pin.save(&a.ca).unwrap();
        let (url, _, seen) = controller(&dir.join("a"), Duration::from_secs(10)).await;
        let _ = session(&url).await;
        let header = seen.await.unwrap();
        assert_eq!(header, Some(Some("Bearer the-credential".into())));

        // Neither (a device moved on purpose): the next controller is trusted on first use and
        // pinned, and nothing is presented to it.
        std::fs::remove_file(&pin.0).unwrap();
        std::fs::remove_file(credential.path()).unwrap();
        let (url, other, seen) = controller(&dir.join("b"), Duration::from_secs(10)).await;
        let _ = session(&url).await;
        assert_eq!(seen.await.unwrap(), Some(None));
        assert_eq!(pin.load().unwrap(), Some(other.ca));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Runs `f` with a context whose state directory is fresh (holding `running`, if given)
    /// and whose controller is unreachable.
    async fn with_ctx<T>(running: Option<apply::Running>, f: impl AsyncFnOnce(&Ctx<'_>) -> T) -> T {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "steward-configure-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let state = apply::State::new(&dir);
        if let Some(r) = running {
            state.set_running(&r).unwrap();
        }
        let pin = PinFile(dir.join("controller-ca.pem"));
        let credential = Credential(dir.join("credential"));
        let previous = std::sync::Mutex::new(state::Previous::default());
        let ctx = Ctx {
            url: "wss://127.0.0.1:1",
            pin: &pin,
            credential: &credential,
            state: &state,
            previous: &previous,
        };
        let out = f(&ctx).await;
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn params(uuid: u64) -> serde_json::Value {
        json!({ "serial": "000000000000", "uuid": uuid, "config": { "uuid": uuid, "interfaces": [] } })
    }

    #[tokio::test]
    async fn a_configuration_that_rolled_back_is_refused_until_a_new_uuid() {
        let running = apply::Running {
            uuid: 5,
            rolled_back: Some(9),
        };
        with_ctx(Some(running.clone()), async |ctx| {
            let s = configure(params(9), ctx).await;
            assert_eq!(s.error, proto::configure_error::REJECTED);
            assert!(s.text.contains("rolled back"), "{}", s.text);
            assert_eq!(configure(params(9), ctx).await.error, 2);
            assert_eq!(ctx.state.running(), running);
        })
        .await;
    }

    #[tokio::test]
    async fn the_running_uuid_is_answered_already_running() {
        let running = apply::Running {
            uuid: 7,
            rolled_back: None,
        };
        with_ctx(Some(running), async |ctx| {
            let s = configure(params(7), ctx).await;
            assert_eq!(s.error, proto::configure_error::APPLIED);
            assert_eq!(s.text, "already running");
        })
        .await;
    }

    #[tokio::test]
    async fn an_unreadable_configuration_is_answered_2() {
        with_ctx(None, async |ctx| {
            let s = configure(json!({ "serial": "x", "config": {} }), ctx).await;
            assert_eq!(s.error, proto::configure_error::REJECTED);
            assert!(s.text.contains("unreadable"), "{}", s.text);
            assert_eq!(ctx.state.running(), apply::Running::default());
        })
        .await;
    }

    /// Off a device (no ubus), staging fails at once: answered 2, running.json untouched, and
    /// nothing recorded as rolled back.
    #[tokio::test]
    async fn a_failure_before_applying_is_answered_2_and_changes_nothing() {
        if std::path::Path::new(steward_ubus::SOCKET).exists() {
            return; // on a device, stage() would really run
        }
        let running = apply::Running {
            uuid: 3,
            rolled_back: None,
        };
        with_ctx(Some(running.clone()), async |ctx| {
            let s = configure(params(4), ctx).await;
            assert_eq!(s.error, proto::configure_error::REJECTED, "{}", s.text);
            assert_eq!(ctx.state.running(), running);
        })
        .await;
    }

    /// While another change waits for confirmation (rpcd refuses the apply), staging is tried
    /// again until it goes through, or until the patience runs out: then it's still busy, which
    /// configure answers 2.
    #[tokio::test]
    async fn staging_is_tried_again_while_another_change_is_pending() {
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        let stage = move || {
            if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                Ok(apply::Staged::Busy)
            } else {
                Ok(apply::Staged::Nothing(Default::default()))
            }
        };
        let staged =
            stage_when_free(stage, Duration::from_millis(10), Duration::from_secs(5)).await;
        assert!(matches!(staged, Ok(apply::Staged::Nothing(_))));
        assert_eq!(tries.load(Ordering::SeqCst), 3);

        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        let busy = move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(apply::Staged::Busy)
        };
        let start = Instant::now();
        let staged =
            stage_when_free(busy, Duration::from_millis(20), Duration::from_millis(100)).await;
        assert!(matches!(staged, Ok(apply::Staged::Busy)));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!((3..=6).contains(&tries.load(Ordering::SeqCst)), "{tries:?}");

        // Other failures aren't retried.
        let failed = stage_when_free(
            || Err("reading the wireless config: no".to_string()),
            Duration::from_secs(10),
            Duration::from_secs(120),
        );
        let failed = tokio::time::timeout(Duration::from_secs(1), failed).await;
        assert!(matches!(failed, Ok(Err(_))));
    }

    /// However slowly the tries fail (each takes its whole timeout), they start at 5, 15 and
    /// 30 s and the last ends by 40 s, well inside rpcd's window, with time left to confirm.
    #[tokio::test(start_paused = true)]
    async fn the_confirmation_tries_end_well_inside_the_rollback_window() {
        let applied = Instant::now();
        let mut started = vec![];
        let confirmed = tries(applied, async |timeout| {
            started.push(applied.elapsed().as_secs());
            sleep(timeout).await;
            false
        })
        .await;
        assert!(!confirmed);
        assert_eq!(started, [5, 15, 30]);
        assert_eq!(applied.elapsed(), Duration::from_secs(40));
        assert!(applied.elapsed() + Duration::from_secs(15) <= ROLLBACK);
        // The first to succeed ends them.
        let mut n = 0;
        assert!(
            tries(Instant::now(), async |_| {
                n += 1;
                n == 2
            })
            .await
        );
        assert_eq!(n, 2);
    }

    /// A controller that accepts the connection and never answers holds a try no longer than
    /// its timeout.
    #[tokio::test]
    async fn a_silent_controller_holds_a_try_only_until_its_timeout() {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("wss://{}", silent.local_addr().unwrap());
        with_ctx(None, async |ctx| {
            assert!(
                !reachable(ctx, Duration::from_secs(1)).await,
                "nothing on port 1"
            );
            let hung = Ctx { url: &url, ..*ctx };
            let start = Instant::now();
            assert!(!reachable(&hung, Duration::from_millis(300)).await);
            assert!(start.elapsed() < Duration::from_secs(2));
        })
        .await;
    }
}
