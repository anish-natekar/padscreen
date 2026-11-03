//! Managed mode on a GNOME Wayland session: the client's size drives a Mutter virtual monitor, a rotation
//! restarts the stream, and the monitor is gone again after the idle timeout. No CRTC is touched.
//! Run manually inside the Wayland session: `cargo test --test managed_wayland -- --ignored --nocapture`

use futures_util::{SinkExt, StreamExt};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Logical monitors mutter currently has, as "name WxH" for the virtual ones (`Meta-N`).
fn virtual_monitors() -> Vec<String> {
    let o = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Mutter.DisplayConfig",
            "--object-path",
            "/org/gnome/Mutter/DisplayConfig",
            "--method",
            "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout).to_string();
    // current mode of each Meta monitor is the one flagged 'is-current': <true>; report its WxH
    let mut out = Vec::new();
    for part in s.split("('Meta-").skip(1) {
        let name = part.split('\'').next().unwrap_or("?");
        // the name also shows up in the logical-monitor list, which has no modes
        let Some((mode, _)) = part.split_once("'is-current': <true>") else {
            continue;
        };
        let m = mode.rsplit("('").next().unwrap_or("");
        out.push(format!(
            "Meta-{name} {}",
            m.split('\'').next().unwrap_or("")
        ));
    }
    out
}

fn wait_for(what: &str, secs: u64, ok: impl Fn(&[String]) -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if ok(&virtual_monitors()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what}: {:?}", virtual_monitors());
}

struct Term(std::process::Child);
impl Drop for Term {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.id().to_string()])
            .status();
        let _ = self.0.wait();
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Wait for a config of the wanted size (H.264 4:2:0, which Safari decodes), then `n` frames after it.
async fn expect_stream(ws: &mut Ws, w: u64, h: u64, n: usize) {
    let (mut cfg, mut frames, mut key) = (false, 0, false);
    let deadline = Instant::now() + Duration::from_secs(25);
    while frames < n {
        assert!(
            Instant::now() < deadline,
            "timeout: cfg={cfg} frames={frames} want {w}x{h}"
        );
        match tokio::time::timeout(Duration::from_secs(8), ws.next())
            .await
            .expect("stream stalled")
            .expect("closed")
            .expect("ws error")
        {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "config" && v["width"] == w && v["height"] == h {
                    let codec = v["codec"].as_str().unwrap();
                    assert!(
                        ["avc1.42", "avc1.4d", "avc1.64"]
                            .iter()
                            .any(|p| codec.starts_with(p)),
                        "want a 4:2:0 baseline/main/high profile Safari decodes, got {codec}"
                    );
                    cfg = true;
                    frames = 0;
                }
            }
            Message::Binary(b) if cfg => {
                key |= b[0] & 1 == 1;
                frames += 1;
            }
            _ => {}
        }
    }
    assert!(key, "no keyframe seen");
}

