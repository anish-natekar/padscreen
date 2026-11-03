//! Pointer injection on the laptop (XTEST, no root). Clients send normalised coordinates (0..1 over
//! the streamed picture); we map them onto the virtual monitor's rectangle.

use crate::display::Rect;
use crate::mutter::{Op, evdev_button, to_stream};
use crate::server::AppState;
use std::ops::Not;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use x11rb::{
    connection::Connection,
    protocol::{xproto, xtest},
    rust_connection::RustConnection,
};

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Move {
        x: f32,
        y: f32,
    },
    Button {
        x: f32,
        y: f32,
        button: u8,
        down: bool,
    },
    /// Wheel travel in notches; positive = down / right.
    Scroll {
        dx: f32,
        dy: f32,
    },
    /// Native touch (Wayland only): `slot` is the client's finger id.
    Touch {
        phase: Phase,
        slot: u32,
        x: f32,
        y: f32,
    },
    /// Client went away: lift every finger and release every button still down.
    Reset,
}

impl InputEvent {
    /// Moves and wheel ticks may be dropped when the injector falls behind; presses and lifts may not.
    pub fn droppable(&self) -> bool {
        matches!(
            self,
            Self::Move { .. }
                | Self::Scroll { .. }
                | Self::Touch {
                    phase: Phase::Move,
                    ..
                }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

/// Largest wheel travel, in notches, one message may ask for.
const MAX_SCROLL: f32 = 100.0;

/// Fingers a touchscreen can plausibly have down at once.
pub const MAX_SLOTS: u32 = 16;

/// `{"type":"in","e":"move","x":..,"y":..}`, `"e":"down"|"up"` with `"b":1..3`, `"e":"scroll"` with `dx`,`dy`,
/// `"e":"td"|"tm"|"tu"` (touch down / move / up) with finger `"id"` and, except for `tu`, `x`,`y`.
pub fn parse(text: &str) -> Option<InputEvent> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v["type"] != "in" {
        return None;
    }
    // JSON numbers beyond f32 range cast to infinity, so test finiteness after the cast
    let f = |k: &str| v[k].as_f64().map(|n| n as f32).filter(|n| n.is_finite());
    match v["e"].as_str()? {
        "move" => Some(InputEvent::Move {
            x: f("x")?,
            y: f("y")?,
        }),
        e @ ("down" | "up") => {
            let button = match &v["b"] {
                serde_json::Value::Null => 1,
                b => b.as_u64()?,
            };
            (1..=3).contains(&button).then_some(InputEvent::Button {
                x: f("x")?,
                y: f("y")?,
                button: button as u8,
                down: e == "down",
            })
        }
        "scroll" => Some(InputEvent::Scroll {
            dx: f("dx")?.clamp(-MAX_SCROLL, MAX_SCROLL),
            dy: f("dy")?.clamp(-MAX_SCROLL, MAX_SCROLL),
        }),
        e @ ("td" | "tm" | "tu") => {
            let slot = v["id"].as_u64().filter(|s| *s < MAX_SLOTS as u64)? as u32;
            let phase = match e {
                "td" => Phase::Down,
                "tm" => Phase::Move,
                _ => Phase::Up,
            };
            // lift events carry no position
            let (x, y) = if phase == Phase::Up {
                (0.0, 0.0)
            } else {
                (f("x")?, f("y")?)
            };
            Some(InputEvent::Touch { phase, slot, x, y })
        }
        _ => None,
    }
}

/// Normalised position -> absolute root coordinates inside `r` (clamped to the monitor).
pub fn to_abs(r: Rect, x: f32, y: f32) -> (i16, i16) {
    let px = (x.clamp(0.0, 1.0) * (r.w as f32 - 1.0)).round() as i32 + r.x;
    let py = (y.clamp(0.0, 1.0) * (r.h as f32 - 1.0)).round() as i32 + r.y;
    (
        px.clamp(0, i16::MAX as i32) as i16,
        py.clamp(0, i16::MAX as i32) as i16,
    )
}

/// Whole wheel notches to emit now; the fractional rest stays in `acc`.
pub fn take_notches(acc: &mut f32) -> i32 {
    if !acc.is_finite() {
        *acc = 0.0;
    }
    let n = acc.trunc();
    *acc -= n;
    n as i32
}

struct Injector {
    conn: RustConnection,
    root: u32,
    acc: (f32, f32),
    held: std::collections::BTreeSet<u8>,
}

impl Injector {
    fn new() -> anyhow::Result<Self> {
        let (conn, n) = RustConnection::connect(None)?;
        let root = conn.setup().roots[n].root;
        xtest::get_version(&conn, 2, 2)?.reply()?;
        Ok(Self {
            conn,
            root,
            acc: (0.0, 0.0),
            held: Default::default(),
        })
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) {
        let _ = xtest::fake_input(&self.conn, ty, detail, 0, self.root, x, y, 0);
    }

    fn motion(&self, r: Rect, x: f32, y: f32) {
        let (ax, ay) = to_abs(r, x, y);
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, ax, ay);
    }

    fn handle(&mut self, r: Rect, ev: InputEvent) {
        match ev {
            InputEvent::Move { x, y } => self.motion(r, x, y),
            InputEvent::Button { x, y, button, down } => {
                if down {
                    self.held.insert(button);
                } else {
                    self.held.remove(&button);
                }
                self.motion(r, x, y);
                self.fake(
                    if down {
                        xproto::BUTTON_PRESS_EVENT
                    } else {
                        xproto::BUTTON_RELEASE_EVENT
                    },
                    button,
                    0,
                    0,
                );
            }
            InputEvent::Touch { .. } => {}
            InputEvent::Reset => {
                for b in std::mem::take(&mut self.held) {
                    self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
                }
            }
            InputEvent::Scroll { dx, dy } => {
                self.acc.0 += dx;
                self.acc.1 += dy;
                let (nx, ny) = (take_notches(&mut self.acc.0), take_notches(&mut self.acc.1));
                // X buttons 4/5 = up/down, 6/7 = left/right
                for (n, neg, pos) in [(ny, 4u8, 5u8), (nx, 6, 7)] {
                    for _ in 0..n.unsigned_abs().min(20) {
                        let b = if n < 0 { neg } else { pos };
                        self.fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0);
                        self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0);
                    }
                }
            }
        }
        let _ = self.conn.flush();
    }
}

