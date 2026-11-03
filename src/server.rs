//! WebSocket/HTTP(S) server: fans encoded frames out to connected clients.

use crate::display::Rect;
use crate::h264::{self, AuAssembler, NAL_PPS, NAL_SPS};
use crate::input::{self, InputEvent};
use crate::net;
use crate::session::Ctl;
use crate::settings::{self, Patch, Settings};
use anyhow::Result;
use axum::{
    Router,
    extract::{
        ConnectInfo, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::sync::{
    broadcast,
    mpsc::{Sender, UnboundedSender},
};

const INDEX_HTML: &str = include_str!("../web/index.html");

/// A stalled peer must not hold the virtual monitor open: a send that takes this long ends the connection.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Pending input events; beyond this, moves and wheel ticks are dropped and presses wait.
pub const INPUT_QUEUE: usize = 256;

/// Largest message a client may send: settings and input events are a few hundred bytes.
const MAX_CLIENT_MESSAGE: usize = 16 * 1024;

/// The client is one self-contained page: inline script and style, same-origin WebSocket, canvas output.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; \
                   img-src 'self' data: blob:; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub enum Event {
    /// JSON text message describing the decoder config.
    Config(Arc<String>),
    /// `[flags u8 (bit0 = key)][server ts µs u64 BE][AVCC payload]`
    Frame { key: bool, payload: Bytes },
}

pub struct AppState {
    pub tx: broadcast::Sender<Event>,
    pub config: RwLock<Option<Arc<String>>>,
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub clients: AtomicU64,
    /// Current stream size; the session supervisor changes it when the virtual monitor is resized.
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub fps: u32,
    /// Client lifecycle and size requests for the session supervisor (None in fixed-region mode).
    pub ctl: Option<UnboundedSender<Ctl>>,
    /// Where the streamed picture sits on the X screen; input is mapped onto it.
    pub rect: RwLock<Option<Rect>>,
    /// Pointer injection sink (None = input disabled).
    pub input: Option<Sender<InputEvent>>,
    /// Serve peers beyond loopback and the USB link. Anyone who can reach the port can then view and control this PC.
    pub allow_lan: bool,
    /// USB-link addresses, for deciding who counts as the iPad.
    pub links: net::LinkCache,
    /// The local CA certificate (public part) offered at `/ca.crt` so the iPad can trust the server.
    pub ca_pem: Option<Vec<u8>>,
    /// Live Mutter session (Wayland backend); input goes through it.
    pub wl: RwLock<Option<Arc<crate::mutter::Cast>>>,
    /// Backend can scale and place the virtual monitor (Wayland).
    pub arrange: bool,
    /// Settings a client gets until it asks otherwise; sent to it so its menu can show them.
    pub defaults: Settings,
    /// Backend can inject native multitouch (Wayland); clients then send touch events instead of mouse gestures.
    pub touch: bool,
}

/// Everything `AppState::new` needs.
pub struct StateOpts {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub ctl: Option<UnboundedSender<Ctl>>,
    pub input: Option<Sender<InputEvent>>,
    pub allow_lan: bool,
    pub touch: bool,
    pub arrange: bool,
    pub defaults: Settings,
    pub ca_pem: Option<Vec<u8>>,
}

impl AppState {
    pub fn new(o: StateOpts) -> Arc<Self> {
        let (tx, _) = broadcast::channel(8);
        Arc::new(Self {
            tx,
            config: RwLock::new(None),
            frames: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            clients: AtomicU64::new(0),
            width: AtomicU32::new(o.width),
            height: AtomicU32::new(o.height),
            fps: o.fps,
            ctl: o.ctl,
            rect: RwLock::new(None),
            input: o.input,
            allow_lan: o.allow_lan,
            links: net::LinkCache::new(Duration::from_secs(3)),
            ca_pem: o.ca_pem,
            wl: RwLock::new(None),
            touch: o.touch,
            arrange: o.arrange,
            defaults: o.defaults,
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/ws", get(ws_handler))
        .route("/ca.crt", get(ca_cert))
        .route("/metrics", get(metrics))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Every request: refuse peers outside the USB link (unless `allow_lan`), and add the security headers.
async fn guard(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !st.allow_lan && !st.links.allows(peer.ip()) {
        tracing::warn!(%peer, "refused: not on loopback or the USB link");
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn metrics(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let j = serde_json::json!({
        "frames": st.frames.load(Ordering::Relaxed),
        "bytes": st.bytes.load(Ordering::Relaxed),
        "clients": st.clients.load(Ordering::Relaxed),
    });
    ([("content-type", "application/json")], j.to_string())
}

/// The public CA certificate, for installing as a trusted profile on the iPad.
async fn ca_cert(State(st): State<Arc<AppState>>) -> Response {
    match &st.ca_pem {
        Some(pem) => (
            [(header::CONTENT_TYPE, "application/x-x509-ca-cert")],
            pem.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Browsers attach `Origin` to every WebSocket handshake. A page from another site must not be able to reach
/// this socket (it would drive the mouse), so the origin has to be the page we served: same host and port.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    }; // not a browser
    let (Ok(origin), Some(host)) = (
        origin.to_str(),
        headers.get(header::HOST).and_then(|h| h.to_str().ok()),
    ) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(st): State<Arc<AppState>>,
) -> Response {
    if !same_origin(&headers) {
        tracing::warn!(origin = ?headers.get(header::ORIGIN), "refused cross-origin WebSocket");
        return StatusCode::FORBIDDEN.into_response();
    }
    // Peers were vetted by `guard`; whoever is let in may also drive the pointer unless input is switched off.
    let input_ok = st.input.is_some();
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |s| client(s, st, input_ok))
}

fn ctl(st: &AppState, c: Ctl) {
    if let Some(tx) = &st.ctl {
        let _ = tx.send(c);
    }
}

/// `{"type":"hello"|"resize","pw":..,"ph":..}` -> requested stream size in device pixels.
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if !matches!(v["type"].as_str(), Some("hello" | "resize")) {
        return None;
    }
    let (w, h) = (v["pw"].as_f64()?, v["ph"].as_f64()?);
    (w >= 1.0 && h >= 1.0 && w < 65536.0 && h < 65536.0).then_some((w as u32, h as u32))
}

/// Send one message, giving up on a peer that has stopped reading.
async fn send_msg(sock: &mut WebSocket, m: Message) -> Result<(), String> {
    match tokio::time::timeout(SEND_TIMEOUT, sock.send(m)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no progress for {SEND_TIMEOUT:?}")),
    }
}

/// Queue an input event; moves and wheel ticks are shed when the injector is behind.
async fn forward_input(tx: &Sender<InputEvent>, ev: InputEvent) {
    if ev.droppable() {
        let _ = tx.try_send(ev);
    } else {
        let _ = tx.send(ev).await;
    }
}

async fn client(mut sock: WebSocket, st: Arc<AppState>, input_ok: bool) {
    st.clients.fetch_add(1, Ordering::Relaxed);
    ctl(&st, Ctl::Connected);
    tracing::info!(input_ok, "client connected");
    let caps = serde_json::json!({
        "type": "caps", "input": input_ok, "touch": input_ok && st.touch, "arrange": st.arrange,
        "settings": st.defaults.to_json(), "profiles": settings::profiles_json(),
    })
    .to_string();
    if send_msg(&mut sock, Message::Text(caps.into()))
        .await
        .is_err()
    {
        st.clients.fetch_sub(1, Ordering::Relaxed);
        ctl(&st, Ctl::Disconnected);
        return;
    }
    let mut rx = st.tx.subscribe();
    let cfg = st.config.read().unwrap().clone();
    if let Some(cfg) = cfg {
        if send_msg(&mut sock, Message::Text(cfg.as_str().into()))
            .await
            .is_err()
        {
            st.clients.fetch_sub(1, Ordering::Relaxed);
            ctl(&st, Ctl::Disconnected);
            return;
        }
    }
    // A client may only start decoding at a keyframe.
    let mut need_key = true;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Event::Config(c)) => {
                    need_key = true;
                    if let Err(e) = send_msg(&mut sock, Message::Text(c.as_str().into())).await { tracing::warn!(error = %e, "send config failed"); break; }
                }
                Ok(Event::Frame { key, payload }) => {
                    if need_key && !key { continue; }
                    need_key = false;
                    if let Err(e) = send_msg(&mut sock, Message::Binary(payload)).await { tracing::warn!(error = %e, "send frame failed"); break; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "client lagging, waiting for next keyframe");
                    need_key = true;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = sock.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Some(ev) = input::parse(t.as_str()) {
                        if let (true, Some(tx)) = (input_ok, &st.input) { forward_input(tx, ev).await; }
                        continue;
                    }
                    // untrusted text: escaped and truncated before it reaches the log
                    let shown: String = t.as_str().chars().take(200).collect();
                    tracing::debug!(msg = %shown.escape_debug(), "client message");
                    // settings first: a hello carries both, and the size must be computed with them
                    if let Some(p) = Patch::parse(t.as_str()) { ctl(&st, Ctl::Settings(p)); }
                    if let Some((w, h)) = parse_size(t.as_str()) { ctl(&st, Ctl::Size { w, h }); }
                }
                Some(Ok(Message::Close(f))) => { tracing::info!(frame = ?f, "client closed"); break; }
                None => { tracing::info!("client stream ended"); break; }
                Some(Err(e)) => { tracing::warn!(error = %e, "client recv error"); break; }
                _ => {}
            },
        }
    }
    if let (true, Some(tx)) = (input_ok, &st.input) {
        forward_input(tx, InputEvent::Reset).await;
    }
    st.clients.fetch_sub(1, Ordering::Relaxed);
    ctl(&st, Ctl::Disconnected);
    tracing::info!("client disconnected");
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

/// Read Annex-B from `src`, publish config + frames until EOF.
pub async fn pump(mut src: impl tokio::io::AsyncRead + Unpin, st: Arc<AppState>) -> Result<()> {
    let mut asm = AuAssembler::default();
    let mut buf = vec![0u8; 256 * 1024];
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        for au in asm.push(&buf[..n]) {
            let (new_sps, new_pps) = (au.find(NAL_SPS), au.find(NAL_PPS));
            let changed = new_sps.is_some_and(|s| sps.as_deref() != Some(s))
                || new_pps.is_some_and(|p| pps.as_deref() != Some(p));
            if changed {
                if let Some(s) = new_sps {
                    sps = Some(s.to_vec());
                }
                if let Some(p) = new_pps {
                    pps = Some(p.to_vec());
                }
            }
            if changed {
                if let (Some(s), Some(p)) = (&sps, &pps) {
                    if let (Some(codec), Some(desc)) =
                        (h264::codec_string(s), h264::avcc_description(s, p))
                    {
                        let json = Arc::new(
                            serde_json::json!({
                                "type": "config",
                                "codec": codec,
                                "description": base64::engine::general_purpose::STANDARD.encode(desc),
                                "width": st.width.load(Ordering::Relaxed),
                                "height": st.height.load(Ordering::Relaxed),
                                "fps": st.fps,
                            })
                            .to_string(),
                        );
                        tracing::info!(%codec, "decoder config");
                        *st.config.write().unwrap() = Some(json.clone());
                        let _ = st.tx.send(Event::Config(json));
                    }
                }
            }
            if sps.is_none() {
                continue;
            }
            let data = au.to_avcc();
            if data.is_empty() {
                continue;
            }
            let key = au.is_key();
            let mut payload = Vec::with_capacity(9 + data.len());
            payload.push(key as u8);
            payload.extend_from_slice(&now_us().to_be_bytes());
            payload.extend_from_slice(&data);
            st.frames.fetch_add(1, Ordering::Relaxed);
            st.bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
            let _ = st.tx.send(Event::Frame {
                key,
                payload: payload.into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_size, same_origin};
    use axum::http::{HeaderMap, HeaderValue, header};

    fn hdr(origin: Option<&str>, host: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        if let Some(o) = host {
            h.insert(header::HOST, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    #[test]
    fn websocket_origin_must_match_host() {
        assert!(same_origin(&hdr(
            Some("https://10.42.1.1:8443"),
            Some("10.42.1.1:8443")
        )));
        assert!(same_origin(&hdr(
            Some("http://localhost:8443"),
            Some("LOCALHOST:8443")
        )));
        assert!(
            same_origin(&hdr(None, Some("10.42.1.1:8443"))),
            "non-browser clients send no Origin"
        );
        assert!(!same_origin(&hdr(
            Some("https://evil.example"),
            Some("10.42.1.1:8443")
        )));
        assert!(!same_origin(&hdr(
            Some("https://10.42.1.1:9999"),
            Some("10.42.1.1:8443")
        )));
        assert!(!same_origin(&hdr(Some("null"), Some("10.42.1.1:8443"))));
        assert!(!same_origin(&hdr(Some("https://10.42.1.1:8443"), None)));
    }

    #[test]
    fn size_messages() {
        assert_eq!(
            parse_size(r#"{"type":"hello","pw":2160,"ph":1448.4,"dpr":2}"#),
            Some((2160, 1448))
        );
        assert_eq!(
            parse_size(r#"{"type":"resize","pw":1080,"ph":2160}"#),
            Some((1080, 2160))
        );
        assert_eq!(parse_size(r#"{"type":"diag","pw":1,"ph":1}"#), None);
        assert_eq!(parse_size(r#"{"type":"hello","w":810}"#), None);
        assert_eq!(parse_size(r#"{"type":"hello","pw":0,"ph":5}"#), None);
        assert_eq!(parse_size("nope"), None);
    }
}
