//! Virtual monitor lifecycle on X11 via RandR: pick a spare disconnected output,
//! give it a custom mode, place it relative to an existing monitor, and undo it all.

use anyhow::{Context, Result, anyhow, bail};
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{
    self, Connection as OutConn, ConnectionExt as _, ModeFlag, ModeInfo, Output, Rotation,
    SetConfig,
};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use std::time::Duration;

fn settle_ms() -> u64 {
    std::env::var("PADSCREEN_SETTLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Guard {
    Ok,
    Restored,
    /// Could not bring the reference monitor back; the caller should tear everything down.
    Lost,
}

/// A CRTC that has no mode or no output is switched off.
fn needs_restore(mode: u32, outputs: &[Output]) -> bool {
    mode == 0 || outputs.is_empty()
}

pub const MODE_PREFIX: &str = "padscreen-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Placement {
    Right,
    Left,
    Above,
    Below,
}

impl Placement {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "right" => Some(Self::Right),
            "left" => Some(Self::Left),
            "above" => Some(Self::Above),
            "below" => Some(Self::Below),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayOpts {
    /// RandR output to use; default: first spare one on the same GPU as the reference monitor.
    pub output: Option<String>,
    /// Monitor to place relative to; default: the primary one.
    pub reference: Option<String>,
    pub placement: Placement,
    /// Slide along the shared edge (px).
    pub offset: i32,
    pub hz: u32,
}

// ---------------------------------------------------------------- pure helpers

/// Simple reduced-blanking-style timing. The output has no real panel, so any sane timing works.
pub fn modeline(w: u32, h: u32, hz: u32) -> ModeInfo {
    let (hfp, hsw, hbp) = (48u32, 32u32, 80u32);
    let (vfp, vsw, vbp) = (3u32, 5u32, 20u32);
    let htotal = w + hfp + hsw + hbp;
    let vtotal = h + vfp + vsw + vbp;
    ModeInfo {
        id: 0,
        width: w as u16,
        height: h as u16,
        dot_clock: htotal * vtotal * hz,
        hsync_start: (w + hfp) as u16,
        hsync_end: (w + hfp + hsw) as u16,
        htotal: htotal as u16,
        hskew: 0,
        vsync_start: (h + vfp) as u16,
        vsync_end: (h + vfp + vsw) as u16,
        vtotal: vtotal as u16,
        name_len: format!("{MODE_PREFIX}{w}x{h}").len() as u16,
        mode_flags: ModeFlag::HSYNC_POSITIVE | ModeFlag::VSYNC_NEGATIVE,
    }
}

/// Where the new monitor's top-left goes relative to `r` (may be negative).
pub fn place(r: Rect, w: u32, h: u32, p: Placement, offset: i32) -> (i32, i32) {
    match p {
        Placement::Right => (r.x + r.w as i32, r.y + offset),
        Placement::Left => (r.x - w as i32, r.y + offset),
        Placement::Above => (r.x + offset, r.y - h as i32),
        Placement::Below => (r.x + offset, r.y + r.h as i32),
    }
}

/// RandR requires non-negative positions. Returns the shift to apply to every rect so the union starts at 0,0.
pub fn origin_shift(rects: &[Rect]) -> (i32, i32) {
    let minx = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let miny = rects.iter().map(|r| r.y).min().unwrap_or(0);
    (-minx.min(0), -miny.min(0))
}

pub fn bounding(rects: &[Rect]) -> (u32, u32) {
    let w = rects.iter().map(|r| r.x + r.w as i32).max().unwrap_or(0);
    let h = rects.iter().map(|r| r.y + r.h as i32).max().unwrap_or(0);
    (w.max(0) as u32, h.max(0) as u32)
}

/// Round a client-reported pixel size to something safe for the encoder and RandR.
pub fn clamp_size(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let f = |v: u32| (((v as f32 * scale) as u32).clamp(320, 4096)) & !1;
    (f(w), f(h))
}

// ---------------------------------------------------------------- X11 side

#[derive(Clone)]
struct SavedCrtc {
    crtc: u32,
    x: i16,
    y: i16,
    mode: u32,
    rotation: Rotation,
    outputs: Vec<Output>,
}

struct Active {
    output: Output,
    crtc: u32,
    mode: u32,
    rect: Rect,
    /// Original configuration of CRTCs we had to move (left/above placement).
    moved: Vec<SavedCrtc>,
    /// What the reference (laptop) monitor's CRTC must keep showing while we are up.
    reference: Option<SavedCrtc>,
}

pub struct VirtualDisplay {
    conn: RustConnection,
    root: Window,
    opts: DisplayOpts,
    active: Option<Active>,
}

impl VirtualDisplay {
    pub fn connect(opts: DisplayOpts) -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("connect to X server ($DISPLAY)")?;
        let root = conn.setup().roots[screen_num].root;
        let v = conn.randr_query_version(1, 5)?.reply()?;
        if (v.major_version, v.minor_version) < (1, 3) {
            bail!(
                "RandR {}.{} too old, need >= 1.3",
                v.major_version,
                v.minor_version
            );
        }
        let mut d = Self {
            conn,
            root,
            opts,
            active: None,
        };
        d.cleanup_stale()?;
        Ok(d)
    }

