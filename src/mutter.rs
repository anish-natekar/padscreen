//! GNOME Wayland backend: a virtual monitor from Mutter's ScreenCast API (`RecordVirtual`), pointer input
//! from the linked RemoteDesktop session. Nothing here touches CRTCs: mutter renders the monitor itself and
//! it disappears when the session stops or this process leaves the bus.

use crate::display::Placement;
use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::{
    Connection, Proxy,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

type Conn4 = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Mon = (Conn4, Vec<Mode>, HashMap<String, OwnedValue>);
type Logical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<Conn4>,
    HashMap<String, OwnedValue>,
);
/// `org.gnome.Mutter.DisplayConfig.GetCurrentState`: serial, monitors, logical monitors, properties.
type State = (u32, Vec<Mon>, Vec<Logical>, HashMap<String, OwnedValue>);
type LogicalCfg = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, Value<'static>>)>,
);

const SC: &str = "org.gnome.Mutter.ScreenCast";
const RD: &str = "org.gnome.Mutter.RemoteDesktop";

/// evdev button codes mutter expects.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// What the RemoteDesktop session is asked to do, already in mutter's units.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Absolute position in stream pixels.
    Motion {
        x: f64,
        y: f64,
    },
    Button {
        code: i32,
        down: bool,
    },
    /// Discrete wheel steps; axis 0 = vertical, 1 = horizontal. Positive = down / right.
    Axis {
        axis: u32,
        steps: i32,
    },
    /// Touch point in stream pixels; `slot` identifies the finger.
    TouchDown {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchMotion {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchUp {
        slot: u32,
    },
}

/// X11-style button number (1 left, 2 middle, 3 right) -> evdev code.
pub fn evdev_button(b: u8) -> Option<i32> {
    match b {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        _ => None,
    }
}

/// Normalised position -> pixel position inside a `w`x`h` stream (clamped to the last pixel).
pub fn to_stream(w: u32, h: u32, x: f32, y: f32) -> (f64, f64) {
    let px = x.clamp(0.0, 1.0) as f64 * (w.max(1) - 1) as f64;
    let py = y.clamp(0.0, 1.0) as f64 * (h.max(1) - 1) as f64;
    (px.round(), py.round())
}

const DC: &str = "org.gnome.Mutter.DisplayConfig";

fn current_mode(m: &Mon) -> Option<&Mode> {
    m.1.iter().find(|mode| {
        mode.6
            .get("is-current")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            == Some(true)
    })
}

fn is_virtual(m: &Mon) -> bool {
    m.0.0.starts_with("Meta-")
}

/// Size of the virtual monitor's current mode, if it exists.
fn virtual_size(st: &State) -> Option<(u32, u32)> {
    st.1.iter()
        .filter(|m| is_virtual(m))
        .find_map(current_mode)
        .map(|m| (m.1 as u32, m.2 as u32))
}

/// The mode a logical monitor is showing, as the size it occupies in mutter's layout. Layout is in physical
/// pixels (layout-mode 2), so UI scale does not change it.
fn layout_size(st: &State, l: &Logical) -> Option<(i32, i32)> {
    let first = &l.5.first()?.0;
    let m = st.1.iter().find(|m| &m.0.0 == first)?;
    current_mode(m).map(|m| (m.1, m.2))
}

/// Config that keeps every logical monitor as it is except the virtual one: that gets UI scale `want`
/// (or the best scale its mode supports, never above `want`) and sits on side `place` of the reference
/// monitor, slid `offset` px along the shared edge. Positions are shifted to a non-negative origin.
/// `None` when nothing would change.
fn plan_layout(
    st: &State,
    want: f64,
    place: Placement,
    offset: i32,
) -> Option<(Vec<LogicalCfg>, f64)> {
    let virt = st.1.iter().find(|m| is_virtual(m))?;
    let mode = current_mode(virt)?;
    let applied = mode
        .5
        .iter()
        .copied()
        .filter(|s| *s <= want + 1e-6)
        .fold(1.0, f64::max);
    let vi =
        st.2.iter()
            .position(|l| l.5.iter().any(|c| c.0 == virt.0.0))?;
    let reference =
        st.2.iter()
            .enumerate()
            .filter(|(i, _)| *i != vi)
            .map(|(_, l)| l)
            .min_by_key(|l| !l.4)?;
    let ((rw, rh), (vw, vh)) = (layout_size(st, reference)?, (mode.1, mode.2));
    let (vx, vy) = match place {
        Placement::Right => (reference.0 + rw, reference.1 + offset),
        Placement::Left => (reference.0 - vw, reference.1 + offset),
        Placement::Below => (reference.0 + offset, reference.1 + rh),
        Placement::Above => (reference.0 + offset, reference.1 - vh),
    };
    let mut pos: Vec<(i32, i32, f64)> = st.2.iter().map(|l| (l.0, l.1, l.2)).collect();
    pos[vi] = (vx, vy, applied);
    if pos == st.2.iter().map(|l| (l.0, l.1, l.2)).collect::<Vec<_>>() {
        return None;
    }
    let (ox, oy) = (
        pos.iter().map(|p| p.0).min()?,
        pos.iter().map(|p| p.1).min()?,
    );
    let cfg =
        st.2.iter()
            .zip(pos)
            .map(|(l, (x, y, scale))| {
                let mons =
                    l.5.iter()
                        .filter_map(|c| {
                            let m = st.1.iter().find(|m| m.0.0 == c.0)?;
                            Some((c.0.clone(), current_mode(m)?.0.clone(), HashMap::new()))
                        })
                        .collect();
                (x - ox, y - oy, scale, l.3, l.4, mons)
            })
            .collect();
    Some((cfg, applied))
}

/// One live virtual monitor plus its input channel.
pub struct Cast {
    conn: Connection,
    rd: Proxy<'static>,
    sc: Proxy<'static>,
    pub stream: String,
    pub node: u32,
}

impl Cast {
    /// Create the sessions, ask for a virtual monitor and wait for its PipeWire node. The monitor itself only
    /// appears (at the size the consumer negotiates) once something connects to that node.
    pub async fn start() -> Result<Cast> {
        let conn = Connection::session().await.context("session bus")?;

        let rd_root = Proxy::new(&conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD).await?;
        let rd_path: OwnedObjectPath = rd_root
            .call("CreateSession", &())
            .await
            .context("RemoteDesktop.CreateSession")?;
        let rd = Proxy::new(&conn, RD, rd_path, "org.gnome.Mutter.RemoteDesktop.Session").await?;
        let rd_id: String = rd.get_property("SessionId").await?;

        let sc_root = Proxy::new(&conn, SC, "/org/gnome/Mutter/ScreenCast", SC).await?;
        let props: HashMap<&str, Value> =
            HashMap::from([("remote-desktop-session-id", Value::from(rd_id))]);
        let sc_path: OwnedObjectPath = sc_root
            .call("CreateSession", &(props,))
            .await
            .context("ScreenCast.CreateSession")?;
        let sc = Proxy::new(&conn, SC, sc_path, "org.gnome.Mutter.ScreenCast.Session").await?;

        // cursor-mode 1 = embedded in the frames.
        let opts: HashMap<&str, Value> = HashMap::from([("cursor-mode", Value::from(1u32))]);
        let stream_path: OwnedObjectPath = sc
            .call("RecordVirtual", &(opts,))
            .await
            .context("RecordVirtual")?;
        let stream = Proxy::new(
            &conn,
            SC,
            stream_path.clone(),
            "org.gnome.Mutter.ScreenCast.Stream",
        )
        .await?;
        let mut added = stream.receive_signal("PipeWireStreamAdded").await?;

        rd.call::<_, _, ()>("Start", &())
            .await
            .context("RemoteDesktop.Start")?;
        let msg = tokio::time::timeout(Duration::from_secs(5), added.next())
            .await
            .map_err(|_| anyhow!("no PipeWire stream after 5 s"))?
            .ok_or_else(|| anyhow!("signal stream closed"))?;
        let node: u32 = msg
            .body()
            .deserialize()
            .context("PipeWireStreamAdded body")?;
        Ok(Cast {
            conn,
            rd,
            sc,
            stream: stream_path.as_str().to_owned(),
            node,
        })
    }

    pub async fn apply(&self, op: Op) -> Result<()> {
        match op {
            Op::Motion { x, y } => {
                self.rd
                    .call::<_, _, ()>("NotifyPointerMotionAbsolute", &(self.stream.as_str(), x, y))
                    .await?
            }
            Op::Button { code, down } => {
                self.rd
                    .call::<_, _, ()>("NotifyPointerButton", &(code, down))
                    .await?
            }
            Op::Axis { axis, steps } => {
                self.rd
                    .call::<_, _, ()>("NotifyPointerAxisDiscrete", &(axis, steps))
                    .await?
            }
            Op::TouchDown { slot, x, y } => {
                self.rd
                    .call::<_, _, ()>("NotifyTouchDown", &(self.stream.as_str(), slot, x, y))
                    .await?
            }
            Op::TouchMotion { slot, x, y } => {
                self.rd
                    .call::<_, _, ()>("NotifyTouchMotion", &(self.stream.as_str(), slot, x, y))
                    .await?
            }
            Op::TouchUp { slot } => self.rd.call::<_, _, ()>("NotifyTouchUp", &(slot,)).await?,
        }
        Ok(())
    }

    async fn display_config(&self) -> Result<Proxy<'static>> {
        Ok(Proxy::new(&self.conn, DC, "/org/gnome/Mutter/DisplayConfig", DC).await?)
    }

    /// Wait until mutter shows the virtual monitor at `w`x`h` (it only appears once a consumer has connected).
    pub async fn wait_monitor(&self, w: u32, h: u32, timeout: Duration) -> Result<()> {
        let dc = self.display_config().await?;
        let end = tokio::time::Instant::now() + timeout;
        loop {
            let st: State = dc
                .call("GetCurrentState", &())
                .await
                .context("GetCurrentState")?;
            if virtual_size(&st) == Some((w, h)) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= end {
                return Err(anyhow!("virtual monitor {w}x{h} did not appear"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Scale the virtual monitor and put it on side `place` of the laptop screen. A temporary config: every
    /// other monitor is passed back unchanged and nothing is persisted. Returns the UI scale in effect.
    pub async fn arrange(&self, ui: u32, place: Placement, offset: i32) -> Result<f64> {
        let dc = self.display_config().await?;
        let st: State = dc
            .call("GetCurrentState", &())
            .await
            .context("GetCurrentState")?;
        let Some((cfg, applied)) = plan_layout(&st, ui as f64, place, offset) else {
            return Ok(st
                .2
                .iter()
                .find(|l| l.5.iter().any(|c| c.0.starts_with("Meta-")))
                .map_or(1.0, |l| l.2));
        };
        let props: HashMap<&str, Value> = HashMap::new();
        dc.call::<_, _, ()>("ApplyMonitorsConfig", &(st.0, 1u32, cfg, props))
            .await
            .context("ApplyMonitorsConfig")?;
        Ok(applied)
    }

    /// End the session; mutter removes the virtual monitor.
    pub async fn stop(&self) {
        if let Err(e) = self.rd.call::<_, _, ()>("Stop", &()).await {
            tracing::debug!(error = %e, "RemoteDesktop.Stop");
        }
        let _ = (&self.sc, &self.conn);
    }
}

#[allow(dead_code)]
fn _path(p: &str) -> Result<ObjectPath<'_>> {
    Ok(ObjectPath::try_from(p)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(n: &str) -> Conn4 {
        (n.into(), "v".into(), "p".into(), "s".into())
    }

    fn mode(id: &str, w: i32, h: i32, scales: &[f64], current: bool) -> Mode {
        let mut props = HashMap::new();
        if current {
            props.insert("is-current".to_string(), OwnedValue::from(true));
        }
        (id.into(), w, h, 60.0, 1.0, scales.to_vec(), props)
    }

    /// eDP at scale 1 plus the iPad's virtual monitor at `ipad_scale`.
    fn state(ipad_scale: f64) -> State {
        let edp: Mon = (
            conn("eDP-1"),
            vec![
                mode("1920x1080@60.030", 1920, 1080, &[1.0, 2.0], true),
                mode("1280x720@60.030", 1280, 720, &[1.0], false),
            ],
            HashMap::new(),
        );
        let meta: Mon = (
            conn("Meta-0"),
            vec![mode("2160x1620@60.000", 2160, 1620, &[1.0, 2.0, 3.0], true)],
            HashMap::new(),
        );
        let logical = vec![
            (0, 0, 1.0, 0, true, vec![conn("eDP-1")], HashMap::new()),
            (
                1920,
                0,
                ipad_scale,
                0,
                false,
                vec![conn("Meta-0")],
                HashMap::new(),
            ),
        ];
        (7, vec![edp, meta], logical, HashMap::new())
    }

    fn at(cfg: &[LogicalCfg], connector: &str) -> (i32, i32, f64) {
        let l = cfg
            .iter()
            .find(|l| l.5.iter().any(|c| c.0 == connector))
            .unwrap();
        (l.0, l.1, l.2)
    }

    #[test]
    fn layout_changes_only_the_virtual_monitor() {
        let (cfg, applied) = plan_layout(&state(1.0), 2.0, Placement::Right, 0).unwrap();
        assert_eq!(applied, 2.0);
        assert_eq!(cfg.len(), 2);
        // eDP: same place, scale, primary flag and its *current* mode id (not the 1280x720 one)
        assert_eq!((cfg[0].0, cfg[0].1, cfg[0].2, cfg[0].4), (0, 0, 1.0, true));
        assert_eq!(cfg[0].5[0].0, "eDP-1");
        assert_eq!(cfg[0].5[0].1, "1920x1080@60.030");
        assert_eq!(
            (cfg[1].0, cfg[1].1, cfg[1].2, cfg[1].4),
            (1920, 0, 2.0, false)
        );
        assert_eq!(
            (cfg[1].5[0].0.as_str(), cfg[1].5[0].1.as_str()),
            ("Meta-0", "2160x1620@60.000")
        );
    }

    #[test]
    fn all_four_sides_are_adjacent_and_non_negative() {
        // the iPad is 2160x1620 in layout space whatever its UI scale; eDP is 1920x1080
        let run = |p, off| {
            plan_layout(&state(2.0), 2.0, p, off).map(|(c, _)| (at(&c, "eDP-1"), at(&c, "Meta-0")))
        };
        assert_eq!(
            run(Placement::Left, 0),
            Some(((2160, 0, 1.0), (0, 0, 2.0))),
            "eDP shifts right so nothing is negative"
        );
        assert_eq!(
            run(Placement::Above, 0),
            Some(((0, 1620, 1.0), (0, 0, 2.0)))
        );
        assert_eq!(
            run(Placement::Below, 0),
            Some(((0, 0, 1.0), (0, 1080, 2.0)))
        );
        assert_eq!(
            run(Placement::Below, 100),
            Some(((0, 0, 1.0), (100, 1080, 2.0))),
            "offset slides along the shared edge"
        );
        assert_eq!(
            run(Placement::Right, 50),
            Some(((0, 0, 1.0), (1920, 50, 2.0)))
        );
        assert_eq!(run(Placement::Right, 0), None, "already there");
    }

    #[test]
    fn scale_clamps_to_supported_and_skips_noops() {
        assert_eq!(
            plan_layout(&state(1.0), 9.0, Placement::Right, 0)
                .unwrap()
                .1,
            3.0,
            "capped at the best supported scale"
        );
        assert!(
            plan_layout(&state(1.0), 1.5, Placement::Right, 0).is_none(),
            "1.5 is not offered; 1.0 is what it already has"
        );
        assert!(plan_layout(&state(2.0), 2.0, Placement::Right, 0).is_none());
        assert!(plan_layout(&state(1.0), 1.0, Placement::Right, 0).is_none());
    }

    #[test]
    fn no_virtual_monitor_means_nothing_to_do() {
        let mut none = state(1.0);
        none.1.retain(|m| !is_virtual(m));
        none.2.truncate(1);
        assert!(plan_layout(&none, 2.0, Placement::Left, 0).is_none());
        assert_eq!(virtual_size(&state(1.0)), Some((2160, 1620)));
        assert_eq!(virtual_size(&none), None);
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(1), Some(0x110));
        assert_eq!(evdev_button(2), Some(0x112));
        assert_eq!(evdev_button(3), Some(0x111));
        assert_eq!(evdev_button(4), None);
    }

    #[test]
    fn stream_coordinates() {
        assert_eq!(to_stream(2160, 1620, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(to_stream(2160, 1620, 1.0, 1.0), (2159.0, 1619.0));
        assert_eq!(to_stream(2160, 1620, 0.5, 0.5), (1080.0, 810.0));
        assert_eq!(to_stream(2160, 1620, -1.0, 7.0), (0.0, 1619.0), "clamped");
        assert_eq!(to_stream(0, 0, 0.5, 0.5), (0.0, 0.0), "degenerate size");
    }
}