/// Wayland: events become Mutter RemoteDesktop calls on the live cast, addressed in stream pixels.
pub fn wl_ops(
    w: u32,
    h: u32,
    acc: &mut (f32, f32),
    down: &mut std::collections::BTreeSet<u32>,
    ev: InputEvent,
) -> Vec<Op> {
    let motion = |x, y| {
        let (x, y) = to_stream(w, h, x, y);
        Op::Motion { x, y }
    };
    match ev {
        InputEvent::Touch { phase, slot, x, y } => {
            let (px, py) = to_stream(w, h, x, y);
            match phase {
                Phase::Down => {
                    // a repeated down for a live slot would be rejected by mutter
                    let lift = down.insert(slot).not().then_some(Op::TouchUp { slot });
                    lift.into_iter()
                        .chain([Op::TouchDown { slot, x: px, y: py }])
                        .collect()
                }
                Phase::Move if down.contains(&slot) => vec![Op::TouchMotion { slot, x: px, y: py }],
                Phase::Up if down.remove(&slot) => vec![Op::TouchUp { slot }],
                _ => vec![],
            }
        }
        InputEvent::Reset => std::mem::take(down)
            .into_iter()
            .map(|slot| Op::TouchUp { slot })
            .collect(),
        InputEvent::Move { x, y } => vec![motion(x, y)],
        InputEvent::Button { x, y, button, down } => match evdev_button(button) {
            Some(code) => vec![motion(x, y), Op::Button { code, down }],
            None => vec![],
        },
        InputEvent::Scroll { dx, dy } => {
            acc.0 += dx;
            acc.1 += dy;
            let (nx, ny) = (take_notches(&mut acc.0), take_notches(&mut acc.1));
            [(0u32, ny), (1, nx)]
                .into_iter()
                .filter(|(_, n)| *n != 0)
                .map(|(axis, n)| Op::Axis {
                    axis,
                    steps: n.clamp(-20, 20),
                })
                .collect()
        }
    }
}