    pub fn rect(&self) -> Option<Rect> {
        self.active.as_ref().map(|a| a.rect)
    }

    pub fn output_name(&self) -> Option<String> {
        let a = self.active.as_ref()?;
        let res = self.res().ok()?;
        let info = self
            .conn
            .randr_get_output_info(a.output, res.config_timestamp)
            .ok()?
            .reply()
            .ok()?;
        Some(String::from_utf8_lossy(&info.name).into_owned())
    }

    fn res(&self) -> Result<randr::GetScreenResourcesCurrentReply> {
        Ok(self
            .conn
            .randr_get_screen_resources_current(self.root)?
            .reply()?)
    }

    fn mode_names(res: &randr::GetScreenResourcesCurrentReply) -> Vec<(u32, String)> {
        let mut off = 0usize;
        res.modes
            .iter()
            .map(|m| {
                let n = m.name_len as usize;
                let name = String::from_utf8_lossy(res.names.get(off..off + n).unwrap_or(&[]))
                    .into_owned();
                off += n;
                (m.id, name)
            })
            .collect()
    }

    fn crtc_rect(&self, ts: u32, crtc: u32) -> Result<Option<Rect>> {
        let c = self.conn.randr_get_crtc_info(crtc, ts)?.reply()?;
        Ok((c.mode != 0).then_some(Rect {
            x: c.x as i32,
            y: c.y as i32,
            w: c.width as u32,
            h: c.height as u32,
        }))
    }

    fn screen_size(&self) -> Result<(u32, u32)> {
        let g = self.conn.get_geometry(self.root)?.reply()?;
        Ok((g.width as u32, g.height as u32))
    }

    fn set_screen(&self, w: u32, h: u32) -> Result<()> {
        // 96 dpi, as xrandr does when it picks the physical size itself.
        self.conn
            .randr_set_screen_size(self.root, w as u16, h as u16, w * 254 / 960, h * 254 / 960)?
            .check()?;
        Ok(())
    }

    fn set_crtc(
        &self,
        crtc: u32,
        x: i32,
        y: i32,
        mode: u32,
        rotation: Rotation,
        outputs: &[Output],
    ) -> Result<()> {
        let res = self.res()?;
        let r = self
            .conn
            .randr_set_crtc_config(
                crtc,
                CURRENT_TIME,
                res.config_timestamp,
                x as i16,
                y as i16,
                mode,
                rotation,
                outputs,
            )?
            .reply()?;
        if r.status != SetConfig::SUCCESS {
            bail!(
                "set_crtc_config(crtc {crtc}, {x},{y}, mode {mode}) failed: {:?}",
                r.status
            );
        }
        Ok(())
    }

