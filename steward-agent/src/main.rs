//! steward-agent: connects this device to a Steward controller and applies
//! the configuration it sends (UCI and ubus on the device's side, uCentral's
//! protocol on the controller's).

mod device;

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use steward_proto::{self as proto, Message, command, event};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, interval, sleep, timeout, timeout_at};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as Frame;

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
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => {
                println!("steward-agent {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--controller" => controller = it.next(),
            _ => {
                eprintln!("usage: steward-agent --controller <ws://host:port>");
                std::process::exit(2);
            }
        }
    }
    let Some(url) = controller else {
        eprintln!(
            "steward-agent: no controller given (--controller ws://host:{})",
            proto::PORT
        );
        std::process::exit(2);
    };

    // Reconnect for ever, backing off to a minute.
    let mut backoff = Backoff::default();
    loop {
        let mut connected = None;
        match session(&url, &mut connected).await {
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

/// One session, from the WebSocket to its end. `connected` is set once `connect` has gone out.
async fn session(url: &str, connected: &mut Option<Instant>) -> Result<(), Error> {
    let (ws, _) = timeout(OPEN_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| format!("no answer in {} s", OPEN_TIMEOUT.as_secs()))??;
    let info = tokio::task::spawn_blocking(device::identity).await??;
    log!("connected to {url} as {}", info.serial);
    talk(ws, info, device::state, connected).await
}

/// The session's messages: `connect`, then the device's `state` (read by `state`) and a ping
/// every [`STATE_INTERVAL`], and answers to the controller's commands. It ends when the
/// controller closes the connection, or when it has sent nothing for [`SILENCE`].
async fn talk<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    info: proto::Connect,
    state: fn() -> Result<serde_json::Value, steward_ubus::Error>,
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
                        Frame::Close(_) => return Ok(()),
                        _ => continue,
                    },
                };
                match serde_json::from_str::<Message>(&text) {
                    Ok(Message::Request { id, method, params, .. }) => {
                        let answer = handle(&serial, &method, params).await;
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
async fn handle(serial: &str, method: &str, params: serde_json::Value) -> proto::CommandResult {
    let status = |error, text: &str| proto::CommandStatus {
        error,
        text: text.into(),
        when: None,
        rejected: vec![],
    };
    match method {
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
        // The listen backlog takes the connection; nothing ever answers the upgrade.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let start = Instant::now();
        let mut connected = None;
        let ended = timeout(Duration::from_secs(3600), session(&url, &mut connected)).await;
        let err = ended.expect("still waiting after an hour").unwrap_err();
        assert!(err.to_string().contains("no answer in 30 s"), "{err}");
        about(start.elapsed(), OPEN_TIMEOUT);
        assert!(connected.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_controller_that_goes_silent_ends_the_session() {
        // The controller's end is never read: nothing comes back, not even a pong.
        let (agent, _controller) = pair().await;
        let start = Instant::now();
        let mut connected = None;
        let talking = talk(agent, identity(), state, &mut connected);
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
            talk(agent, identity(), state, &mut connected)
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
}