/// How long one RemoteDesktop call may take before it counts as failed.
const OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Task that forwards events to the current Mutter session (dropped while no capture is running).
pub fn spawn_wayland(st: Arc<AppState>, mut rx: Receiver<InputEvent>) {
    tokio::spawn(async move {
        let mut acc = (0.0f32, 0.0f32);
        let mut down = std::collections::BTreeSet::new();
        let mut buttons = std::collections::BTreeSet::new();
        let mut failing = false;
        let mut stream = String::new();
        while let Some(ev) = rx.recv().await {
            let Some(cast) = st.wl.read().unwrap().clone() else {
                // the session that owned those fingers and buttons is gone
                down.clear();
                buttons.clear();
                continue;
            };
            if cast.stream != stream {
                // fingers and buttons belong to the session that saw them go down
                down.clear();
                buttons.clear();
                stream = cast.stream.clone();
            }
            let (w, h) = (
                st.width.load(std::sync::atomic::Ordering::Relaxed),
                st.height.load(std::sync::atomic::Ordering::Relaxed),
            );
            let reset = ev == InputEvent::Reset;
            let mut ops = wl_ops(w, h, &mut acc, &mut down, ev);
            if reset {
                ops.extend(
                    std::mem::take(&mut buttons)
                        .into_iter()
                        .map(|code| Op::Button { code, down: false }),
                );
            }
            for op in ops {
                if let Op::Button { code, down } = op {
                    if down {
                        buttons.insert(code);
                    } else {
                        buttons.remove(&code);
                    }
                }
                // a stalled compositor must not wedge input for good
                let res = match tokio::time::timeout(OP_TIMEOUT, cast.apply(op)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow::anyhow!(
                        "compositor did not answer in {OP_TIMEOUT:?}"
                    )),
                };
                match res {
                    Ok(()) => failing = false,
                    Err(e) => {
                        // one warning per streak of failures, not one per mouse move
                        if !std::mem::replace(&mut failing, true) {
                            tracing::warn!(error = %e, "input op failed");
                        }
                        break;
                    }
                }
            }
        }
    });
}