    /// Disable anything left over from a previous (crashed) run.
    fn cleanup_stale(&mut self) -> Result<()> {
        let res = self.res()?;
        let ours: Vec<u32> = Self::mode_names(&res)
            .into_iter()
            .filter(|(_, n)| n.starts_with(MODE_PREFIX))
            .map(|(i, _)| i)
            .collect();
        if ours.is_empty() {
            return Ok(());
        }
        tracing::debug!(?ours, "stale padscreen modes");
        for crtc in &res.crtcs {
            let c = self
                .conn
                .randr_get_crtc_info(*crtc, res.config_timestamp)?
                .reply()?;
            if ours.contains(&c.mode) {
                tracing::warn!(crtc, "removing stale padscreen monitor from a previous run");
                for o in &c.outputs {
                    let _ = self.conn.randr_delete_output_mode(*o, c.mode)?.check();
                }
                self.set_crtc(*crtc, 0, 0, 0, Rotation::ROTATE0, &[])?;
            }
        }
        // A mode still attached to any output cannot be destroyed (and would make the next CreateMode
        // of the same name fail with BadName), so detach it everywhere first.
        for o in &res.outputs {
            let info = self
                .conn
                .randr_get_output_info(*o, res.config_timestamp)?
                .reply()?;
            for m in info.modes.iter().filter(|m| ours.contains(m)) {
                let _ = self.conn.randr_delete_output_mode(*o, *m)?.check();
            }
        }
        for m in ours {
            let _ = self.conn.randr_destroy_mode(m)?.check();
        }
        self.shrink_screen()?;
        Ok(())
    }

    fn shrink_screen(&self) -> Result<()> {
        let res = self.res()?;
        let mut rects = Vec::new();
        for c in &res.crtcs {
            if let Some(r) = self.crtc_rect(res.config_timestamp, *c)? {
                rects.push(r);
            }
        }
        let (w, h) = bounding(&rects);
        if w > 0 && h > 0 && (w, h) != self.screen_size()? {
            self.set_screen(w, h)?;
        }
        Ok(())
    }

    fn output_name_of(&self, ts: u32, o: Output) -> Result<(String, randr::GetOutputInfoReply)> {
        let info = self.conn.randr_get_output_info(o, ts)?.reply()?;
        Ok((String::from_utf8_lossy(&info.name).into_owned(), info))
    }

    /// Make the monitor `w`x`h`. Resizes in place when possible (windows stay on it), else recreates.
    pub fn up(&mut self, w: u32, h: u32) -> Result<Rect> {
        if let Some(a) = &self.active {
            if (a.rect.w, a.rect.h) == (w, h) {
                return Ok(a.rect);
            }
            // The top-left corner stays put only for right/below placement.
            if matches!(self.opts.placement, Placement::Right | Placement::Below) {
                return self.resize_in_place(w, h);
            }
        }
        self.create(w, h)
    }

    fn resize_in_place(&mut self, w: u32, h: u32) -> Result<Rect> {
        let a = self.active.as_ref().expect("active");
        let (out, crtc, old_mode, old_rect) = (a.output, a.crtc, a.mode, a.rect);
        let res = self.res()?;
        let new_rect = Rect { w, h, ..old_rect };
        let mut all: Vec<Rect> = Vec::new();
        for c in &res.crtcs {
            if *c != crtc {
                if let Some(r) = self.crtc_rect(res.config_timestamp, *c)? {
                    all.push(r);
                }
            }
        }
        all.push(new_rect);
        let (bw, bh) = bounding(&all);
        let (cw, ch) = self.screen_size()?;

        let mode = self.install_mode(out, w, h)?;
        let bring_up = || -> Result<()> {
            self.set_screen(bw.max(cw), bh.max(ch))?;
            std::thread::sleep(Duration::from_millis(settle_ms()));
            self.set_crtc(
                crtc,
                new_rect.x,
                new_rect.y,
                mode,
                Rotation::ROTATE0,
                &[out],
            )
        };
        if let Err(e) = bring_up() {
            self.discard_mode(out, mode);
            let _ = self.shrink_screen();
            return Err(e);
        }
        let _ = self.conn.randr_delete_output_mode(out, old_mode)?.check();
        let _ = self.conn.randr_destroy_mode(old_mode)?.check();
        self.shrink_screen()?;
        tracing::info!(?old_rect, ?new_rect, "virtual monitor resized in place");
        let a = self.active.as_mut().expect("active");
        a.mode = mode;
        a.rect = new_rect;
        Ok(new_rect)
    }

