//! Session supervisor: owns the virtual monitor and the capture child.
//!
//! The monitor and ffmpeg only exist while a client is connected (plus a short idle grace period).
//! A client's requested size drives the monitor size; a size change restarts capture on the new rect.

use crate::capture::{self, CaptureOpts, Region};
use crate::display::{DisplayOpts, Guard, Placement, Rect, VirtualDisplay, clamp_size};
use crate::mutter::Cast;
use crate::server::{self, AppState};
use crate::settings::{Overrides, Patch, Profile, Settings};
use anyhow::{Context, Result};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    process::Child,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::{JoinHandle, block_in_place},
    time::Instant,
};

#[derive(Debug)]
pub enum Ctl {
    /// Client asked for a stream of this many device pixels.
    Size {
        w: u32,
        h: u32,
    },
    /// Client changed resolution, UI scale, profile or placement.
    Settings(Patch),
    Connected,
    Disconnected,
    /// The pump of capture generation `.0` ended (ffmpeg died or closed its pipe).
    CaptureEnded(u64),
}

/// How the virtual monitor is made.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Backend {
    /// RandR on a spare output (X11 sessions).
    X11,
    /// Mutter's ScreenCast virtual monitor (GNOME Wayland sessions).
    Wayland,
}

pub struct SessionOpts {
    pub backend: Backend,
    pub display: DisplayOpts,
    /// Template; the region is filled in from the virtual monitor.
    pub capture: CaptureOpts,
    /// Ignore client sizes and always use this one.
    pub fixed_size: Option<(u32, u32)>,
    /// What clients get until they ask for something else.
    pub settings: Settings,
    /// Explicit fps / bitrate / gop flags; they beat the profile.
    pub overrides: Overrides,
    /// Keep the monitor this long after the last client left.
    pub idle: Duration,
}

struct Running {
    child: Child,
    cast: Option<Arc<Cast>>,
    pump: JoinHandle<()>,
    epoch: u64,
    size: (u32, u32),
    profile: Profile,
    /// UI scale and side last applied to the virtual monitor.
    arranged: (u32, Placement),
    started: Instant,
}

const RETRY: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A capture that lives this long counts as healthy: the backoff starts over.
const STABLE: Duration = Duration::from_secs(10);

/// Exponential delay before the `failures`-th consecutive restart (1-based).
fn backoff(failures: u32) -> Duration {
    (RETRY * 2u32.pow(failures.clamp(1, 6) - 1)).min(RETRY_MAX)
}
/// A new size must stay unchanged this long before the monitor is reconfigured...
const SIZE_SETTLE: Duration = Duration::from_millis(1500);
/// ...and monitor reconfigurations of a live session are at least this far apart. Every one makes the
/// shell reshuffle the screen (and the GPU reallocate its framebuffer), so churn is expensive and risky.
const MIN_RECONFIG_GAP: Duration = Duration::from_secs(5);

