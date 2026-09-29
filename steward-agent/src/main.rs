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
//! Until the controller adopts the device, the agent has no credential and the
//! controller sends it nothing. Adoption delivers one (`steward.adopt`), kept in
//! `<state dir>/credential`; the agent presents it on every connection, as
//! `Authorization: Bearer` on the WebSocket upgrade.

mod device;

use futures_util::{SinkExt, StreamExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use steward_proto::{self as proto, Message, command, event};
use steward_tls::{PinFile, PinnedCa, ServerName, TlsConnector, fingerprint};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, interval, sleep, timeout, timeout_at};
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
/// How long the controller has to take a connection, TCP and the WebSocket upgrade together.
/// One that accepts it and never answers (stopped, or not a controller) is given up on, and
/// the backoff goes on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// A session that has heard nothing from the controller for this long is over (it went away
/// without closing, or hangs), and the backoff takes over. The agent pings it with every
/// state, so a controller that's there answers well within it.
const SILENCE: Duration = Duration::from_secs(180);

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
        let mut connected = None;
        match session(&url, &pin, &credential, &mut connected).await {
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

/// Connects to the controller: TLS checked against the pinned CA (or trusted on first use and
/// pinned), then the WebSocket, all within [`OPEN_TIMEOUT`]. `connected` is set once `connect`
/// has gone out. A credential without a pin stops it before it connects: the credential is only
/// for the controller the lost pin named, and without the pin any controller would be trusted
/// and handed it.
async fn session(
    url: &str,
    pin: &PinFile,
    credential: &Credential,
    connected: &mut Option<Instant>,
) -> Result<(), Error> {
    let uri: Uri = url.parse()?;
    let tls = uri.scheme_str() != Some("ws");
    let pinned = if tls { pin.load()? } else { None };
    let mut request = url.into_client_request()?;
    if let Some(c) = credential.load()? {
        if tls && pinned.is_none() {
            return Err(format!(
                "not connecting: this device has a credential ({}) but no pinned controller ({} is missing), and the credential is only for the controller that pin named. Restore the pin, or remove both files to move this device to another controller",
                credential.path().display(),
                pin.0.display()
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
            Ok::<_, Error>(tokio_tungstenite::client_async(request, tcp).await?)
        })
        .await
        .map_err(|_| late())??;
        return start(ws, url, credential, connected).await;
    }
    let verifier = PinnedCa::new(pinned.clone());
    let connector = TlsConnector::from(verifier.client_config()?);
    let name = ServerName::try_from(host.clone())?;
    let (ws, _) = timeout(OPEN_TIMEOUT, async {
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        let tls = connector.connect(name, tcp).await?;
        Ok::<_, Error>(tokio_tungstenite::client_async(request, tls).await?)
    })
    .await
    .map_err(|_| late())??;
    match verifier.first_use() {
        Some(ca) => {
            pin.save(&ca)?;
            log!(
                "pinned the controller's CA {} ({})",
                fingerprint(&ca),
                pin.0.display()
            );
        }
        None => {
            if let Some(ca) = &pinned {
                log!("controller's CA matches the pin {}", fingerprint(ca));
            }
        }
    }
    start(ws, url, credential, connected).await
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

/// A session on an open WebSocket: the device's identity, then [`talk`].
async fn start<S: AsyncRead + AsyncWrite + Unpin>(
    ws: WebSocketStream<S>,
    url: &str,
    credential: &Credential,
    connected: &mut Option<Instant>,
) -> Result<(), Error> {
    let info = tokio::task::spawn_blocking(device::identity).await??;
    log!("connected to {url} as {}", info.serial);
    talk(ws, info, device::state, credential, connected).await
}

/// The session's messages: `connect`, then the device's `state` (read by `state`) and a ping
/// every [`STATE_INTERVAL`], and answers to the controller's commands. It ends when the
/// controller closes the connection, or when it has sent nothing for [`SILENCE`].
async fn talk<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    info: proto::Connect,
    state: fn() -> Result<serde_json::Value, steward_ubus::Error>,
    credential: &Credential,
    connected: &mut Option<Instant>,
) -> Result<(), Error> {
    let serial = info.serial.clone();
    let uuid = device::running_uuid();
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
                        let answer = handle(&serial, &method, params, credential).await;
                        send(&mut ws, Message::result(id, answer)?).await?;
                    }
                    Ok(other) => log!("unexpected {other:?}"),
                    Err(e) => log!("unreadable message: {e}"),
                }
            }
            _ = tick.tick() => {
                let state = tokio::task::spawn_blocking(state).await??;
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
    credential: &Credential,
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
                        credential.save(&a.credential)
                    } else {
                        Err(format!("a credential for {}, not this device", a.serial).into())
                    }
                });
            let (error, text) = match result {
                Ok(()) => {
                    log!(
                        "adopted by the controller; credential saved in {}",
                        credential.path().display()
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
            log!("configuration {uuid:?} received; applying configurations is not implemented yet");
            proto::CommandResult {
                serial: serial.into(),
                uuid,
                status: status(
                    proto::configure_error::REJECTED,
                    "steward-agent does not apply configurations yet",
                ),
            }
        }
        _ => proto::CommandResult {
            serial: serial.into(),
            uuid: None,
            status: status(1, &format!("{method}: not supported")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// No credential (a device not adopted yet).
    fn nothing() -> Credential {
        Credential(PathBuf::from("/nonexistent/steward-agent/credential"))
    }

    fn state() -> Result<serde_json::Value, steward_ubus::Error> {
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
        let pin = PinFile(PathBuf::from(
            "/nonexistent/steward-agent/controller-ca.pem",
        ));
        let credential = Credential(PathBuf::from("/nonexistent/steward-agent/credential"));
        for url in [format!("ws://{addr}"), format!("wss://{addr}")] {
            let start = Instant::now();
            let mut connected = None;
            let ended = timeout(
                Duration::from_secs(3600),
                session(&url, &pin, &credential, &mut connected),
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
        let credential = nothing();
        let talking = talk(agent, identity(), state, &credential, &mut connected);
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
        let session = tokio::spawn(async move {
            let mut connected = None;
            talk(agent, identity(), state, &nothing(), &mut connected)
                .await
                .map_err(|e| e.to_string())
        });
        // The controller only reads, which answers the agent's pings, for a day.
        let day = Instant::now() + Duration::from_secs(24 * 3600);
        let (mut states, mut pings) = (0, 0);
        while let Ok(Some(frame)) = timeout_at(day, controller.next()).await {
            match frame.unwrap() {
                Frame::Text(t) if t.contains(r#""method":"state""#) => states += 1,
                Frame::Ping(_) => pings += 1,
                _ => {}
            }
        }
        assert!(!session.is_finished(), "{:?}", session.await);
        assert!(
            states >= 24 * 60 && pings >= 24 * 60,
            "{states} states, {pings} pings"
        );
        // It ends when the controller closes the connection.
        controller.close(None).await.unwrap();
        session.await.unwrap().unwrap();
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
        let session = async |url: &str| {
            let mut connected = None;
            let s = session(url, &pin, &credential, &mut connected);
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
}
