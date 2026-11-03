use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, Subcommand};
use padscreen::capture::{self, CaptureOpts, Encoder, Region, Source};
use padscreen::display::{DisplayOpts, Placement};
use padscreen::settings::{Overrides, Profile, Settings};
use padscreen::{net, server, session};
use std::{net::SocketAddr, path::PathBuf, sync::atomic::Ordering, time::Duration};

#[derive(Parser)]
#[command(name = "padscreen", about = "Use an iPad as a secondary screen")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Capture a screen region, encode it and serve it to the iPad client.
    Serve(ServeArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    /// How to make the virtual monitor (default: from $XDG_SESSION_TYPE)
    #[arg(long, value_enum, env = "PADSCREEN_BACKEND")]
    backend: Option<session::Backend>,
    #[arg(long, value_enum, default_value = "x11")]
    source: Source,
    /// X display (default: $DISPLAY)
    #[arg(long)]
    display: Option<String>,
    /// Fixed-region mode: capture WxH[+X+Y] of the existing screen, no virtual monitor.
    #[arg(long)]
    region: Option<Region>,
    /// Where the virtual monitor sits relative to the reference monitor.
    #[arg(long, value_enum, default_value = "right", env = "PADSCREEN_LAYOUT")]
    layout: Placement,
    /// Slide the virtual monitor along the shared edge (px).
    #[arg(
        long,
        default_value_t = 0,
        allow_hyphen_values = true,
        env = "PADSCREEN_OFFSET"
    )]
    offset: i32,
    /// RandR output to use for the virtual monitor (default: a spare one on the same GPU).
    #[arg(long, env = "PADSCREEN_OUTPUT")]
    output: Option<String>,
    /// Monitor to place relative to (default: primary).
    #[arg(long, env = "PADSCREEN_REFERENCE")]
    reference: Option<String>,
    /// Refresh rate of the virtual monitor mode.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240), env = "PADSCREEN_HZ")]
    hz: u32,
    /// Default fraction of the client's device pixels that is streamed (clients can change it).
    #[arg(long, default_value_t = 1.0, alias = "scale", env = "PADSCREEN_RES")]
    res: f32,
    /// Default UI scale of the virtual monitor, 1 or 2 (Wayland; clients can change it). 2 suits a 264 ppi iPad.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=2), env = "PADSCREEN_UI_SCALE")]
    ui_scale: u32,
    /// Default fps / latency / quality trade-off (clients can change it).
    #[arg(
        long,
        value_enum,
        default_value = "balanced",
        env = "PADSCREEN_PROFILE"
    )]
    profile: Profile,
    /// Always use this size (WxH) instead of the client's.
    #[arg(long, env = "PADSCREEN_SIZE")]
    size: Option<Region>,
    /// Do not inject pointer or touch input from the client (view-only).
    #[arg(long, env = "PADSCREEN_NO_INPUT")]
    no_input: bool,
    /// Serve any network peer, not just loopback and the USB link. Anyone who can reach the port can then
    /// view this PC's iPad screen and control the PC.
    #[arg(long, env = "PADSCREEN_ALLOW_LAN")]
    allow_lan: bool,
    /// Seconds to keep the virtual monitor after the last client leaves.
    #[arg(long, default_value_t = 60, env = "PADSCREEN_IDLE_TIMEOUT")]
    idle_timeout: u64,
    /// Frame rate; overrides the profile's.
    #[arg(long, env = "PADSCREEN_FPS")]
    fps: Option<u32>,
    /// Bitrate in kbit/s; overrides the profile's.
    #[arg(long, env = "PADSCREEN_BITRATE_KBPS")]
    bitrate_kbps: Option<u32>,
    /// Keyframe interval in frames (default: one second)
    #[arg(long, env = "PADSCREEN_GOP")]
    gop: Option<u32>,
    #[arg(long, value_enum, default_value = "auto", env = "PADSCREEN_ENCODER")]
    encoder: Encoder,
    /// Address to listen on. Peers other than loopback and the USB link are refused unless --allow-lan.
    #[arg(long, default_value = "0.0.0.0", env = "PADSCREEN_BIND")]
    bind: String,
    #[arg(long, default_value_t = 8443, env = "PADSCREEN_PORT")]
    port: u16,
    /// TLS certificate chain, PEM (default: <config dir>/leaf.crt, made by scripts/gen-cert.sh)
    #[arg(long, env = "PADSCREEN_CERT")]
    cert: Option<PathBuf>,
    /// TLS private key, PEM (default: <config dir>/leaf.key)
    #[arg(long, env = "PADSCREEN_KEY")]
    key: Option<PathBuf>,
    /// Public CA certificate offered at /ca.crt for installing on the iPad (default: <config dir>/ca.crt if present)
    #[arg(long, env = "PADSCREEN_CA_CERT")]
    ca_cert: Option<PathBuf>,
    /// Plain HTTP/WS (dev and tests only; WebCodecs needs HTTPS on the iPad)
    #[arg(long)]
    no_tls: bool,
}

