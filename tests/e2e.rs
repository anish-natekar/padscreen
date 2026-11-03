//! Spawns the real binary with the ffmpeg test source, connects as a client over
//! WebSocket, and checks that the frames received decode back to video.

use base64::Engine;
use futures_util::StreamExt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

struct Kill(std::process::Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// avcC record -> Annex-B SPS+PPS
fn avcc_to_annexb_headers(d: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = 5;
    let nsps = (d[p] & 0x1f) as usize;
    p += 1;
    for _ in 0..nsps {
        let l = u16::from_be_bytes([d[p], d[p + 1]]) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&d[p + 2..p + 2 + l]);
        p += 2 + l;
    }
    let npps = d[p] as usize;
    p += 1;
    for _ in 0..npps {
        let l = u16::from_be_bytes([d[p], d[p + 1]]) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&d[p + 2..p + 2 + l]);
        p += 2 + l;
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_decodable_h264_over_websocket() {
    let port = free_port();
    let _srv = Kill(
        Command::new(env!("CARGO_BIN_EXE_padscreen"))
            .args([
                "serve",
                "--source",
                "test",
                "--encoder",
                "x264",
                "--no-tls",
                "--region",
                "640x360",
                "--fps",
                "30",
                "--gop",
                "15",
                "--bind",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn padscreen"),
    );

    let url = format!("ws://127.0.0.1:{port}/ws");
    let mut ws = None;
    for _ in 0..50 {
        if let Ok((s, _)) = connect_async(&url).await {
            ws = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut ws = ws.expect("server did not come up");

    let mut cfg: Option<serde_json::Value> = None;
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut first_key = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while frames.len() < 60 {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .expect("timeout waiting for frames")
            .unwrap()
            .unwrap();
        match msg {
            Message::Text(t) => cfg = Some(serde_json::from_str(t.as_str()).unwrap()),
            Message::Binary(b) => {
                assert!(cfg.is_some(), "frame arrived before config");
                let key = b[0] & 1 == 1;
                if first_key.is_none() {
                    first_key = Some(key);
                }
                frames.push(b[9..].to_vec());
            }
            _ => {}
        }
    }
    let cfg = cfg.unwrap();
    assert_eq!(
        first_key,
        Some(true),
        "first frame delivered must be a keyframe"
    );
    assert_eq!(
        (cfg["width"].as_u64(), cfg["height"].as_u64()),
        (Some(640), Some(360))
    );
    assert!(cfg["codec"].as_str().unwrap().starts_with("avc1."));

    // Rebuild an Annex-B stream and decode it with ffmpeg.
    let desc = base64::engine::general_purpose::STANDARD
        .decode(cfg["description"].as_str().unwrap())
        .unwrap();
    let mut annexb = avcc_to_annexb_headers(&desc);
    for f in &frames {
        let mut p = 0;
        while p < f.len() {
            let l = u32::from_be_bytes(f[p..p + 4].try_into().unwrap()) as usize;
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(&f[p + 4..p + 4 + l]);
            p += 4 + l;
        }
        assert_eq!(
            p,
            f.len(),
            "AVCC length prefixes must tile the payload exactly"
        );
    }
    let mut ff = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-vsync",
            "0",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = ff.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&annexb));
    let out = ff.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(
        out.stderr.is_empty(),
        "ffmpeg decode errors: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let decoded = out.stdout.len() / (640 * 360);
    assert_eq!(decoded, frames.len(), "every received frame must decode");
}
