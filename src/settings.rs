//! Per-session knobs a client can change at runtime: stream resolution, UI scale, the fps / latency /
//! quality profile and which side of the laptop screen the iPad sits on. The server starts from CLI
//! defaults; every `hello` or `settings` message carrying any of the fields patches them.

use crate::capture::CaptureOpts;
use crate::display::Placement;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    /// Smallest buffers and bitrate: lowest delay, softer picture in motion.
    Latency,
    /// 60 fps with a few frames of rate-control headroom.
    Balanced,
    /// Highest bitrate and deepest buffer: cleanest picture, a few frames more delay.
    Quality,
    /// 30 fps at a low bitrate: least CPU, GPU and USB load.
    Saver,
}

impl Profile {
    pub const ALL: [Profile; 4] = [
        Profile::Latency,
        Profile::Balanced,
        Profile::Quality,
        Profile::Saver,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::Balanced => "balanced",
            Self::Quality => "quality",
            Self::Saver => "saver",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == s)
    }

    /// (fps, bitrate kbit/s, rate-control buffer in frames)
    pub fn params(self) -> (u32, u32, f32) {
        match self {
            Self::Latency => (60, 12_000, 1.5),
            Self::Balanced => (60, 20_000, 4.0),
            Self::Quality => (60, 40_000, 8.0),
            Self::Saver => (30, 8_000, 4.0),
        }
    }
}

/// Explicit `--fps` / `--bitrate-kbps` / `--gop` flags; they win over whatever the profile says.
#[derive(Clone, Copy, Debug, Default)]
pub struct Overrides {
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub gop: Option<u32>,
}

impl Overrides {
    /// Profile first (keyframe once a second), then the explicit flags.
    pub fn tune(&self, profile: Profile, o: &mut CaptureOpts) {
        let (fps, kbps, vbv) = profile.params();
        o.fps = self.fps.unwrap_or(fps);
        o.bitrate_kbps = self.bitrate_kbps.unwrap_or(kbps);
        o.gop = self.gop.unwrap_or(o.fps);
        o.vbv_frames = vbv;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Fraction of the client's device pixels that is streamed (0.25..=1).
    pub res: f32,
    /// Integer UI scale of the virtual monitor (1..=2). Wayland only.
    pub ui: u32,
    pub profile: Profile,
    /// Which side of the laptop screen the virtual monitor sits on. Wayland only.
    pub place: Placement,
}

/// The fields a message wants changed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Patch {
    pub res: Option<f32>,
    pub ui: Option<u32>,
    pub profile: Option<Profile>,
    pub place: Option<Placement>,
}

impl Patch {
    /// `{"type":"hello"|"settings", "res":..,"ui":..,"profile":"..","place":".."}`. `None` when the message
    /// is not one of those or carries none of the fields.
    pub fn parse(text: &str) -> Option<Patch> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        if !matches!(v["type"].as_str(), Some("hello" | "settings")) {
            return None;
        }
        let p = Patch {
            res: v["res"]
                .as_f64()
                .filter(|r| r.is_finite())
                .map(|r| (r as f32).clamp(0.25, 1.0)),
            ui: v["ui"].as_u64().map(|u| (u as u32).clamp(1, 2)),
            profile: v["profile"].as_str().and_then(Profile::parse),
            place: v["place"].as_str().and_then(Placement::parse),
        };
        (p != Patch::default()).then_some(p)
    }
}

impl Settings {
    pub fn with(self, p: Patch) -> Settings {
        Settings {
            res: p.res.unwrap_or(self.res),
            ui: p.ui.unwrap_or(self.ui),
            profile: p.profile.unwrap_or(self.profile),
            place: p.place.unwrap_or(self.place),
        }
    }

    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({"res": self.res, "ui": self.ui, "profile": self.profile.name(), "place": self.place.as_str()})
    }
}