pub async fn run(
    opts: SessionOpts,
    st: Arc<AppState>,
    tx: UnboundedSender<Ctl>,
    mut rx: UnboundedReceiver<Ctl>,
) -> Result<()> {
    let mut vd = match opts.backend {
        Backend::X11 => Some(
            block_in_place(|| VirtualDisplay::connect(opts.display.clone()))
                .context("virtual display")?,
        ),
        Backend::Wayland => None,
    };
    let mut want = opts.fixed_size.map(|(w, h)| clamp_size(w, h, 1.0));
    let mut settings = opts.settings;
    let mut viewport: Option<(u32, u32)> = None;
    let mut clients = 0usize;
    let mut running: Option<Running> = None;
    let mut epoch = 0u64;
    let mut idle_at: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;
    let mut failures = 0u32;
    let mut apply_at: Option<Instant> = None;
    let mut last_reconfig = Instant::now()
        .checked_sub(MIN_RECONFIG_GAP)
        .unwrap_or_else(Instant::now);
    let mut guard_tick = tokio::time::interval(Duration::from_secs(1));
    let far = Duration::from_secs(86_400 * 365);

    loop {
        let deadline = [idle_at, retry_at, apply_at].into_iter().flatten().min();
        tokio::select! {
            m = rx.recv() => match m {
                None => break,
                Some(Ctl::Connected) => { clients += 1; idle_at = None; }
                Some(Ctl::Disconnected) => {
                    clients = clients.saturating_sub(1);
                    if clients == 0 { idle_at = Some(Instant::now() + opts.idle); }
                }
                Some(Ctl::Size { w, h }) => {
                    viewport = Some((w, h));
                    if opts.fixed_size.is_none() {
                        let new = clamp_size(w, h, settings.res);
                        if want != Some(new) {
                            want = Some(new);
                            // First start is immediate; changes to a live monitor are debounced and rate-limited.
                            apply_at = running.is_some().then(|| (Instant::now() + SIZE_SETTLE).max(last_reconfig + MIN_RECONFIG_GAP));
                        }
                    }
                }
                Some(Ctl::Settings(p)) => {
                    let new = settings.with(p);
                    if new != settings {
                        tracing::info!(?new, "settings changed");
                        settings = new;
                        let size = viewport.filter(|_| opts.fixed_size.is_none()).map(|(w, h)| clamp_size(w, h, settings.res));
                        if size.is_some() && want != size {
                            want = size;
                        }
                        // A resolution or profile change restarts the capture: same debounce as a resize.
                        let restart = running.as_ref().is_some_and(|r| Some(r.size) != want || r.profile != settings.profile);
                        if restart {
                            apply_at = Some((Instant::now() + Duration::from_millis(300)).max(last_reconfig + MIN_RECONFIG_GAP));
                        }
                    }
                }
                Some(Ctl::CaptureEnded(g)) => {
                    if let Some(lived) = running.as_ref().filter(|r| r.epoch == g).map(|r| r.started.elapsed()) {
                        // a capture that dies right after start is a failure, not a fresh beginning
                        failures = if lived < STABLE { failures + 1 } else { 0 };
                        let wait = backoff(failures);
                        tracing::warn!(?lived, retry_in = ?wait, "capture ended unexpectedly");
                        stop_capture(&mut running, &st).await;
                        retry_at = Some(Instant::now() + wait);
                    }
                }
            },
            _ = guard_tick.tick(), if running.is_some() && vd.is_some() => {
                if block_in_place(|| vd.as_ref().map(|v| v.guard_reference())) == Some(Guard::Lost) {
                    tracing::error!("laptop display could not be restored, removing the virtual monitor");
                    stop_capture(&mut running, &st).await;
                    block_in_place(|| vd.as_mut().map(|v| v.down()));
                    retry_at = Some(Instant::now() + Duration::from_secs(30));
                }
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(|| Instant::now() + far)), if deadline.is_some() => {
                let now = Instant::now();
                if idle_at.is_some_and(|t| t <= now) {
                    idle_at = None;
                    tracing::info!("no clients, tearing down virtual monitor");
                    stop_capture(&mut running, &st).await;
                    block_in_place(|| vd.as_mut().map(|v| v.down()));
                }
                if retry_at.is_some_and(|t| t <= now) { retry_at = None; }
                if apply_at.is_some_and(|t| t <= now) { apply_at = None; }
            }
        }

        // Converge on: capture running at `want` iff a client is connected.
        if clients > 0 {
            if let Some((w, h)) = want {
                let ok = running
                    .as_ref()
                    .is_some_and(|r| r.size == (w, h) && r.profile == settings.profile);
                if !ok && retry_at.is_none() && apply_at.is_none() {
                    stop_capture(&mut running, &st).await;
                    epoch += 1;
                    last_reconfig = Instant::now();
                    match start(
                        &opts,
                        &mut vd,
                        &st,
                        &tx,
                        Launch {
                            w,
                            h,
                            epoch,
                            settings,
                        },
                    )
                    .await
                    {
                        Ok(r) => running = Some(r),
                        Err(e) => {
                            // Back off exponentially: every attempt may touch the real display.
                            failures += 1;
                            let wait = backoff(failures);
                            tracing::error!(error = %format!("{e:#}"), attempt = failures, retry_in = ?wait, "cannot start capture");
                            retry_at = Some(Instant::now() + wait);
                        }
                    }
                } else if let (true, Some(r)) = (apply_at.is_none(), running.as_mut()) {
                    // Scale and side change on the live monitor; the stream itself keeps running.
                    if r.arranged != (settings.ui, settings.place) {
                        r.arranged = (settings.ui, settings.place);
                        if let Some(cast) = &r.cast {
                            match cast
                                .arrange(settings.ui, settings.place, opts.display.offset)
                                .await
                            {
                                Ok(scale) => {
                                    tracing::info!(scale, place = ?settings.place, "virtual monitor arranged")
                                }
                                Err(e) => {
                                    tracing::warn!(error = %format!("{e:#}"), "cannot arrange the virtual monitor")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    stop_capture(&mut running, &st).await;
    Ok(())
}

/// What to start: stream size, capture generation and the client's settings.
struct Launch {
    w: u32,
    h: u32,
    epoch: u64,
    settings: Settings,
}

async fn start(
    opts: &SessionOpts,
    vd: &mut Option<VirtualDisplay>,
    st: &Arc<AppState>,
    tx: &UnboundedSender<Ctl>,
    want: Launch,
) -> Result<Running> {
    let Launch {
        w,
        h,
        epoch,
        settings,
    } = want;
    let mut cap = opts.capture.clone();
    opts.overrides.tune(settings.profile, &mut cap);
    let (rect, cast, mut child) = match vd {
        Some(vd) => {
            let rect = block_in_place(|| vd.up(w, h))?;
            cap.region = Region {
                w: rect.w,
                h: rect.h,
                x: rect.x,
                y: rect.y,
            };
            (rect, None, capture::spawn(&cap)?)
        }
        None => {
            let cast = Arc::new(Cast::start().await?);
            let child = match capture::spawn_gst(cast.node, w, h, &cap) {
                Ok(c) => c,
                Err(e) => {
                    cast.stop().await;
                    return Err(e);
                }
            };
            // Input is addressed in stream pixels, so the "monitor" is the stream itself.
            st.width.store(w, Ordering::Relaxed);
            st.height.store(h, Ordering::Relaxed);
            *st.wl.write().unwrap() = Some(cast.clone());
            // The monitor appears once the capture child connects; then scale and place it.
            match cast.wait_monitor(w, h, Duration::from_secs(4)).await {
                Ok(()) => match cast
                    .arrange(settings.ui, settings.place, opts.display.offset)
                    .await
                {
                    Ok(scale) => {
                        tracing::info!(scale, place = ?settings.place, "virtual monitor arranged")
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "cannot arrange the virtual monitor")
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "virtual monitor not visible, leaving it where mutter put it")
                }
            }
            (Rect { x: 0, y: 0, w, h }, Some(cast), child)
        }
    };
    st.width.store(rect.w, Ordering::Relaxed);
    st.height.store(rect.h, Ordering::Relaxed);
    *st.rect.write().unwrap() = Some(rect);
    let stdout = child.stdout.take().context("capture stdout")?;
    let (pst, ptx) = (st.clone(), tx.clone());
    let pump = tokio::spawn(async move {
        if let Err(e) = server::pump(stdout, pst).await {
            tracing::warn!(error = %e, "pump error");
        }
        let _ = ptx.send(Ctl::CaptureEnded(epoch));
    });
    tracing::info!(?rect, epoch, "capture started");
    Ok(Running {
        child,
        cast,
        pump,
        epoch,
        size: (w, h),
        profile: settings.profile,
        arranged: (settings.ui, settings.place),
        started: Instant::now(),
    })
}

async fn stop_capture(running: &mut Option<Running>, st: &AppState) {
    if let Some(mut r) = running.take() {
        r.pump.abort();
        let _ = r.child.kill().await;
        if let Some(c) = r.cast.take() {
            *st.wl.write().unwrap() = None;
            c.stop().await;
        }
        *st.config.write().unwrap() = None;
        *st.rect.write().unwrap() = None;
        tracing::info!(epoch = r.epoch, "capture stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(1), RETRY);
        assert_eq!(backoff(2), RETRY * 2);
        assert_eq!(backoff(3), RETRY * 4);
        assert_eq!(backoff(50), RETRY_MAX);
        assert_eq!(backoff(0), RETRY, "never underflows");
    }
}
