//! The controller's HTTPS API, for the web interface and scripts.
//!
//! Sign-in uses OpenWrt's own users: `POST /api/login` checks the username and password
//! with rpcd (`session login`, as LuCI does) and hands back rpcd's session token, which
//! every other call presents as `Authorization: Bearer <token>` or the `steward_session`
//! cookie (which the browser's EventSource can send). rpcd's session timeout and logout
//! apply; the controller keeps no passwords.
//!
//! Cookies aren't port-specific: other pages on the router (LuCI on 443) are the same site,
//! and a browser sends them the cookie too. So it's scoped to `Path=/api`, and a write
//! (anything but GET and HEAD) is refused (403) when it comes from another origin: a
//! `Sec-Fetch-Site` other than `same-origin`, or an `Origin` other than `https://<Host>`.
//! A write signed in by the cookie must also carry `X-Steward: 1`, a header no other origin
//! can add without a CORS preflight, which the API never grants. A bearer token is never
//! sent by the browser on its own, so a write carrying one needs no such header.
//!
//! A request's body has 10 s from its headers ([`BODY_TIMEOUT`]), or it gets 408. Signing in
//! reads one before anyone is signed in, so it takes at most [`LOGIN_BODY`] bytes.
//!
//! What an account may do is rpcd's too, as in LuCI: the access group `steward`
//! (`/usr/share/rpcd/acl.d/steward-controller.json`). Its `read` lets a session see (the
//! GETs and the event stream), its `write` change something (everything else). A login with
//! `read '*'` and `write '*'`, as root's, has both; one with only `read 'steward'` looks
//! without changing; one without the group is refused at sign-in. rpcd's unauthenticated
//! session (all zeros) is never a sign-in.
//!
//!     POST /api/login                    {"username", "password"} -> {"token"}
//!     POST /api/logout                   ends the session the call presents
//!     GET  /api/devices                  every device: standing, connected, running uuid, state
//!     POST /api/devices/{serial}/adopt
//!     POST /api/devices/{serial}/forget
//!     GET  /api/devices/{serial}/config  the stored uCentral configuration
//!     PUT  /api/devices/{serial}/config  store one (a new uuid), and send it when adopted
//!     GET  /api/events                   Server-Sent Events: what happens, as it happens

use crate::hub::{Hub, OpError};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Who may use the API.
pub trait Auth: Send + Sync + 'static {
    /// A session token for these credentials, or `None`.
    fn login(&self, username: &str, password: &str) -> Option<String>;
    /// Whether the token is a live session whose account has `right`.
    fn check(&self, token: &str, right: Right) -> Access;
    fn logout(&self, token: &str);
}

/// rpcd's access group that holds Steward's rights; the package's ACL file defines it.
pub const GROUP: &str = "steward";

/// A right in [`GROUP`]: `read` to see, `write` to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    Read,
    Write,
}

impl std::fmt::Display for Right {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Right::Read => "read",
            Right::Write => "write",
        })
    }
}

/// What a session may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Granted,
    /// Signed in, but the account doesn't have the right.
    Denied,
    /// No such session: signed out, timed out, or never signed in.
    Ended,
}

/// rpcd's sessions, over ubus: OpenWrt's users, as LuCI signs them in.
pub struct Rpcd;

impl Rpcd {
    fn call(method: &str, args: Value) -> Option<serde_json::Map<String, Value>> {
        let mut ubus = steward_ubus::Ubus::connect().ok()?;
        ubus.call("session", method, args.as_object()?).ok()
    }

    /// Reads `session access`'s answer: none when the session doesn't exist (NotFound), and
    /// `{"access": <bool>}` when it does.
    fn access(answer: Option<&serde_json::Map<String, Value>>) -> Access {
        match answer {
            None => Access::Ended,
            Some(a) if a.get("access") == Some(&Value::Bool(true)) => Access::Granted,
            Some(_) => Access::Denied,
        }
    }
}

