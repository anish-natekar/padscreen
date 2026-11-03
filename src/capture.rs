//! Capture + encode via an ffmpeg child process emitting Annex-B H.264 on stdout.
//! M1 backend; replaced later by native XShm/XDamage + libav encoding.

use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::process::{Child, Command};

#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
}

impl std::str::FromStr for Region {
    type Err = String;
    /// `WxH` or `WxH+X+Y`
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("bad region '{s}', want WxH or WxH+X+Y");
        let (size, off) = match s.find('+') {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let (x, y) = match off {
            Some(o) => o.split_once('+').ok_or_else(bad)?,
            None => ("0", "0"),
        };
        Ok(Region {
            w: w.parse().map_err(|_| bad())?,
            h: h.parse().map_err(|_| bad())?,
            x: x.parse().map_err(|_| bad())?,
            y: y.parse().map_err(|_| bad())?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Source {
    X11,
    Test,
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Encoder {
    Auto,
    Nvenc,
    Vaapi,
    X264,
}

#[derive(Clone, Debug)]
pub struct CaptureOpts {
    pub source: Source,
    pub display: String,
    pub region: Region,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gop: u32,
    /// Rate-control buffer depth in frames (smaller = lower delay, less headroom for busy frames).
    pub vbv_frames: f32,
    pub encoder: Encoder,
}

impl CaptureOpts {
    pub fn resolved_encoder(&self) -> Encoder {
        match self.encoder {
            Encoder::Auto if std::path::Path::new("/dev/nvidiactl").exists() => Encoder::Nvenc,
            Encoder::Auto => Encoder::X264,
            e => e,
        }
    }

    pub fn ffmpeg_args(&self) -> Vec<String> {
        let r = &self.region;
        let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"]
            .map(String::from)
            .into();
        match self.source {
            Source::X11 => a.extend([
                "-f".into(),
                "x11grab".into(),
                "-framerate".into(),
                self.fps.to_string(),
                "-video_size".into(),
                format!("{}x{}", r.w, r.h),
                "-draw_mouse".into(),
                "1".into(),
                "-i".into(),
                format!("{}+{},{}", self.display, r.x, r.y),
            ]),
            Source::Test => a.extend([
                "-f".into(),
                "lavfi".into(),
                "-re".into(),
                "-i".into(),
                format!("testsrc2=size={}x{}:rate={}", r.w, r.h, self.fps),
            ]),
        }
        let br = format!("{}k", self.bitrate_kbps);
        let gop = self.gop.to_string();
        let enc = self.resolved_encoder();
        match enc {
            Encoder::Nvenc => a.extend(
                [
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    "p1",
                    "-tune",
                    "ll",
                    "-rc",
                    "cbr",
                    "-b:v",
                    &br,
                    "-maxrate",
                    &br,
                    "-bufsize",
                    &format!("{}k", self.bitrate_kbps / self.fps.max(1) * 2),
                    "-zerolatency",
                    "1",
                    "-bf",
                    "0",
                    "-g",
                    &gop,
                    "-profile:v",
                    "high",
                    "-bsf:v",
                    "dump_extra=freq=keyframe,h264_metadata=aud=insert",
                ]
                .map(String::from),
            ),
            Encoder::Vaapi => a.extend(
                [
                    "-vaapi_device",
                    "/dev/dri/renderD128",
                    "-vf",
                    "format=nv12,hwupload",
                    "-c:v",
                    "h264_vaapi",
                    "-b:v",
                    &br,
                    "-bf",
                    "0",
                    "-g",
                    &gop,
                    "-profile:v",
                    "high",
                    "-bsf:v",
                    "dump_extra=freq=keyframe,h264_metadata=aud=insert",
                ]
                .map(String::from),
            ),
            Encoder::X264 | Encoder::Auto => a.extend(
                [
                    "-vf",
                    "format=yuv420p",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-b:v",
                    &br,
                    "-maxrate",
                    &br,
                    "-bufsize",
                    &format!("{}k", self.bitrate_kbps / self.fps.max(1) * 2),
                    "-bf",
                    "0",
                    "-g",
                    &gop,
                    "-profile:v",
                    "high",
                    "-bsf:v",
                    "h264_metadata=aud=insert",
                ]
                .map(String::from),
            ),
        }
        a.extend(["-flush_packets", "1", "-f", "h264", "pipe:1"].map(String::from));
        a
    }
}

/// `gst-launch-1.0` pipeline for a Mutter ScreenCast PipeWire node: variable-rate BGRx frames at the exact
/// size asked for (which sizes the virtual monitor) -> constant `fps` -> H.264 Annex-B with AUDs on stdout.
pub fn gst_args(node: u32, w: u32, h: u32, o: &CaptureOpts) -> Vec<String> {
    let (br, fps) = (o.bitrate_kbps, o.fps.max(1));
    let vbv = (br as f32 / fps as f32 * o.vbv_frames).max(1.0) as u32;
    let mut p = vec![
        // keepalive re-sends the last frame: mutter only delivers on damage and videorate cannot invent frames
        format!(
            "pipewiresrc path={node} always-copy=true do-timestamp=true keepalive-time={}",
            1000 / fps
        ),
        format!("video/x-raw,width={w},height={h},framerate=0/1,max-framerate=60/1"),
        "videorate".into(),
        format!("video/x-raw,framerate={fps}/1"),
        "queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream".into(),
        "videoconvert".into(),
    ];
    // Safari decodes High 4:2:0 only, so pin the pixel format feeding the encoder.
    match o.resolved_encoder() {
        Encoder::Nvenc => p.extend([
            "video/x-raw,format=BGRA".into(),
            format!("nvh264enc preset=low-latency-hp rc-mode=cbr zerolatency=true bitrate={br} vbv-buffer-size={vbv} gop-size={} bframes=0 aud=true", o.gop),
        ]),
        Encoder::Vaapi => p.extend([
            "video/x-raw,format=NV12".into(),
            format!("vah264enc rate-control=cbr bitrate={br} cpb-size={vbv} key-int-max={} b-frames=0 aud=true target-usage=7", o.gop),
        ]),
        Encoder::X264 | Encoder::Auto => p.extend([
            "video/x-raw,format=I420".into(),
            format!("x264enc tune=zerolatency speed-preset=ultrafast bitrate={br} vbv-buf-capacity={} key-int-max={} bframes=0 aud=true byte-stream=true", (1000.0 * o.vbv_frames / fps as f32).max(1.0) as u32, o.gop),
        ]),
    }
    p.extend([
        "video/x-h264,profile=high".into(),
        "h264parse config-interval=-1".into(),
        "video/x-h264,stream-format=byte-stream,alignment=au".into(),
        "fdsink fd=1 sync=false".into(),
    ]);
    // gst-launch wants one argv per token, elements separated by "!".
    let mut a = vec!["-q".to_string()];
    for (i, e) in p.iter().enumerate() {
        if i > 0 {
            a.push("!".into());
        }
        a.extend(e.split_whitespace().map(String::from));
    }
    a
}

/// Start the GStreamer capture+encode child. `w`/`h` must be even.
pub fn spawn_gst(node: u32, w: u32, h: u32, opts: &CaptureOpts) -> Result<Child> {
    if w % 2 != 0 || h % 2 != 0 {
        bail!("size must be even, got {w}x{h}");
    }
    let args = gst_args(node, w, h, opts);
    tracing::info!(encoder = ?opts.resolved_encoder(), "gst-launch-1.0 {}", args.join(" "));
    Command::new("gst-launch-1.0")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawn gst-launch-1.0")
}

pub fn spawn(opts: &CaptureOpts) -> Result<Child> {
    if opts.region.w % 2 != 0 || opts.region.h % 2 != 0 {
        bail!(
            "region size must be even, got {}x{}",
            opts.region.w,
            opts.region.h
        );
    }
    let args = opts.ffmpeg_args();
    tracing::info!(encoder = ?opts.resolved_encoder(), "ffmpeg {}", args.join(" "));
    Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("spawn ffmpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(enc: Encoder) -> CaptureOpts {
        CaptureOpts {
            source: Source::X11,
            display: ":0".into(),
            region: Region {
                w: 0,
                h: 0,
                x: 0,
                y: 0,
            },
            fps: 60,
            bitrate_kbps: 20000,
            gop: 60,
            vbv_frames: 4.0,
            encoder: enc,
        }
    }

    #[test]
    fn gst_pipeline_shapes() {
        for (enc, needle, fmt) in [
            (Encoder::Nvenc, "nvh264enc", "BGRA"),
            (Encoder::Vaapi, "vah264enc", "NV12"),
            (Encoder::X264, "x264enc", "I420"),
        ] {
            let a = gst_args(62, 2160, 1620, &opts(enc)).join(" ");
            assert!(
                a.starts_with("-q pipewiresrc path=62 always-copy=true"),
                "{a}"
            );
            assert!(a.contains("keepalive-time=16"), "{a}");
            assert!(
                a.contains("width=2160,height=1620,framerate=0/1,max-framerate=60/1"),
                "{a}"
            );
            assert!(
                a.contains(needle)
                    && a.contains(&format!("format={fmt}"))
                    && a.contains("aud=true"),
                "{a}"
            );
            assert!(
                a.contains("profile=high") && a.contains("bframes=0") || a.contains("b-frames=0"),
                "{a}"
            );
            assert!(a.ends_with("fdsink fd=1 sync=false"), "{a}");
            assert!(!a.contains("Y444"), "{a}");
        }
        // every element boundary is a lone "!"
        let a = gst_args(1, 800, 600, &opts(Encoder::X264));
        assert_eq!(a.iter().filter(|t| *t == "!").count(), 11);
    }

    #[test]
    fn region_parse() {
        assert_eq!(
            "1672x1194+1920+0".parse::<Region>().unwrap(),
            Region {
                w: 1672,
                h: 1194,
                x: 1920,
                y: 0
            }
        );
        assert_eq!(
            "640x360".parse::<Region>().unwrap(),
            Region {
                w: 640,
                h: 360,
                x: 0,
                y: 0
            }
        );
        assert!("640".parse::<Region>().is_err());
        assert!("640x360+5".parse::<Region>().is_err());
    }
}