/// What each profile costs, for the client's menu.
pub fn profiles_json() -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for p in Profile::ALL {
        let (fps, kbps, buf) = p.params();
        m.insert(
            p.name().into(),
            serde_json::json!({"fps": fps, "kbps": kbps, "buf": buf}),
        );
    }
    m.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Settings = Settings {
        res: 1.0,
        ui: 2,
        profile: Profile::Balanced,
        place: Placement::Right,
    };

    #[test]
    fn patches_only_given_fields() {
        let p = Patch::parse(r#"{"type":"settings","res":0.5}"#).unwrap();
        assert_eq!(D.with(p), Settings { res: 0.5, ..D });
        let p = Patch::parse(r#"{"type":"settings","ui":1,"profile":"latency","place":"below"}"#)
            .unwrap();
        assert_eq!(
            D.with(p),
            Settings {
                ui: 1,
                profile: Profile::Latency,
                place: Placement::Below,
                ..D
            }
        );
        let p = Patch::parse(r#"{"type":"hello","pw":2160,"ph":1620,"res":0.75,"ui":2,"profile":"saver","place":"left"}"#).unwrap();
        assert_eq!(
            D.with(p),
            Settings {
                res: 0.75,
                ui: 2,
                profile: Profile::Saver,
                place: Placement::Left
            }
        );
    }

    #[test]
    fn ignores_other_messages_and_clamps() {
        assert_eq!(
            Patch::parse(r#"{"type":"hello","pw":2160,"ph":1620}"#),
            None,
            "no settings fields"
        );
        assert_eq!(
            Patch::parse(r#"{"type":"resize","res":0.5}"#),
            None,
            "wrong type"
        );
        assert_eq!(
            Patch::parse(r#"{"type":"settings","profile":"warp"}"#),
            None,
            "unknown profile"
        );
        assert_eq!(
            Patch::parse(r#"{"type":"settings","place":"diagonal"}"#),
            None,
            "unknown side"
        );
        assert_eq!(Patch::parse("junk"), None);
        assert_eq!(
            D.with(Patch::parse(r#"{"type":"settings","res":9,"ui":0}"#).unwrap()),
            Settings {
                res: 1.0,
                ui: 1,
                ..D
            }
        );
        assert_eq!(
            D.with(Patch::parse(r#"{"type":"settings","res":0,"ui":99}"#).unwrap()),
            Settings {
                res: 0.25,
                ui: 2,
                ..D
            }
        );
    }

    #[test]
    fn profiles_trade_latency_for_quality() {
        let [l, b, q, s] = Profile::ALL.map(Profile::params);
        assert!(
            l.2 < b.2 && b.2 < q.2,
            "buffer depth grows latency -> quality"
        );
        assert!(l.1 < b.1 && b.1 < q.1, "bitrate grows latency -> quality");
        assert_eq!((l.0, b.0, q.0), (60, 60, 60));
        assert!(
            s.0 < b.0 && s.1 < l.1,
            "saver halves fps and is the cheapest"
        );
        assert_eq!(Profile::parse("quality"), Some(Profile::Quality));
    }

    #[test]
    fn flags_override_the_profile() {
        let mut o = CaptureOpts {
            source: crate::capture::Source::X11,
            display: ":0".into(),
            region: crate::capture::Region {
                w: 0,
                h: 0,
                x: 0,
                y: 0,
            },
            fps: 0,
            bitrate_kbps: 0,
            gop: 0,
            vbv_frames: 0.0,
            encoder: crate::capture::Encoder::X264,
        };
        Overrides::default().tune(Profile::Saver, &mut o);
        assert_eq!(
            (o.fps, o.bitrate_kbps, o.gop, o.vbv_frames),
            (30, 8_000, 30, 4.0)
        );
        Overrides {
            fps: Some(45),
            bitrate_kbps: None,
            gop: Some(15),
        }
        .tune(Profile::Latency, &mut o);
        assert_eq!(
            (o.fps, o.bitrate_kbps, o.gop, o.vbv_frames),
            (45, 12_000, 15, 1.5)
        );
    }

    #[test]
    fn json_for_the_client() {
        assert_eq!(
            D.to_json(),
            serde_json::json!({"res": 1.0, "ui": 2, "profile": "balanced", "place": "right"})
        );
        assert_eq!(
            profiles_json()["latency"],
            serde_json::json!({"fps": 60, "kbps": 12000, "buf": 1.5})
        );
    }
}