/// `$XDG_CONFIG_HOME/padscreen` or `~/.config/padscreen`.
fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("padscreen"))
}

/// Settings may live in `./.env` (a source checkout) or `<config dir>/padscreen.env`. Real environment variables win.
fn load_env_files() {
    let _ = dotenvy::dotenv();
    if let Some(p) = config_dir().map(|d| d.join("padscreen.env")) {
        let _ = dotenvy::from_path(p);
    }
}

/// Warn when the TLS private key can be read by other users.
fn warn_if_key_exposed(key: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::metadata(key).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
        tracing::warn!(key = %key.display(), "TLS private key is readable by other users; run chmod 600 on it");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_env_files();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match Cli::parse().cmd {
        Cmd::Serve(a) => serve(a).await,
    }
}

async fn serve(a: ServeArgs) -> Result<()> {
    let mut opts = CaptureOpts {
        source: a.source,
        display: a
            .display
            .or_else(|| std::env::var("DISPLAY").ok())
            .unwrap_or_else(|| ":0".into()),
        region: a.region.clone().unwrap_or(Region {
            w: 1920,
            h: 1080,
            x: 0,
            y: 0,
        }),
        fps: 0,
        bitrate_kbps: 0,
        gop: 0,
        vbv_frames: 0.0,
        encoder: a.encoder,
    };
    let overrides = Overrides {
        fps: a.fps,
        bitrate_kbps: a.bitrate_kbps,
        gop: a.gop,
    };
    overrides.tune(a.profile, &mut opts);
    let defaults = Settings {
        res: a.res.clamp(0.25, 1.0),
        ui: a.ui_scale,
        profile: a.profile,
        place: a.layout,
    };

    // TLS material is resolved before anything is started, so a missing cert fails fast and clearly.
    let cfg_dir = config_dir();
    let in_cfg = |name: &str| cfg_dir.as_ref().map(|d| d.join(name));
    let tls_files = if a.no_tls {
        None
    } else {
        let cert = a
            .cert
            .clone()
            .or_else(|| in_cfg("leaf.crt"))
            .context("no TLS certificate: set PADSCREEN_CERT")?;
        let key = a
            .key
            .clone()
            .or_else(|| in_cfg("leaf.key"))
            .context("no TLS key: set PADSCREEN_KEY")?;
        for f in [&cert, &key] {
            anyhow::ensure!(
                f.is_file(),
                "{} not found; create a certificate with scripts/gen-cert.sh",
                f.display()
            );
        }
        warn_if_key_exposed(&key);
        Some((cert, key))
    };
    let ca_pem = a.ca_cert.clone().or_else(|| in_cfg("ca.crt")).filter(|p| p.is_file()).and_then(|p| std::fs::read(&p).map_err(|e| tracing::warn!(error = %e, path = %p.display(), "cannot read CA certificate")).ok());
    if a.allow_lan {
        tracing::warn!(
            "--allow-lan: any peer that can reach the port may view and control this PC"
        );
    }

    let backend = a.backend.unwrap_or_else(|| {
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
            session::Backend::Wayland
        } else {
            session::Backend::X11
        }
    });
    tracing::info!(?backend, "virtual monitor backend");
    let (in_tx, in_rx) = tokio::sync::mpsc::channel(padscreen::server::INPUT_QUEUE);
    let in_tx = (!a.no_input).then_some(in_tx);

    // Fixed-region mode runs ffmpeg for the whole process; managed mode hands everything to the supervisor.
    let (state, background) = match &a.region {
        Some(r) => {
            let state = server::AppState::new(server::StateOpts {
                width: r.w,
                height: r.h,
                fps: opts.fps,
                ctl: None,
                input: in_tx.clone(),
                allow_lan: a.allow_lan,
                touch: false,
                arrange: false,
                defaults,
                ca_pem,
            });
            *state.rect.write().unwrap() = Some(padscreen::display::Rect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
            });
            let mut child = capture::spawn(&opts)?;
            let stdout = child.stdout.take().context("ffmpeg stdout")?;
            let pump_state = state.clone();
            let task = tokio::spawn(async move {
                let _child = child; // kill_on_drop
                server::pump(stdout, pump_state).await?;
                anyhow::bail!("ffmpeg stream ended")
            });
            (state, task)
        }
        None => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (w, h) = a.size.as_ref().map_or((1920, 1080), |r| (r.w, r.h));
            let wayland = backend == session::Backend::Wayland;
            let state = server::AppState::new(server::StateOpts {
                width: w,
                height: h,
                fps: opts.fps,
                ctl: Some(tx.clone()),
                input: in_tx.clone(),
                allow_lan: a.allow_lan,
                touch: wayland,
                arrange: wayland,
                defaults,
                ca_pem,
            });
            let sopts = session::SessionOpts {
                backend,
                display: DisplayOpts {
                    output: a.output,
                    reference: a.reference,
                    placement: a.layout,
                    offset: a.offset,
                    hz: a.hz,
                },
                capture: opts,
                fixed_size: a.size.as_ref().map(|r| (r.w, r.h)),
                settings: defaults,
                overrides,
                idle: Duration::from_secs(a.idle_timeout),
            };
            let st = state.clone();
            (state, tokio::spawn(session::run(sopts, st, tx, rx)))
        }
    };

    if in_tx.is_some() && backend == session::Backend::Wayland && a.region.is_none() {
        padscreen::input::spawn_wayland(state.clone(), in_rx);
    } else if in_tx.is_some() {
        // keep the thread's JoinHandle alive for the process lifetime; it ends when state (the senders) drops
        match padscreen::input::spawn(state.clone(), in_rx) {
            Ok(h) => std::mem::forget(h),
            Err(e) => {
                tracing::warn!(error = %e, "input injection unavailable, clients are view-only")
            }
        }
    }

    // Tell the user where to point the iPad, and follow the USB link as it changes per plug.
    let (port, tls) = (a.port, !a.no_tls);
    tokio::spawn(async move {
        let mut seen = Vec::new();
        loop {
            let links = net::usb_links();
            if links != seen {
                if links.is_empty() {
                    tracing::info!("no USB network link yet (plug in the iPad)");
                }
                for u in net::urls(&links, port, tls) {
                    tracing::info!("USB link up: open {u} on the iPad");
                }
                seen = links;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // 1 Hz stats line
    let st = state.clone();
    tokio::spawn(async move {
        let (mut pf, mut pb) = (0u64, 0u64);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let (f, b) = (
                st.frames.load(Ordering::Relaxed),
                st.bytes.load(Ordering::Relaxed),
            );
            tracing::info!(
                fps = f - pf,
                kbps = (b - pb) * 8 / 1000,
                clients = st.clients.load(Ordering::Relaxed),
                "stats"
            );
            (pf, pb) = (f, b);
        }
    });

    let addr: SocketAddr = format!("{}:{}", a.bind, a.port)
        .parse()
        .context("bind address")?;
    let app = server::router(state);
    let serving = async {
        let svc = app.into_make_service_with_connect_info::<SocketAddr>();
        match &tls_files {
            None => {
                tracing::info!("listening on http://{addr}");
                axum_server::bind(addr).serve(svc).await
            }
            Some((cert, key)) => {
                let cfg = RustlsConfig::from_pem_file(cert, key)
                    .await
                    .context("load cert/key")?;
                tracing::info!("listening on https://{addr}");
                axum_server::bind_rustls(addr, cfg).serve(svc).await
            }
        }
        .context("server")
    };
    tokio::select! {
        r = serving => r,
        r = background => { r??; anyhow::bail!("background task ended") },
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = sigterm() => Ok(()),
    }
}

async fn sigterm() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}