async fn run(encoder: &str) {
    assert!(
        virtual_monitors().is_empty(),
        "stale virtual monitor before the test"
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _srv = Term(
        Command::new(env!("CARGO_BIN_EXE_padscreen"))
            .args([
                "serve",
                "--backend",
                "wayland",
                "--encoder",
                encoder,
                "--ui-scale",
                "1",
                "--no-tls",
                "--idle-timeout",
                "2",
                "--bind",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .stdout(Stdio::inherit())
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
    assert!(
        virtual_monitors().is_empty(),
        "no monitor before a client asks for one"
    );

    ws.send(Message::Text(
        r#"{"type":"hello","pw":800,"ph":600,"dpr":2}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 800, 600, 30).await;
    wait_for("virtual monitor 800x600", 5, |m| {
        m.len() == 1 && m[0].contains("800x600")
    });

    // input must not break the stream (pointer motion, click, wheel go to mutter)
    for e in [
        r#"{"type":"in","e":"move","x":0.5,"y":0.5}"#,
        r#"{"type":"in","e":"down","x":0.5,"y":0.5,"b":1}"#,
        r#"{"type":"in","e":"up","x":0.5,"y":0.5,"b":1}"#,
        r#"{"type":"in","e":"scroll","dx":0,"dy":2}"#,
    ] {
        ws.send(Message::Text(e.into())).await.unwrap();
    }

    // native touch: two fingers down, drag, up, and one left down so the disconnect has to lift it
    for e in [
        r#"{"type":"in","e":"td","id":0,"x":0.3,"y":0.3}"#,
        r#"{"type":"in","e":"td","id":1,"x":0.6,"y":0.6}"#,
        r#"{"type":"in","e":"tm","id":0,"x":0.35,"y":0.4}"#,
        r#"{"type":"in","e":"tm","id":1,"x":0.7,"y":0.6}"#,
        r#"{"type":"in","e":"tu","id":0}"#,
        r#"{"type":"in","e":"tu","id":1}"#,
        r#"{"type":"in","e":"td","id":2,"x":0.5,"y":0.5}"#,
    ] {
        ws.send(Message::Text(e.into())).await.unwrap();
    }
    // the stream keeps flowing while fingers are down
    for _ in 0..5 {
        loop {
            if let Message::Binary(_) = tokio::time::timeout(Duration::from_secs(8), ws.next())
                .await
                .expect("stream stalled")
                .expect("closed")
                .expect("ws error")
            {
                break;
            }
        }
    }

    // rotate: stream restarts at the new size
    ws.send(Message::Text(
        r#"{"type":"resize","pw":600,"ph":800}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 600, 800, 30).await;
    wait_for("virtual monitor 600x800", 5, |m| {
        m.len() == 1 && m[0].contains("600x800")
    });

    // last client leaves -> idle teardown removes the monitor
    ws.close(None).await.ok();
    drop(ws);
    wait_for("monitor removed after idle", 15, |m| m.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn wayland_x264() {
    run("x264").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn wayland_nvenc() {
    run("nvenc").await;
}

/// Logical monitors as (connector, x, y, scale) from `GetCurrentState`.
fn logical_monitors() -> Vec<(String, i32, i32, f64)> {
    let o = Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "call",
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.gnome.Mutter.DisplayConfig",
            "GetCurrentState",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    v["data"][2]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l[5][0][0].as_str().unwrap().to_string(),
                l[0].as_i64().unwrap() as i32,
                l[1].as_i64().unwrap() as i32,
                l[2].as_f64().unwrap(),
            )
        })
        .collect()
}

fn wait_layout(what: &str, want: &[(&str, i32, i32, f64)]) {
    let t = Instant::now();
    loop {
        let mut have = logical_monitors();
        have.sort_by(|a, b| a.0.cmp(&b.0));
        if have.len() == want.len()
            && have.iter().zip(want).all(|(h, w)| {
                h.0.starts_with(w.0) && (h.1, h.2) == (w.1, w.2) && (h.3 - w.3).abs() < 1e-6
            })
        {
            return;
        }
        assert!(
            t.elapsed() < Duration::from_secs(8),
            "{what}: have {have:?}, want {want:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// UI scale and the four sides, live and across a profile restart. This one does reconfigure monitors
/// (a handful of `ApplyMonitorsConfig` calls), so it is kept apart from the plain stream tests.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn wayland_layout() {
    // eDP-1 is 1920x1080 here; the iPad monitor is 1600x1200 in layout space whatever its UI scale
    assert_eq!(
        logical_monitors().len(),
        1,
        "stale virtual monitor before the test"
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _srv = Term(
        Command::new(env!("CARGO_BIN_EXE_padscreen"))
            .args([
                "serve",
                "--backend",
                "wayland",
                "--encoder",
                "x264",
                "--no-tls",
                "--idle-timeout",
                "2",
                "--bind",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .stdout(Stdio::inherit())
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

    // the server's defaults reach the client: UI 2x, iPad on the right, balanced
    let caps = loop {
        if let Message::Text(t) = ws.next().await.unwrap().unwrap() {
            let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
            if v["type"] == "caps" {
                break v;
            }
        }
    };
    assert_eq!(caps["arrange"], true);
    assert_eq!(
        caps["settings"],
        serde_json::json!({"res": 1.0, "ui": 2, "profile": "balanced", "place": "right"})
    );
    assert_eq!(caps["profiles"]["latency"]["fps"], 60);

    ws.send(Message::Text(
        r#"{"type":"hello","pw":1600,"ph":1200,"dpr":2}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 1600, 1200, 5).await;
    wait_layout(
        "default: 2x, right",
        &[("Meta-", 1920, 0, 2.0), ("eDP-1", 0, 0, 1.0)],
    );

    for (side, want) in [
        ("left", [("Meta-", 0, 0, 2.0), ("eDP-1", 1600, 0, 1.0)]),
        ("above", [("Meta-", 0, 0, 2.0), ("eDP-1", 0, 1200, 1.0)]),
        ("below", [("Meta-", 0, 1080, 2.0), ("eDP-1", 0, 0, 1.0)]),
        ("right", [("Meta-", 1920, 0, 2.0), ("eDP-1", 0, 0, 1.0)]),
        ("left", [("Meta-", 0, 0, 2.0), ("eDP-1", 1600, 0, 1.0)]),
    ] {
        ws.send(Message::Text(
            format!(r#"{{"type":"settings","place":"{side}"}}"#).into(),
        ))
        .await
        .unwrap();
        wait_layout(side, &want);
    }

    // UI scale back to 1 on the same monitor, no stream restart needed
    ws.send(Message::Text(r#"{"type":"settings","ui":1}"#.into()))
        .await
        .unwrap();
    wait_layout("ui 1", &[("Meta-", 0, 0, 1.0), ("eDP-1", 1600, 0, 1.0)]);
    ws.send(Message::Text(r#"{"type":"settings","ui":2}"#.into()))
        .await
        .unwrap();
    wait_layout("ui 2", &[("Meta-", 0, 0, 2.0), ("eDP-1", 1600, 0, 1.0)]);

    // a profile change restarts the stream (new virtual monitor); side and scale must come back as chosen
    ws.send(Message::Text(
        r#"{"type":"settings","profile":"latency"}"#.into(),
    ))
    .await
    .unwrap();
    expect_stream(&mut ws, 1600, 1200, 10).await;
    wait_layout(
        "after profile restart",
        &[("Meta-", 0, 0, 2.0), ("eDP-1", 1600, 0, 1.0)],
    );

    // leaving removes the monitor and mutter puts the laptop screen back at the origin
    ws.close(None).await.ok();
    drop(ws);
    wait_layout("after idle", &[("eDP-1", 0, 0, 1.0)]);
}