    /// Create the `w`x`h` mode and attach it to `out`. Leaves nothing behind if the driver refuses it.
    fn install_mode(&self, out: Output, w: u32, h: u32) -> Result<u32> {
        let name = format!("{MODE_PREFIX}{w}x{h}");
        let mode = self
            .conn
            .randr_create_mode(self.root, modeline(w, h, self.opts.hz), name.as_bytes())?
            .reply()?
            .mode;
        if let Err(e) = self.conn.randr_add_output_mode(out, mode)?.check() {
            let _ = self.conn.randr_destroy_mode(mode)?.check();
            return Err(anyhow!("output refused mode {w}x{h}: {e}"));
        }
        Ok(mode)
    }

    fn discard_mode(&self, out: Output, mode: u32) {
        let _ = self
            .conn
            .randr_delete_output_mode(out, mode)
            .map(|c| c.check());
        let _ = self.conn.randr_destroy_mode(mode).map(|c| c.check());
        let _ = self.conn.flush();
    }

    /// Create the monitor at `w`x`h`, replacing any previous one. Returns its rectangle in screen coordinates.
    fn create(&mut self, w: u32, h: u32) -> Result<Rect> {
        self.down();
        let res = self.res()?;
        let ts = res.config_timestamp;

        // Active CRTCs (their configs, so we can restore them) and output names.
        let mut saved = Vec::new();
        for c in &res.crtcs {
            let i = self.conn.randr_get_crtc_info(*c, ts)?.reply()?;
            if i.mode != 0 {
                saved.push(SavedCrtc {
                    crtc: *c,
                    x: i.x,
                    y: i.y,
                    mode: i.mode,
                    rotation: i.rotation,
                    outputs: i.outputs.clone(),
                });
            }
        }
        let mut outs = Vec::new();
        for o in &res.outputs {
            outs.push((*o, self.output_name_of(ts, *o)?));
        }

        // Reference monitor.
        let ref_out = match &self.opts.reference {
            Some(n) => outs
                .iter()
                .find(|(_, (name, _))| name == n)
                .map(|(o, _)| *o)
                .ok_or_else(|| anyhow!("reference output '{n}' not found"))?,
            None => {
                let p = self
                    .conn
                    .randr_get_output_primary(self.root)?
                    .reply()?
                    .output;
                if p != 0 {
                    p
                } else {
                    outs.iter()
                        .find(|(_, (_, i))| i.connection == OutConn::CONNECTED && i.crtc != 0)
                        .map(|(o, _)| *o)
                        .ok_or_else(|| anyhow!("no active monitor to place relative to"))?
                }
            }
        };
        let (ref_name, ref_info) = &outs.iter().find(|(o, _)| *o == ref_out).unwrap().1;
        let ref_rect = self
            .crtc_rect(ts, ref_info.crtc)?
            .ok_or_else(|| anyhow!("reference monitor '{ref_name}' is not active"))?;

        // Spare output: disconnected, unused, and able to use the same CRTCs as the reference (same GPU).
        let target = outs
            .iter()
            .find(|(o, (name, i))| {
                *o != ref_out
                    && i.connection == OutConn::DISCONNECTED
                    && i.crtc == 0
                    && self.opts.output.as_ref().is_none_or(|n| n == name)
                    && (self.opts.output.is_some()
                        || i.crtcs.iter().any(|c| ref_info.crtcs.contains(c)))
            })
            .ok_or_else(|| anyhow!("no spare disconnected output found (use --output NAME)"))?;
        let (out, (out_name, out_info)) = (target.0, (&target.1.0, &target.1.1));
        let crtc = out_info
            .crtcs
            .iter()
            .copied()
            .find(|c| !saved.iter().any(|s| s.crtc == *c))
            .ok_or_else(|| anyhow!("no free CRTC for output {out_name}"))?;

        // Geometry (shift everything if the new monitor lands at negative coordinates).
        let (px, py) = place(ref_rect, w, h, self.opts.placement, self.opts.offset);
        let new_rect = Rect { x: px, y: py, w, h };
        let mut all: Vec<Rect> = saved
            .iter()
            .filter_map(|s| self.crtc_rect(ts, s.crtc).ok().flatten())
            .collect();
        all.push(new_rect);
        let (dx, dy) = origin_shift(&all);
        let new_rect = Rect {
            x: px + dx,
            y: py + dy,
            w,
            h,
        };
        for r in all.iter_mut() {
            r.x += dx;
            r.y += dy;
        }
        // Mode first: this is the step a driver may refuse (e.g. NVIDIA outputs), and it must fail before
        // anything visible (screen size, CRTCs) has been touched.
        let mode = self.install_mode(out, w, h)?;
        let (bw, bh) = bounding(&all);
        let (cw, ch) = self.screen_size()?;
        let bring_up = || -> Result<()> {
            self.set_screen(bw.max(cw), bh.max(ch))?;
            // Let the shell digest the new screen/mode before the CRTC goes live; a burst of requests races
            // with its reaction to the hotplug-style notifications and it switches our CRTC back off.
            std::thread::sleep(Duration::from_millis(settle_ms()));
            if dx != 0 || dy != 0 {
                for s in &saved {
                    self.set_crtc(
                        s.crtc,
                        s.x as i32 + dx,
                        s.y as i32 + dy,
                        s.mode,
                        s.rotation,
                        &s.outputs,
                    )?;
                }
            }
            self.set_crtc(
                crtc,
                new_rect.x,
                new_rect.y,
                mode,
                Rotation::ROTATE0,
                &[out],
            )
        };
        if let Err(e) = bring_up() {
            // Undo whatever got applied so a retry starts from the original layout.
            for s in &saved {
                let _ = self.set_crtc(
                    s.crtc, s.x as i32, s.y as i32, s.mode, s.rotation, &s.outputs,
                );
            }
            self.discard_mode(out, mode);
            let _ = self.shrink_screen();
            return Err(e);
        }

        let reference = saved
            .iter()
            .find(|s| s.crtc == ref_info.crtc)
            .map(|s| SavedCrtc {
                x: s.x + dx as i16,
                y: s.y + dy as i16,
                ..s.clone()
            });
        let moved = if dx != 0 || dy != 0 {
            saved
        } else {
            Vec::new()
        };
        self.shrink_screen()?;
        tracing::info!(output = %out_name, ?new_rect, "virtual monitor up");
        self.active = Some(Active {
            output: out,
            crtc,
            mode,
            rect: new_rect,
            moved,
            reference,
        });
        Ok(new_rect)
    }