impl Auth for Rpcd {
    fn login(&self, username: &str, password: &str) -> Option<String> {
        let s = Self::call(
            "login",
            json!({ "username": username, "password": password, "timeout": 3600 }),
        )?;
        s.get("ubus_rpc_session")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn check(&self, token: &str, right: Right) -> Access {
        // rpcd grants a login the access groups its `read` and `write` lists match, as LuCI
        // relies on. Asking keeps the session alive, as any call through rpcd does.
        Self::access(
            Self::call(
                "access",
                json!({ "ubus_rpc_session": token, "scope": "access-group",
                        "object": GROUP, "function": right.to_string() }),
            )
            .as_ref(),
        )
    }

    fn logout(&self, token: &str) {
        let _ = Self::call("destroy", json!({ "ubus_rpc_session": token }));
    }
}

#[derive(Clone)]
struct Api {
    hub: Arc<Hub>,
    auth: Arc<dyn Auth>,
}

const COOKIE: &str = "steward_session";

/// The header a write signed in by the cookie carries, as `X-Steward: 1`.
const WRITE_HEADER: &str = "x-steward";

/// How often an open event stream makes sure its session is still signed in.
const RECHECK: Duration = Duration::from_secs(30);

/// The session token the request presents. rpcd's unauthenticated session, whose id is all
/// zeros, always exists and anyone can name it: it's no token at all.
fn token(headers: &HeaderMap) -> Option<String> {
    presented(headers).filter(|t| !t.bytes().all(|b| b == b'0'))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn presented(headers: &HeaderMap) -> Option<String> {
    if let Some(t) = bearer(headers) {
        return Some(t.trim().to_owned());
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn op_error(e: OpError) -> Response {
    let status = match e {
        OpError::NotFound(_) => StatusCode::NOT_FOUND,
        OpError::Conflict(_) => StatusCode::CONFLICT,
        OpError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, e.to_string())
}

/// Asks rpcd, off the runtime (ubus blocks), whether the session has `right`.
async fn check(api: &Api, token: String, right: Right) -> Access {
    let auth = api.auth.clone();
    tokio::task::spawn_blocking(move || auth.check(&token, right))
        .await
        .unwrap_or(Access::Ended)
}

fn refused(access: Access, right: Right) -> Response {
    match access {
        Access::Denied => error(
            StatusCode::FORBIDDEN,
            format!("this account has no {right} access to Steward (rpcd access group {GROUP})"),
        ),
        _ => error(
            StatusCode::UNAUTHORIZED,
            "the session has ended: sign in again",
        ),
    }
}

/// Anything but GET and HEAD changes something.
fn is_write(method: &Method) -> bool {
    method != Method::GET && method != Method::HEAD
}

/// What shows that a request comes from another origin, if anything does: the browser's
/// `Sec-Fetch-Site` other than `same-origin` (LuCI on the same host is `same-site`), or an
/// `Origin` other than this API's own, `https://` and the `Host` the request names.
fn foreign(headers: &HeaderMap) -> Option<&'static str> {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|s| s != "same-origin")
    {
        return Some("Sec-Fetch-Site");
    }
    let origin = headers.get(header::ORIGIN)?;
    let own = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(|h| format!("https://{}", h.strip_suffix(":443").unwrap_or(h)));
    let same = own.is_some_and(|own| origin.to_str().is_ok_and(|o| o.eq_ignore_ascii_case(&own)));
    (!same).then_some("Origin")
}

/// A write from another origin is refused, whatever it presents.
async fn same_origin(req: Request, next: Next) -> Response {
    if is_write(req.method())
        && let Some(by) = foreign(req.headers())
    {
        return error(
            StatusCode::FORBIDDEN,
            format!("a change from another origin is refused ({by})"),
        );
    }
    next.run(req).await
}

/// Whether a write lacks `X-Steward: 1` while it's signed in by the cookie, which the browser
/// adds on its own. No other origin can add the header without a CORS preflight, which the
/// API never grants. A bearer token is never added by the browser, so it needs no header.
fn unasked(headers: &HeaderMap) -> bool {
    bearer(headers).is_none() && headers.get(WRITE_HEADER).is_none_or(|v| v != "1")
}

/// How long a request's body has to arrive, from its headers: as long as they have.
const BODY_TIMEOUT: Duration = HANDSHAKE_TIMEOUT;
/// The largest sign-in body: a username and a password. A configuration can be as large as
/// axum's default limit (2 MB), but only a signed-in account's is read.
const LOGIN_BODY: usize = 4096;

/// A request's body must arrive within [`BODY_TIMEOUT`] of its headers, or the request gets
/// 408 and its connection is closed. Signing in reads one before anyone is signed in, so a
/// body trickled in could otherwise hold one of the [`CONNECTIONS`] for as long as it liked.
async fn body_deadline(req: Request, next: Next) -> Response {
    let deadline = Instant::now() + BODY_TIMEOUT;
    let late = Arc::new(AtomicBool::new(false));
    let flag = late.clone();
    let req = req.map(|body| {
        let chunks = Some(body.into_data_stream());
        Body::from_stream(futures_util::stream::unfold(chunks, move |chunks| {
            let flag = flag.clone();
            async move {
                let mut chunks = chunks?;
                let next = futures_util::StreamExt::next(&mut chunks);
                match tokio::time::timeout_at(deadline, next).await {
                    Ok(Some(chunk)) => Some((chunk, Some(chunks))),
                    Ok(None) => None,
                    Err(_) => {
                        flag.store(true, Ordering::Relaxed);
                        let e = axum::Error::new("the request's body took too long");
                        Some((Err(e), None))
                    }
                }
            }
        }))
    });
    let response = next.run(req).await;
    if !late.load(Ordering::Relaxed) {
        return response;
    }
    let mut r = error(
        StatusCode::REQUEST_TIMEOUT,
        format!(
            "the request's body didn't arrive within {} s",
            BODY_TIMEOUT.as_secs()
        ),
    );
    r.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    r
}

fn unasked_refused() -> Response {
    error(
        StatusCode::FORBIDDEN,
        "a change signed in by the cookie needs the header X-Steward: 1",
    )
}

/// Seeing (GET, the event stream included) needs the group's `read`, anything else `write`.
async fn require_session(State(api): State<Api>, req: Request, next: Next) -> Response {
    let Some(t) = token(req.headers()) else {
        return error(StatusCode::UNAUTHORIZED, "sign in first");
    };
    let right = if is_write(req.method()) {
        Right::Write
    } else {
        Right::Read
    };
    if right == Right::Write && unasked(req.headers()) {
        return unasked_refused();
    }
    match check(&api, t, right).await {
        Access::Granted => next.run(req).await,
        a => refused(a, right),
    }
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login(State(api): State<Api>, Json(l): Json<Login>) -> Response {
    let auth = api.auth.clone();
    let session = tokio::task::spawn_blocking(move || auth.login(&l.username, &l.password))
        .await
        .ok()
        .flatten();
    let Some(t) = session else {
        return error(StatusCode::UNAUTHORIZED, "wrong username or password");
    };
    // An account that may not even see Steward gets no session.
    let access = check(&api, t.clone(), Right::Read).await;
    if access != Access::Granted {
        let auth = api.auth.clone();
        let _ = tokio::task::spawn_blocking(move || auth.logout(&t)).await;
        return refused(access, Right::Read);
    }
    let cookie = format!("{COOKIE}={t}; Path=/api; HttpOnly; Secure; SameSite=Strict");
    let mut r = Json(json!({ "token": t })).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

/// Ends the session the call presents, whatever its account may do.
async fn logout(State(api): State<Api>, headers: HeaderMap) -> Response {
    let Some(t) = token(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "sign in first");
    };
    if unasked(&headers) {
        return unasked_refused();
    }
    let auth = api.auth.clone();
    let _ = tokio::task::spawn_blocking(move || auth.logout(&t)).await;
    let mut r = Json(json!({ "ok": true })).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "steward_session=; Path=/api; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        ),
    );
    r
}

async fn devices(State(api): State<Api>) -> Json<Value> {
    Json(json!({ "devices": api.hub.devices().await }))
}

async fn adopt(State(api): State<Api>, Path(serial): Path<String>) -> Response {
    match api.hub.adopt(&serial).await {
        Ok(m) => Json(json!({ "message": m })).into_response(),
        Err(e) => op_error(e),
    }
}

async fn forget(State(api): State<Api>, Path(serial): Path<String>) -> Response {
    match api.hub.forget(&serial).await {
        Ok(m) => Json(json!({ "message": m })).into_response(),
        Err(e) => op_error(e),
    }
}

async fn get_config(State(api): State<Api>, Path(serial): Path<String>) -> Response {
    match api.hub.config(&serial) {
        Ok(Some(c)) => Json(c).into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            format!("no configuration stored for {serial}"),
        ),
        Err(e) => op_error(e),
    }
}

async fn put_config(
    State(api): State<Api>,
    Path(serial): Path<String>,
    Json(config): Json<Value>,
) -> Response {
    match api.hub.set_config(&serial, config).await {
        Ok(uuid) => Json(json!({ "uuid": uuid })).into_response(),
        Err(e) => op_error(e),
    }
}

/// The session is checked again every [`RECHECK`], and before an event when that long has
/// passed: the stream ends once the session has (logout, rpcd's timeout). Checking keeps the
/// session alive, as an open LuCI page does.
async fn events(
    State(api): State<Api>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let session = token(&headers).unwrap_or_default();
    let rx = api.hub.subscribe();
    let start = (rx, api, session, Instant::now());
    let stream =
        futures_util::stream::unfold(start, |(mut rx, api, session, mut checked)| async move {
            loop {
                let event = tokio::select! {
                    // A waiting event goes first; the check below still comes before it.
                    biased;
                    r = rx.recv() => match r {
                        Ok(v) => Some(v),
                        // A slow reader missed some: carry on with what comes next.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = tokio::time::sleep_until(checked + RECHECK) => None,
                };
                if checked.elapsed() >= RECHECK {
                    if check(&api, session.clone(), Right::Read).await != Access::Granted {
                        return None;
                    }
                    checked = Instant::now();
                }
                if let Some(v) = event {
                    let event = Event::default().data(v.to_string());
                    return Some((Ok(event), (rx, api, session, checked)));
                }
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn router(hub: Arc<Hub>, auth: Arc<dyn Auth>) -> Router {
    let api = Api { hub, auth };
    let protected = Router::new()
        .route("/api/devices", get(devices))
        .route("/api/devices/{serial}/adopt", post(adopt))
        .route("/api/devices/{serial}/forget", post(forget))
        .route(
            "/api/devices/{serial}/config",
            get(get_config).put(put_config),
        )
        .route("/api/events", get(events))
        .route_layer(middleware::from_fn_with_state(api.clone(), require_session));
    Router::new()
        .route(
            "/api/login",
            post(login).layer(DefaultBodyLimit::max(LOGIN_BODY)),
        )
        .route("/api/logout", post(logout))
        .merge(protected)
        .layer(middleware::from_fn(same_origin))
        .layer(middleware::from_fn(body_deadline))
        .with_state(api)
}

/// How long a new connection has for its TLS handshake, and a request for its headers (an
/// idle connection is closed after as long), as on the device port.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections served at once, at most. Further ones wait in the listen backlog, so
/// connections that never get anywhere can't take every file descriptor.
const CONNECTIONS: usize = 32;
/// How long to wait after a failed accept (no file descriptors left, say) before the next.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// Serves the API on `listener`, over TLS when there's an acceptor.
pub async fn serve(
    listener: tokio::net::TcpListener,
    acceptor: Option<steward_tls::TlsAcceptor>,
    app: Router,
) {
    let incoming = futures_util::stream::unfold(listener, |l| async move {
        let accepted = l.accept().await;
        Some((accepted, l))
    });
    accept(std::pin::pin!(incoming), acceptor, app).await
}

/// Serves each connection from `incoming`, [`CONNECTIONS`] at a time.
async fn accept<S, I>(
    mut incoming: std::pin::Pin<&mut S>,
    acceptor: Option<steward_tls::TlsAcceptor>,
    app: Router,
) where
    S: futures_util::Stream<Item = std::io::Result<(I, std::net::SocketAddr)>>,
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let slots = Arc::new(tokio::sync::Semaphore::new(CONNECTIONS));
    loop {
        let slot = slots.clone().acquire_owned().await.expect("never closed");
        match futures_util::StreamExt::next(&mut incoming).await {
            Some(Ok((io, addr))) => {
                let (app, acceptor) = (app.clone(), acceptor.clone());
                tokio::spawn(async move {
                    connection(io, addr, acceptor, app).await;
                    drop(slot);
                });
            }
            Some(Err(e)) => {
                crate::log!("api accept: {e}");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
            None => return,
        }
    }
}

/// One connection: the TLS handshake (none with `--plaintext`) within [`HANDSHAKE_TIMEOUT`],
/// then its requests, each with as long for its headers.
async fn connection<I>(
    io: I,
    addr: std::net::SocketAddr,
    acceptor: Option<steward_tls::TlsAcceptor>,
    app: Router,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper_util::service::TowerToHyperService::new(app);
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    let result = match acceptor {
        Some(acceptor) => {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(io)).await {
                Ok(Ok(tls)) => {
                    http.serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                        .await
                }
                Ok(Err(e)) => {
                    crate::log!("api {addr}: TLS handshake: {e}");
                    return;
                }
                Err(_) => {
                    crate::log!("api {addr}: no TLS handshake in time");
                    return;
                }
            }
        }
        None => {
            http.serve_connection(hyper_util::rt::TokioIo::new(io), service)
                .await
        }
    };
    // A connection that goes quiet between requests ends by the header timeout: no news.
    if let Err(e) = result
        && !e.is_incomplete_message()
        && !e.is_timeout()
    {
        crate::log!("api {addr}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::Devices;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tower::ServiceExt;

    const SERIAL: &str = "00005e005301";
    const NULL_SESSION: &str = "00000000000000000000000000000000";

    /// rpcd as the API sees it. root (`t0k3n`) has every group, `viewer` has `read 'steward'`,
    /// `other` only another group. rpcd's unauthenticated session exists here with every right,
    /// more than rpcd grants it, so the API has to refuse it on its own.
    struct Fake {
        /// Live sessions: (read, write).
        sessions: Mutex<HashMap<String, (bool, bool)>>,
        checks: AtomicUsize,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                sessions: Mutex::new(HashMap::from([
                    ("t0k3n".to_string(), (true, true)),
                    (NULL_SESSION.to_string(), (true, true)),
                ])),
                checks: AtomicUsize::new(0),
            }
        }

        fn live(&self, token: &str) -> bool {
            self.sessions.lock().unwrap().contains_key(token)
        }
    }

    impl Auth for Fake {
        fn login(&self, u: &str, p: &str) -> Option<String> {
            let rights = match (u, p) {
                ("root", "right") => (true, true),
                ("viewer", "right") => (true, false),
                ("other", "right") => (false, false),
                _ => return None,
            };
            let t = match u {
                "root" => "t0k3n".to_string(),
                _ => format!("{u}-t0k3n"),
            };
            self.sessions.lock().unwrap().insert(t.clone(), rights);
            Some(t)
        }
        fn check(&self, t: &str, right: Right) -> Access {
            self.checks.fetch_add(1, Ordering::SeqCst);
            match self.sessions.lock().unwrap().get(t) {
                None => Access::Ended,
                Some(&(read, write)) if (right == Right::Read && read) || write => Access::Granted,
                Some(_) => Access::Denied,
            }
        }
        fn logout(&self, t: &str) {
            self.sessions.lock().unwrap().remove(t);
        }
    }

    async fn app(name: &str) -> (Router, Arc<Hub>, std::path::PathBuf) {
        let (app, hub, _, dir) = app_with(name).await;
        (app, hub, dir)
    }

    async fn app_with(name: &str) -> (Router, Arc<Hub>, Arc<Fake>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("steward-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::new(
            Devices::load(&dir.join("devices.json")).unwrap(),
            crate::states::States::load(&dir.join("state")).0,
            dir.join("configs"),
        ));
        // A device that has connected once: pending.
        hub.registry
            .lock()
            .await
            .devices
            .admit(SERIAL, None, Some("E8450"), "OpenWrt", |_| false)
            .unwrap();
        let fake = Arc::new(Fake::new());
        (router(hub.clone(), fake.clone()), hub, fake, dir)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, format!("Bearer {a}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sign_in_is_required_and_uses_the_system_accounts() {
        let (app, _, dir) = app("auth").await;
        let (s, _, _) = call(
            &app,
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": "root", "password": "wrong" })),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, headers, body) = call(
            &app,
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": "root", "password": "right" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["token"], "t0k3n");
        // Only the API's paths get the cookie, not every page on the host (LuCI's).
        let cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(
            cookie.contains("; Path=/api;")
                && cookie.contains("HttpOnly")
                && cookie.contains("Secure")
                && cookie.contains("SameSite=Strict"),
            "{cookie}"
        );

        assert_eq!(
            call(&app, "GET", "/api/devices", None, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, "GET", "/api/devices", Some("forged"), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, "GET", "/api/devices", Some("t0k3n"), None)
                .await
                .0,
            StatusCode::OK
        );
        // The cookie works too (the browser's EventSource can't set a header).
        let req = axum::http::Request::get("/api/devices")
            .header(header::COOKIE, "a=b; steward_session=t0k3n")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn devices_are_listed_adopted_configured_and_forgotten() {
        let (app, _, dir) = app("ops").await;
        let t = Some("t0k3n");
        let (_, _, body) = call(&app, "GET", "/api/devices", t, None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "pending");
        assert_eq!(body["devices"][SERIAL]["connected"], false);
        assert_eq!(body["devices"][SERIAL]["model"], "E8450");

        let (s, _, body) = call(
            &app,
            "POST",
            &format!("/api/devices/{SERIAL}/adopt"),
            t,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("when it next connects")
        );
        assert_eq!(
            call(&app, "POST", "/api/devices/00005e0053ff/adopt", t, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // A configuration gets a uuid, and a newer one a higher uuid.
        let (s, _, body) = call(
            &app,
            "PUT",
            &format!("/api/devices/{SERIAL}/config"),
            t,
            Some(json!({ "radios": [] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let first = body["uuid"].as_u64().unwrap();
        let (_, _, body) = call(
            &app,
            "PUT",
            &format!("/api/devices/{SERIAL}/config"),
            t,
            Some(json!({ "radios": [] })),
        )
        .await;
        assert!(body["uuid"].as_u64().unwrap() > first);
        let (_, _, body) = call(
            &app,
            "GET",
            &format!("/api/devices/{SERIAL}/config"),
            t,
            None,
        )
        .await;
        assert_eq!(body["radios"], json!([]));
        assert!(body["uuid"].as_u64().unwrap() > first);
        // Not a device the controller knows, not a serial, not an object.
        assert_eq!(
            call(
                &app,
                "PUT",
                "/api/devices/00005e0053ff/config",
                t,
                Some(json!({}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, "GET", "/api/devices/..%2F..%2Fetc/config", t, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                &format!("/api/devices/{SERIAL}/config"),
                t,
                Some(json!([1]))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );

        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/devices/{SERIAL}/forget"),
                t,
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, _, body) = call(&app, "GET", "/api/devices", t, None).await;
        assert_eq!(body["devices"], json!({}));
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/devices/{SERIAL}/forget"),
                t,
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn events_stream_what_happens() {
        let (app, hub, dir) = app("events").await;
        let req = axum::http::Request::get("/api/events")
            .header(header::AUTHORIZATION, "Bearer t0k3n")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = res.into_body();
        hub.emit("adopted", SERIAL, Value::Null);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(
            text.starts_with("data: ") && text.contains("\"adopted\"") && text.contains(SERIAL),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn open_events(app: &Router, token: &str) -> (StatusCode, Body) {
        let req = axum::http::Request::get("/api/events")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        (res.status(), res.into_body())
    }

    /// The next event on the stream, past keep-alives; `None` once the stream has ended.
    async fn next_event(body: &mut Body) -> Option<String> {
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap_or_default();
            let text = String::from_utf8(data.to_vec()).unwrap();
            if text.starts_with("data: ") {
                return Some(text);
            }
        }
        None
    }

    /// rpcd keeps an unauthenticated session under the all-zero id, which always exists and
    /// anyone can name: it's refused on every route before rpcd is asked.
    #[tokio::test(flavor = "current_thread")]
    async fn rpcds_unauthenticated_session_is_never_a_sign_in() {
        let (app, _, fake, dir) = app_with("null").await;
        let config = format!("/api/devices/{SERIAL}/config");
        for (method, uri, body) in [
            ("GET", "/api/devices".to_string(), None),
            ("GET", "/api/events".to_string(), None),
            ("GET", config.clone(), None),
            ("PUT", config.clone(), Some(json!({ "radios": [] }))),
            ("POST", format!("/api/devices/{SERIAL}/adopt"), None),
            ("POST", format!("/api/devices/{SERIAL}/forget"), None),
            ("POST", "/api/logout".to_string(), None),
        ] {
            let (s, _, _) = call(&app, method, &uri, Some(NULL_SESSION), body).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0, "rpcd was asked");
        assert!(fake.live(NULL_SESSION), "rpcd was told to end it");
        let (_, _, body) = call(&app, "GET", "/api/devices", Some("t0k3n"), None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "pending");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_unauthenticated_session_is_no_token() {
        let with = |name, value: String| {
            let mut h = HeaderMap::new();
            h.insert(name, HeaderValue::from_str(&value).unwrap());
            h
        };
        let bearer = |t: &str| with(header::AUTHORIZATION, format!("Bearer {t}"));
        assert_eq!(token(&bearer(NULL_SESSION)), None);
        assert_eq!(token(&bearer("")), None);
        let cookie = with(header::COOKIE, format!("a=b; {COOKIE}={NULL_SESSION}"));
        assert_eq!(token(&cookie), None);
        let live = "0123456789abcdef0123456789abcdef";
        assert_eq!(token(&bearer(live)).as_deref(), Some(live));
    }

    /// `session access` as rpcd answers it: NotFound for no session, else `{"access": bool}`.
    #[test]
    fn rpcds_access_answers() {
        let answer = |v: Value| v.as_object().cloned();
        assert_eq!(Rpcd::access(None), Access::Ended);
        let granted = answer(json!({ "access": true }));
        assert_eq!(Rpcd::access(granted.as_ref()), Access::Granted);
        let denied = answer(json!({ "access": false }));
        assert_eq!(Rpcd::access(denied.as_ref()), Access::Denied);
        assert_eq!(Rpcd::access(answer(json!({})).as_ref()), Access::Denied);
    }

    /// A session may do what its account's rights in the `steward` access group allow: `read`
    /// sees, `write` changes. An account with neither isn't signed in at all.
    #[tokio::test(flavor = "current_thread")]
    async fn what_an_account_may_do_is_its_access_group() {
        let (app, _, fake, dir) = app_with("rights").await;
        let login = |u: &str| Some(json!({ "username": u, "password": "right" }));
        let (s, headers, _) = call(&app, "POST", "/api/login", None, login("other")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert!(!fake.live("other-t0k3n"), "its rpcd session was left open");

        let config = format!("/api/devices/{SERIAL}/config");
        let stored = Some(json!({ "radios": [] }));
        let (s, _, _) = call(&app, "PUT", &config, Some("t0k3n"), stored.clone()).await;
        assert_eq!(s, StatusCode::OK);

        let (s, _, body) = call(&app, "POST", "/api/login", None, login("viewer")).await;
        assert_eq!(s, StatusCode::OK);
        let viewer = body["token"].as_str().unwrap().to_owned();
        let v = Some(viewer.as_str());
        let (s, _, body) = call(&app, "GET", "/api/devices", v, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["devices"][SERIAL]["standing"], "pending");
        let (s, _, body) = call(&app, "GET", &config, v, None).await;
        assert_eq!((s, &body["radios"]), (StatusCode::OK, &json!([])));
        assert_eq!(open_events(&app, &viewer).await.0, StatusCode::OK);
        for (method, uri, body) in [
            ("POST", format!("/api/devices/{SERIAL}/adopt"), None),
            ("POST", format!("/api/devices/{SERIAL}/forget"), None),
            ("PUT", config.clone(), stored.clone()),
        ] {
            let (s, _, _) = call(&app, method, &uri, v, body).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        let (_, _, body) = call(&app, "GET", "/api/devices", v, None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "pending");

        // Signing out needs no right.
        let (s, _, _) = call(&app, "POST", "/api/logout", v, None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!fake.live(&viewer));
        let (s, _, _) = call(&app, "GET", "/api/devices", v, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        let adopt = format!("/api/devices/{SERIAL}/adopt");
        let (s, _, _) = call(&app, "POST", &adopt, Some("t0k3n"), None).await;
        assert_eq!(s, StatusCode::OK);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An open event stream checks its session again every RECHECK, and before an event once
    /// that long has passed: after logout it ends instead of streaming on.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn the_event_stream_ends_with_its_session() {
        let (app, hub, fake, dir) = app_with("recheck").await;
        let (s, mut body) = open_events(&app, "t0k3n").await;
        assert_eq!(s, StatusCode::OK);
        hub.emit("adopted", SERIAL, Value::Null);
        assert!(next_event(&mut body).await.unwrap().contains("adopted"));

        let checks = fake.checks.load(Ordering::SeqCst);
        tokio::time::advance(RECHECK + Duration::from_secs(1)).await;
        hub.emit("state", SERIAL, json!({}));
        assert!(next_event(&mut body).await.unwrap().contains("state"));
        assert!(
            fake.checks.load(Ordering::SeqCst) > checks,
            "not checked again"
        );

        // Signed out: a quiet stream ends at its next check, a busy one before its next event.
        let (s, mut quiet) = open_events(&app, "t0k3n").await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = call(&app, "POST", "/api/logout", Some("t0k3n"), None).await;
        assert_eq!(s, StatusCode::OK);
        let signed_out = Instant::now();
        let ended = tokio::time::timeout(RECHECK * 2, next_event(&mut quiet)).await;
        assert_eq!(ended, Ok(None), "streaming on after logout");
        assert!(signed_out.elapsed() <= RECHECK);
        hub.emit("state", SERIAL, json!({ "unit": {} }));
        let ended = tokio::time::timeout(RECHECK * 2, next_event(&mut body)).await;
        assert_eq!(ended, Ok(None), "an event after logout");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The API as a browser reaches it: `https://192.0.2.1:8443`.
    const HOST: &str = "192.0.2.1:8443";

    /// A request as a browser sends it, with the cookie and `headers`.
    async fn browser(
        app: &Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::COOKIE, format!("{COOKIE}=t0k3n"));
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        (res.status(), res.headers().clone())
    }

    /// Cookies aren't port-specific, so other pages on the router (LuCI on 443, the same
    /// site) could send one along: a change from another origin is refused whatever it
    /// presents, and one signed in by the cookie needs `X-Steward: 1`, which no other origin
    /// can add. None of the refusals asks rpcd. Reading by cookie (the event stream) needs
    /// no header, and a bearer token, which the browser never adds by itself, needs none.
    #[tokio::test(flavor = "current_thread")]
    async fn a_change_from_another_origin_is_refused() {
        let (app, _, fake, dir) = app_with("origin").await;
        let adopt = format!("/api/devices/{SERIAL}/adopt");
        let asked = ("x-steward", "1");
        // A form on another site: no JSON, the cookie, a foreign Origin.
        let req = axum::http::Request::post(&adopt)
            .header(header::HOST, HOST)
            .header(header::COOKIE, format!("{COOKIE}=t0k3n"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::from("a=b"))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        for origin in [
            "https://evil.example",
            "https://192.0.2.1",
            "http://192.0.2.1:8443",
            "https://192.0.2.1:8444",
            "null",
        ] {
            let (s, _) = browser(&app, "POST", &adopt, &[asked, ("origin", origin)]).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "Origin: {origin}");
            let (s, _, _) = call_with(&app, "POST", &adopt, &[("origin", origin)]).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "Origin: {origin}, bearer");
        }
        for site in ["cross-site", "same-site", "none"] {
            let fetch = ("sec-fetch-site", site);
            let (s, _) = browser(&app, "POST", &adopt, &[asked, fetch]).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "Sec-Fetch-Site: {site}");
        }
        // Same origin, but without the header (or with another value).
        let own = [
            ("origin", "https://192.0.2.1:8443"),
            ("sec-fetch-site", "same-origin"),
        ];
        let (s, _) = browser(&app, "POST", &adopt, &own).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = browser(&app, "POST", &adopt, &[own[0], own[1], ("x-steward", "0")]).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = browser(&app, "POST", "/api/logout", &own).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(fake.live("t0k3n"), "signed out by a refused logout");
        // No preflight is granted: the header can't be added from elsewhere.
        let preflight = [
            ("origin", "https://evil.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-steward"),
        ];
        let (_, headers) = browser(&app, "OPTIONS", &adopt, &preflight).await;
        assert!(headers.get("access-control-allow-origin").is_none());
        assert!(headers.get("access-control-allow-headers").is_none());
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0, "rpcd was asked");
        let (_, _, body) = call(&app, "GET", "/api/devices", Some("t0k3n"), None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "pending");

        // Reading by cookie, from wherever: no header needed (EventSource can't add one).
        let (s, _) = browser(&app, "GET", "/api/devices", &[]).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = browser(&app, "GET", "/api/events", &[own[1]]).await;
        assert_eq!(s, StatusCode::OK);
        // The page itself: same origin, the cookie and the header.
        let (s, _) = browser(&app, "POST", &adopt, &[own[0], own[1], asked]).await;
        assert_eq!(s, StatusCode::OK);
        let (_, _, body) = call(&app, "GET", "/api/devices", Some("t0k3n"), None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "adopting");
        // A script's bearer token, no Origin, no header.
        let forget = format!("/api/devices/{SERIAL}/forget");
        let (s, _, _) = call(&app, "POST", &forget, Some("t0k3n"), None).await;
        assert_eq!(s, StatusCode::OK);
        // Signing out by cookie needs the header too; the cookie is cleared where it was set.
        let (s, headers) = browser(&app, "POST", "/api/logout", &[own[0], asked]).await;
        assert_eq!(s, StatusCode::OK);
        let cleared = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cleared.contains("; Path=/api;") && cleared.contains("Max-Age=0"));
        assert!(!fake.live("t0k3n"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A bearer-token call with extra headers.
    async fn call_with(
        app: &Router,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, "Bearer t0k3n");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or_default(),
        )
    }

    /// The request's own origin is `https://` and its Host, the default port left out.
    #[test]
    fn a_requests_own_origin() {
        let headers = |host: Option<&str>, origin: &str| {
            let mut h = HeaderMap::new();
            if let Some(host) = host {
                h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            }
            h.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            h
        };
        let own = [
            ("192.0.2.1:8443", "https://192.0.2.1:8443"),
            ("router.lan:8443", "https://Router.LAN:8443"),
            ("router.lan", "https://router.lan"),
            ("router.lan:443", "https://router.lan"),
            ("[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
        ];
        for (host, origin) in own {
            assert_eq!(
                foreign(&headers(Some(host), origin)),
                None,
                "{host} {origin}"
            );
        }
        let other = [
            (Some("router.lan:8443"), "https://router.lan"),
            (
                Some("router.lan:8443"),
                "https://router.lan:8443.evil.example",
            ),
            (Some("router.lan"), "http://router.lan"),
            (None, "https://router.lan"),
        ];
        for (host, origin) in other {
            assert_eq!(foreign(&headers(host, origin)), Some("Origin"), "{origin}");
        }
        assert_eq!(foreign(&HeaderMap::new()), None, "curl sends neither");
    }

    /// One API connection, as `accept` serves it, from `192.0.2.9`.
    fn connect(
        app: &Router,
        acceptor: Option<steward_tls::TlsAcceptor>,
    ) -> (DuplexStream, tokio::task::JoinHandle<()>) {
        let (client, server) = tokio::io::duplex(1 << 16);
        let addr = "192.0.2.9:40000".parse().unwrap();
        let task = tokio::spawn(connection(server, addr, acceptor, app.clone()));
        (client, task)
    }

    const REQUEST: &[u8] = b"GET /api/devices HTTP/1.1\r\nHost: 192.0.2.1:8443\r\n\r\n";

    /// What arrives until the server closes the connection (an hour at most), and when.
    async fn until_closed(client: &mut (impl AsyncRead + Unpin)) -> (String, Duration) {
        let start = Instant::now();
        let mut got = Vec::new();
        let hour = Duration::from_secs(3600);
        let _ = tokio::time::timeout(hour, client.read_to_end(&mut got)).await;
        (String::from_utf8_lossy(&got).into_owned(), start.elapsed())
    }

    /// A connection that doesn't finish its TLS handshake, or a request's headers, within
    /// HANDSHAKE_TIMEOUT is closed, as on the device port; an idle one after as long.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn quiet_connections_are_closed() {
        let (app, _, dir) = app("quiet").await;
        let limit = HANDSHAKE_TIMEOUT..HANDSHAKE_TIMEOUT + Duration::from_secs(1);
        let id = steward_tls::ControllerIdentity::load_or_create(&dir.join("tls")).unwrap();
        let tls = steward_tls::TlsAcceptor::from(id.server_config().unwrap());
        let (mut silent, task) = connect(&app, Some(tls));
        let (got, after) = until_closed(&mut silent).await;
        assert!(got.is_empty() && limit.contains(&after), "TLS: {after:?}");
        assert!(task.is_finished());

        let (mut slow, _) = connect(&app, None);
        slow.write_all(b"GET /api/devices HTTP/1.1\r\nHost: 192.0.2.1:8443\r\n")
            .await
            .unwrap();
        let (got, after) = until_closed(&mut slow).await;
        assert!(
            !got.contains("HTTP/1.1 2") && limit.contains(&after),
            "{after:?}"
        );

        // A whole request is answered at once, and the idle connection closed later.
        let (mut client, _) = connect(&app, None);
        client.write_all(REQUEST).await.unwrap();
        let mut answer = [0; 64];
        let start = Instant::now();
        let n = client.read(&mut answer).await.unwrap();
        assert!(answer[..n].starts_with(b"HTTP/1.1 401"));
        assert!(start.elapsed() < Duration::from_secs(1));
        let (_, after) = until_closed(&mut client).await;
        assert!(limit.contains(&after), "idle: {after:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A request whose body is sent one byte every 2 s after `head`: the connection's reading
    /// end.
    fn trickle(app: &Router, head: String) -> tokio::io::ReadHalf<DuplexStream> {
        let (client, _) = connect(app, None);
        let (reader, mut writer) = tokio::io::split(client);
        tokio::spawn(async move {
            writer.write_all(head.as_bytes()).await.unwrap();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if writer.write_all(b" ").await.is_err() {
                    return;
                }
            }
        });
        reader
    }

    /// A request's body has BODY_TIMEOUT from its headers, however it trickles in: 408, and the
    /// connection is closed. Signing in, which reads a body before anyone is signed in, takes
    /// at most LOGIN_BODY bytes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_request_body_has_a_time_and_size_limit() {
        let (app, _, dir) = app("body").await;
        let limit = BODY_TIMEOUT..BODY_TIMEOUT + Duration::from_secs(1);
        let post = |request: &str, auth: &str, length: usize, body: &str| {
            format!(
                "{request} HTTP/1.1\r\nHost: {HOST}\r\n{auth}Content-Type: application/json\r\n\
                 Content-Length: {length}\r\n\r\n{body}"
            )
        };
        let login = post("POST /api/login", "", 1000, r#"{"username":"root","#);
        let config = format!("PUT /api/devices/{SERIAL}/config");
        let bearer = "Authorization: Bearer t0k3n\r\n";
        let put = post(&config, bearer, 1000, r#"{"radios":"#);
        for head in [login, put] {
            let mut slow = trickle(&app, head);
            let (got, after) = until_closed(&mut slow).await;
            assert!(got.starts_with("HTTP/1.1 408"), "{got}");
            assert!(limit.contains(&after), "{after:?}");
        }

        // Too large a sign-in is refused at once; one that arrives in time works.
        let big = format!(
            r#"{{"username":"root","password":"{}"}}"#,
            "x".repeat(LOGIN_BODY)
        );
        let (mut client, _) = connect(&app, None);
        let head = post("POST /api/login", "", big.len(), &big);
        client.write_all(head.as_bytes()).await.unwrap();
        let mut answer = [0; 64];
        let n = client.read(&mut answer).await.unwrap();
        assert!(answer[..n].starts_with(b"HTTP/1.1 413"));
        let body = r#"{"username":"root","password":"right"}"#;
        let (mut client, _) = connect(&app, None);
        let head = post("POST /api/login", "", body.len(), "");
        client.write_all(head.as_bytes()).await.unwrap();
        tokio::time::sleep(BODY_TIMEOUT - Duration::from_secs(1)).await;
        client.write_all(body.as_bytes()).await.unwrap();
        let n = client.read(&mut answer).await.unwrap();
        assert!(answer[..n].starts_with(b"HTTP/1.1 200"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// At most CONNECTIONS are served at once: the next waits (in the listen backlog) until
    /// one ends.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn connections_are_served_a_bounded_number_at_a_time() {
        let (app, _, dir) = app("slots").await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(CONNECTIONS * 2);
        let incoming = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
        tokio::spawn(async move { accept(std::pin::pin!(incoming), None, app).await });
        let addr: std::net::SocketAddr = "192.0.2.9:40000".parse().unwrap();
        let mut idle = vec![];
        for _ in 0..CONNECTIONS {
            let (client, server) = tokio::io::duplex(1 << 16);
            tx.send(Ok((server, addr))).await.unwrap();
            idle.push(client);
        }
        let (mut next, server) = tokio::io::duplex(1 << 16);
        tx.send(Ok((server, addr))).await.unwrap();
        next.write_all(REQUEST).await.unwrap();
        let mut answer = [0; 64];
        let wait = Duration::from_secs(5);
        let early = tokio::time::timeout(wait, next.read(&mut answer)).await;
        assert!(early.is_err(), "served beyond the limit");
        // One of them hangs up: the waiting one is served.
        drop(idle.pop());
        let n = tokio::time::timeout(Duration::from_secs(1), next.read(&mut answer))
            .await
            .expect("still waiting")
            .unwrap();
        assert!(answer[..n].starts_with(b"HTTP/1.1 401"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A failed accept (no file descriptors left) waits ACCEPT_RETRY before the next, instead
    /// of spinning and logging as fast as it can.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_failed_accept_waits_before_the_next() {
        let (app, _, dir) = app("accept").await;
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        // Errors for ever; a hundred ends it, so a loop that doesn't wait fails, not hangs.
        let incoming = futures_util::stream::poll_fn(move |_| {
            if counted.fetch_add(1, Ordering::SeqCst) == 100 {
                return std::task::Poll::Ready(None);
            }
            let e = std::io::Error::other("too many open files");
            std::task::Poll::Ready(Some(Err::<(DuplexStream, std::net::SocketAddr), _>(e)))
        });
        let incoming = std::pin::pin!(incoming);
        let _ = tokio::time::timeout(ACCEPT_RETRY * 5, accept(incoming, None, app)).await;
        let tries = tries.load(Ordering::SeqCst);
        assert!((5..=6).contains(&tries), "{tries} accepts in 5 s");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The credential's hash stays in devices.json: the device list, the API's and the
    /// control socket's (the same hub operation), leaves it out.
    #[tokio::test(flavor = "current_thread")]
    async fn the_device_list_carries_no_credential_hash() {
        let (app, hub, dir) = app("hash").await;
        {
            let mut reg = hub.registry.lock().await;
            reg.devices.adopt(SERIAL).unwrap();
            reg.devices.issue(SERIAL).unwrap().unwrap();
            reg.devices.delivered(SERIAL, false).unwrap();
        }
        let stored = std::fs::read_to_string(dir.join("devices.json")).unwrap();
        assert!(stored.contains("credential_sha256"));
        let (_, _, body) = call(&app, "GET", "/api/devices", Some("t0k3n"), None).await;
        assert_eq!(body["devices"][SERIAL]["standing"], "adopted");
        assert!(!body.to_string().contains("credential_sha256"), "{body}");
        assert!(!hub.devices().await.to_string().contains("credential"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