/// Dedicated thread (blocking X connection). Ends when every sender is dropped.
pub fn spawn(
    st: Arc<AppState>,
    mut rx: Receiver<InputEvent>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let mut inj = Injector::new()?;
    Ok(std::thread::spawn(move || {
        while let Some(ev) = rx.blocking_recv() {
            let rect = *st.rect.read().unwrap();
            if let Some(r) = rect {
                inj.handle(r, ev);
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        assert_eq!(
            parse(r#"{"type":"in","e":"move","x":0.5,"y":0.25}"#),
            Some(InputEvent::Move { x: 0.5, y: 0.25 })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"down","x":0,"y":1,"b":3}"#),
            Some(InputEvent::Button {
                x: 0.0,
                y: 1.0,
                button: 3,
                down: true
            })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"up","x":0,"y":1}"#),
            Some(InputEvent::Button {
                x: 0.0,
                y: 1.0,
                button: 1,
                down: false
            })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"scroll","dx":0,"dy":-1.5}"#),
            Some(InputEvent::Scroll { dx: 0.0, dy: -1.5 })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"down","x":0,"y":0,"b":9}"#),
            None,
            "bad button"
        );
        assert_eq!(parse(r#"{"type":"in","e":"move","x":"a","y":0}"#), None);
        assert_eq!(parse(r#"{"type":"hello"}"#), None);
        assert_eq!(parse("junk"), None);
    }

    #[test]
    fn maps_onto_monitor_rect() {
        let r = Rect {
            x: 1920,
            y: 0,
            w: 800,
            h: 600,
        };
        assert_eq!(to_abs(r, 0.0, 0.0), (1920, 0));
        assert_eq!(to_abs(r, 1.0, 1.0), (1920 + 799, 599));
        assert_eq!(to_abs(r, 0.5, 0.5), (1920 + 400, 300));
        assert_eq!(to_abs(r, -3.0, 9.0), (1920, 599), "clamped");
    }

    #[test]
    fn wayland_ops() {
        let mut acc = (0.0, 0.0);
        let mut dn = std::collections::BTreeSet::new();
        assert_eq!(
            wl_ops(
                2160,
                1620,
                &mut acc,
                &mut dn,
                InputEvent::Move { x: 0.5, y: 0.5 }
            ),
            vec![Op::Motion {
                x: 1080.0,
                y: 810.0
            }]
        );
        assert_eq!(
            wl_ops(
                2160,
                1620,
                &mut acc,
                &mut dn,
                InputEvent::Button {
                    x: 0.0,
                    y: 1.0,
                    button: 3,
                    down: true
                }
            ),
            vec![
                Op::Motion { x: 0.0, y: 1619.0 },
                Op::Button {
                    code: 0x111,
                    down: true
                }
            ]
        );
        assert!(
            wl_ops(
                10,
                10,
                &mut acc,
                &mut dn,
                InputEvent::Button {
                    x: 0.0,
                    y: 0.0,
                    button: 9,
                    down: true
                }
            )
            .is_empty()
        );
        // fractional scroll accumulates, vertical + horizontal come out as separate axes
        assert!(
            wl_ops(
                10,
                10,
                &mut acc,
                &mut dn,
                InputEvent::Scroll { dx: 0.0, dy: 0.6 }
            )
            .is_empty()
        );
        assert_eq!(
            wl_ops(
                10,
                10,
                &mut acc,
                &mut dn,
                InputEvent::Scroll { dx: -2.0, dy: 0.6 }
            ),
            vec![
                Op::Axis { axis: 0, steps: 1 },
                Op::Axis { axis: 1, steps: -2 }
            ]
        );
        assert_eq!(
            wl_ops(
                10,
                10,
                &mut acc,
                &mut dn,
                InputEvent::Scroll { dx: 0.0, dy: 99.0 }
            ),
            vec![Op::Axis { axis: 0, steps: 20 }],
            "capped"
        );
    }

    #[test]
    fn parses_touch() {
        assert_eq!(
            parse(r#"{"type":"in","e":"td","id":2,"x":0.5,"y":0.25}"#),
            Some(InputEvent::Touch {
                phase: Phase::Down,
                slot: 2,
                x: 0.5,
                y: 0.25
            })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"tm","id":0,"x":1,"y":0}"#),
            Some(InputEvent::Touch {
                phase: Phase::Move,
                slot: 0,
                x: 1.0,
                y: 0.0
            })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"tu","id":3}"#),
            Some(InputEvent::Touch {
                phase: Phase::Up,
                slot: 3,
                x: 0.0,
                y: 0.0
            })
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"td","id":99,"x":0,"y":0}"#),
            None,
            "slot out of range"
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"td","x":0,"y":0}"#),
            None,
            "missing id"
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"tm","id":1}"#),
            None,
            "move needs a position"
        );
    }

    #[test]
    fn touch_ops_track_live_slots() {
        let (mut acc, mut dn) = ((0.0, 0.0), std::collections::BTreeSet::new());
        let t = |phase, slot| InputEvent::Touch {
            phase,
            slot,
            x: 0.5,
            y: 0.5,
        };
        assert!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Move, 0)).is_empty(),
            "move without down"
        );
        assert!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Up, 0)).is_empty(),
            "up without down"
        );
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Down, 0)),
            vec![Op::TouchDown {
                slot: 0,
                x: 5.0,
                y: 5.0
            }]
        );
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Down, 1)),
            vec![Op::TouchDown {
                slot: 1,
                x: 5.0,
                y: 5.0
            }]
        );
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Move, 1)),
            vec![Op::TouchMotion {
                slot: 1,
                x: 5.0,
                y: 5.0
            }]
        );
        // duplicate down lifts the stale finger first
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Down, 1)),
            vec![
                Op::TouchUp { slot: 1 },
                Op::TouchDown {
                    slot: 1,
                    x: 5.0,
                    y: 5.0
                }
            ]
        );
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, t(Phase::Up, 0)),
            vec![Op::TouchUp { slot: 0 }]
        );
        // reset lifts whatever is left, once
        assert_eq!(
            wl_ops(11, 11, &mut acc, &mut dn, InputEvent::Reset),
            vec![Op::TouchUp { slot: 1 }]
        );
        assert!(wl_ops(11, 11, &mut acc, &mut dn, InputEvent::Reset).is_empty());
    }

    #[test]
    fn wheel_accumulates_fractions() {
        let mut a = 0.0;
        let mut total = 0;
        for _ in 0..10 {
            a += 0.25;
            total += take_notches(&mut a);
        }
        assert_eq!(total, 2);
        let mut b = -1.6;
        assert_eq!(take_notches(&mut b), -1);
        assert!((b + 0.6).abs() < 1e-6);
    }

    #[test]
    fn rejects_hostile_numbers() {
        // 1e300 is a valid f64 but infinite as f32
        assert_eq!(parse(r#"{"type":"in","e":"move","x":1e300,"y":0}"#), None);
        assert_eq!(
            parse(r#"{"type":"in","e":"scroll","dx":0,"dy":1e300}"#),
            None
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"scroll","dx":0,"dy":1e30}"#),
            Some(InputEvent::Scroll {
                dx: 0.0,
                dy: MAX_SCROLL
            }),
            "huge finite scroll is clamped"
        );
        assert_eq!(
            parse(r#"{"type":"in","e":"down","x":0,"y":0,"b":"1"}"#),
            None,
            "button must be a number"
        );
        let mut a = f32::NAN;
        assert_eq!(take_notches(&mut a), 0);
        assert_eq!(a, 0.0);
    }

    #[test]
    fn only_motion_is_droppable() {
        assert!(InputEvent::Move { x: 0.0, y: 0.0 }.droppable());
        assert!(InputEvent::Scroll { dx: 0.0, dy: 1.0 }.droppable());
        assert!(!InputEvent::Reset.droppable());
        assert!(
            !InputEvent::Button {
                x: 0.0,
                y: 0.0,
                button: 1,
                down: false
            }
            .droppable()
        );
    }
}