    /// Check that the reference monitor (the laptop panel) is still lit; if the shell or driver switched it
    /// off while we were reconfiguring, put its CRTC back immediately.
    pub fn guard_reference(&self) -> Guard {
        let Some(r) = self.active.as_ref().and_then(|a| a.reference.as_ref()) else {
            return Guard::Ok;
        };
        let probe = || -> Result<bool> {
            let res = self.res()?;
            let i = self
                .conn
                .randr_get_crtc_info(r.crtc, res.config_timestamp)?
                .reply()?;
            Ok(!needs_restore(i.mode, &i.outputs))
        };
        match probe() {
            Ok(true) => Guard::Ok,
            Ok(false) | Err(_) => {
                tracing::error!(crtc = r.crtc, "reference monitor went dark, restoring it");
                match self.set_crtc(
                    r.crtc, r.x as i32, r.y as i32, r.mode, r.rotation, &r.outputs,
                ) {
                    Ok(()) => Guard::Restored,
                    Err(e) => {
                        tracing::error!(error = %e, "could not restore reference monitor");
                        Guard::Lost
                    }
                }
            }
        }
    }

    /// Remove the monitor and restore everything we moved. Best effort, never panics.
    pub fn down(&mut self) {
        let Some(a) = self.active.take() else { return };
        let r = (|| -> Result<()> {
            self.set_crtc(a.crtc, 0, 0, 0, Rotation::ROTATE0, &[])?;
            let _ = self
                .conn
                .randr_delete_output_mode(a.output, a.mode)?
                .check();
            let _ = self.conn.randr_destroy_mode(a.mode)?.check();
            for s in &a.moved {
                self.set_crtc(
                    s.crtc, s.x as i32, s.y as i32, s.mode, s.rotation, &s.outputs,
                )?;
            }
            self.shrink_screen()
        })();
        match r {
            Ok(()) => tracing::info!("virtual monitor down"),
            Err(e) => tracing::warn!(error = %e, "virtual monitor teardown incomplete"),
        }
        let _ = self.conn.flush();
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.down();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAPTOP: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn modeline_is_consistent() {
        let m = modeline(2160, 1620, 60);
        assert_eq!((m.width, m.height), (2160, 1620));
        assert!(m.hsync_start > m.width && m.hsync_end > m.hsync_start && m.htotal > m.hsync_end);
        assert!(m.vsync_start > m.height && m.vsync_end > m.vsync_start && m.vtotal > m.vsync_end);
        let hz = m.dot_clock as f64 / (m.htotal as f64 * m.vtotal as f64);
        assert!((hz - 60.0).abs() < 0.01, "hz={hz}");
        assert!(m.dot_clock < 600_000_000);
        // X reads the name length from the mode record; 0 would create an unnamed (non-unique) mode.
        assert_eq!(m.name_len as usize, "padscreen-2160x1620".len());
    }

    #[test]
    fn dark_crtc_detected() {
        assert!(needs_restore(0, &[1]));
        assert!(needs_restore(7, &[]));
        assert!(!needs_restore(7, &[1]));
    }

    #[test]
    fn placement_all_sides() {
        assert_eq!(place(LAPTOP, 800, 600, Placement::Right, 0), (1920, 0));
        assert_eq!(place(LAPTOP, 800, 600, Placement::Right, 40), (1920, 40));
        assert_eq!(place(LAPTOP, 800, 600, Placement::Left, 0), (-800, 0));
        assert_eq!(place(LAPTOP, 800, 600, Placement::Above, 0), (0, -600));
        assert_eq!(place(LAPTOP, 800, 600, Placement::Below, -10), (-10, 1080));
    }

    #[test]
    fn negative_positions_shift_everything_to_origin() {
        let new = Rect {
            x: -800,
            y: 0,
            w: 800,
            h: 600,
        };
        assert_eq!(origin_shift(&[LAPTOP, new]), (800, 0));
        let above = Rect {
            x: 0,
            y: -600,
            w: 800,
            h: 600,
        };
        assert_eq!(origin_shift(&[LAPTOP, above]), (0, 600));
        assert_eq!(
            origin_shift(&[
                LAPTOP,
                Rect {
                    x: 1920,
                    y: 0,
                    w: 5,
                    h: 5
                }
            ]),
            (0, 0)
        );
    }

    #[test]
    fn bounding_box() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let b = Rect {
            x: 1920,
            y: 0,
            w: 2160,
            h: 1620,
        };
        assert_eq!(bounding(&[a, b]), (4080, 1620));
        assert_eq!(bounding(&[]), (0, 0));
    }

    #[test]
    fn client_size_is_even_clamped_and_scaled() {
        assert_eq!(clamp_size(2160, 1449, 1.0), (2160, 1448));
        assert_eq!(clamp_size(10, 10, 1.0), (320, 320));
        assert_eq!(clamp_size(9000, 9000, 1.0), (4096, 4096));
        assert_eq!(clamp_size(2160, 1620, 0.5), (1080, 810));
    }
}
