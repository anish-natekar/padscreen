//! Managed mode on a real X server: client size drives the virtual monitor, resize restarts capture,
//! disconnect tears the monitor down. Flickers your screen briefly.
//! Run manually: `cargo test --test managed_x11 -- --ignored --nocapture`

use futures_util::{SinkExt, StreamExt};
use padscreen::{display::Rect, input::to_abs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use x11rb::{connection::Connection, protocol::xproto};

fn monitors() -> Vec<String> {
    let o = Command::new("xrandr")
        .arg("--listmonitors")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .skip(1)
        .map(|l| l.trim().to_string())
        .collect()
}

fn wait_for(what: &str, secs: u64, ok: impl Fn(&[String]) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if ok(&monitors()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}: {:?}", monitors());
}

fn pointer() -> (i16, i16, bool) {
    let (c, n) = x11rb::connect(None).unwrap();
    let root = c.setup().roots[n].root;
    let q = xproto::query_pointer(&c, root).unwrap().reply().unwrap();
    (
        q.root_x,
        q.root_y,
        u16::from(q.mask) & u16::from(xproto::KeyButMask::BUTTON1) != 0,
    )
}

fn warp(x: i16, y: i16) {
    let (c, n) = x11rb::connect(None).unwrap();
    xproto::warp_pointer(&c, x11rb::NONE, c.setup().roots[n].root, 0, 0, 0, 0, x, y).unwrap();
    c.flush().unwrap();
}

fn wait_pointer(what: &str, ok: impl Fn((i16, i16, bool)) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if ok(pointer()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}: pointer={:?}", pointer());
}

/// Geometry of the 800x600 virtual monitor from `xrandr --listmonitors` ("800/212x600/159+1920+0").
fn monitor_rect() -> Rect {
    let l = monitors()
        .into_iter()
        .find(|m| m.contains(" 800/") || m.contains("+800/"))
        .expect("800x600 monitor");
    let g = l
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_start_matches(['+', '*']);
    let (_, rest) = g.split_once('x').unwrap();
    let mut it = rest.split('+');
    it.next();
    let (x, y) = (
        it.next().unwrap().parse().unwrap(),
        it.next().unwrap().parse().unwrap(),
    );
    Rect {
        x,
        y,
        w: 800,
        h: 600,
    }
}

/// SIGTERM on drop so the daemon runs its cleanup (a plain kill would leave the monitor behind).
struct Term(std::process::Child);
impl Drop for Term {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

/// Read until a config with the wanted size arrives, then `n` frames after it.
async fn expect_stream(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    w: u64,
    h: u64,
    n: usize,
) {
    let mut cfg = false;
    let mut frames = 0;
    let deadline = Instant::now() + Duration::from_secs(20);
    while frames < n {
        assert!(
            Instant::now() < deadline,
            "timeout: cfg={cfg} frames={frames} want {w}x{h}"
        );
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("stream stalled")
            .expect("closed")
            .expect("ws error")
        {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "config" && v["width"] == w && v["height"] == h {
                    cfg = true;
                    frames = 0;
                }
            }
            Message::Binary(_) if cfg => frames += 1,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn size_follows_client_and_idle_teardown() {
    let before = monitors();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _srv = Term(
        Command::new(env!("CARGO_BIN_EXE_padscreen"))
            .args([
                "serve",
                "--encoder",
                "x264",
                "--no-tls",
                "--fps",
                "30",
                "--gop",
                "15",
                "--idle-timeout",
                "2",
                "--bind",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn padscreen"),
    );
    let url = format!("ws://127.0.0.1:{port}/ws");
    let mut conn = None;
    for _ in 0..50 {
        if let Ok((s, _)) = connect_async(&url).await {
            conn = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut ws = conn.expect("server did not come up");
    assert_eq!(
        monitors(),
        before,
        "no monitor before a client asks for one"
    );

    ws.send(Message::Text(
        r#"{"type":"hello","pw":800,"ph":600,"dpr":2}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 800, 600, 15).await;
    wait_for("monitor 800x600", 5, |l| {
        l.iter().any(|m| m.contains("800/") && m.contains("x600/"))
    });

    // input: pointer lands on the virtual monitor, button state follows, then put the pointer back
    let r = monitor_rect();
    let home = pointer();
    let inp =
        |e: &str, x: f32, y: f32| format!(r#"{{"type":"in","e":"{e}","x":{x},"y":{y},"b":1}}"#);
    ws.send(Message::Text(inp("move", 0.25, 0.75).into()))
        .await
        .unwrap();
    let (ex, ey) = to_abs(r, 0.25, 0.75);
    wait_pointer("move", |p| (p.0, p.1) == (ex, ey));
    ws.send(Message::Text(inp("down", 0.5, 0.5).into()))
        .await
        .unwrap();
    let (cx, cy) = to_abs(r, 0.5, 0.5);
    wait_pointer("button down", |p| (p.0, p.1) == (cx, cy) && p.2);
    ws.send(Message::Text(inp("up", 0.5, 0.5).into()))
        .await
        .unwrap();
    wait_pointer("button up", |p| !p.2);
    ws.send(Message::Text(
        r#"{"type":"in","e":"scroll","dx":0,"dy":1.5}"#.into(),
    ))
    .await
    .unwrap();
    warp(home.0, home.1);

    // rotate
    ws.send(Message::Text(
        r#"{"type":"resize","pw":600,"ph":800}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 600, 800, 15).await;
    wait_for("monitor 600x800", 5, |l| {
        l.iter().any(|m| m.contains("600/") && m.contains("x800/"))
    });

    // disconnect -> idle timeout -> monitor gone, layout back
    drop(ws);
    wait_for("teardown", 15, |l| l.len() == before.len());
    assert_eq!(monitors(), before, "layout restored");
}
