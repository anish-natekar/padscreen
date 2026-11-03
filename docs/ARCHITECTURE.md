# Architecture

One binary, `padscreen serve`, plus one self-contained web page (`web/index.html`, embedded in the binary).

```
                 ┌────────────────────────── padscreen ───────────────────────────┐
 iPad Safari     │  server.rs        session.rs            capture.rs              │
 ┌──────────┐ WS │  HTTPS + WS  ──▶  supervisor  ──────▶  gst-launch-1.0 child     │
 │ WebCodecs│◀───┤  (axum)           (Wayland: mutter.rs)  pipewiresrc ▶ encoder   │
 │ canvas   │    │     ▲  │                │                      │ H.264 Annex-B │
 │ touch    │───▶│     │  └─ input.rs ◀────┘                      ▼               │
 └──────────┘    │  settings.rs     mutter / display      h264.rs: access units    │
                 └──────────────────────────────────────────────────────────────────┘
```

## Modules

| Module | Role |
|---|---|
| `main.rs` | CLI and environment (`clap` with `env`, `.env` files), TLS setup, wiring. |
| `server.rs` | axum app. `guard` middleware refuses peers outside loopback / USB link and adds security headers. `/` serves the page, `/ws` the stream and control socket, `/ca.crt` the public CA, `/metrics` counters. Fans encoded frames out through a broadcast channel. |
| `session.rs` | Supervisor task. Owns the virtual monitor and the capture child; reacts to client connect/disconnect, size and settings messages. Debounces resizes, restarts capture with backoff, tears everything down after the idle timeout. |
| `mutter.rs` | GNOME Wayland backend over D-Bus (`zbus`): a ScreenCast `RecordVirtual` stream for the monitor, the RemoteDesktop session for pointer and touch input, and DisplayConfig calls that scale and place the monitor. |
| `display.rs` | X11 backend: RandR mode, output and CRTC handling on a spare output (`x11rb`). |
| `capture.rs` | Builds the `gst-launch-1.0` (Wayland) or `ffmpeg` (X11) command line for each encoder. |
| `settings.rs` | The knobs a client can change (resolution, UI scale, profile, placement), their defaults, and how a message patches them. |
| `input.rs` | Parses client input events and injects them: Mutter RemoteDesktop on Wayland, XTEST on X11. |
| `h264.rs` | Splits the encoder's Annex-B byte stream into access units, converts to AVCC, and derives the WebCodecs codec string. |
| `net.rs` | Finds the laptop's address on the USB link; decides who counts as a trusted peer. |

## Data flow

1. The iPad opens `https://<laptop>:8443/` and the page opens a WebSocket. Its first message is `hello` with the
   screen size in device pixels, the pixel ratio, and any saved settings.
2. The supervisor creates the virtual monitor: Mutter makes a monitor of exactly the size the capture negotiates,
   named `Meta-N`, positioned beside the laptop screen. A `gst-launch-1.0` child reads the PipeWire stream, paces
   it to a constant frame rate, converts, encodes (NVENC, VA-API or x264, no B-frames, repeated SPS/PPS) and writes
   Annex-B to stdout.
3. `h264.rs` cuts that into access units. Every frame is sent as a binary WebSocket message
   `[flags: u8, bit0 = key][server time µs: u64 BE][AVCC payload]`; the decoder configuration goes first as a JSON
   text message (`codec`, base64 `description`, size, fps).
4. The page feeds a `VideoDecoder` and draws to a canvas. Clients that fall behind wait for the next keyframe
   instead of queueing.
5. Touch, Pencil and mouse events go back as small JSON text messages, in stream pixels. On Wayland they become
   RemoteDesktop touch and pointer calls on the same session as the virtual monitor.

## Messages

Client → server (text JSON):

| `type` | Fields |
|---|---|
| `hello` | `pw`, `ph` (device pixels), `dpr`, optional `res`, `ui`, `profile`, `place` |
| `resize` | `pw`, `ph` after a rotation or window change |
| `settings` | any of `res` (0.25–1), `ui` (1–3), `profile` (`latency` \| `balanced` \| `quality` \| `saver`), `place` (`right` \| `left` \| `above` \| `below`) |
| `in` | pointer: `e` = `move` \| `down` \| `up` \| `scroll`; touch: `td` \| `tm` \| `tu` with a slot `id`; coordinates normalised to the stream |

Server → client: `caps` (what the backend supports, the defaults, the profile table), `config` (decoder
configuration), then binary frames. Messages from clients are capped at 16 KiB.

## Settings and what they cost

| Setting | Applied by | Restarts the stream |
|---|---|---|
| `ui`, `place` | DisplayConfig `ApplyMonitorsConfig` on the live monitor | no |
| `res`, `profile` | new capture size / encoder parameters | yes (new virtual monitor) |

The monitor layout follows mutter's rules: in physical layout mode a monitor occupies its pixel size in layout
space whatever its UI scale, and monitors must touch. `mutter.rs::plan_layout` computes the positions for the four
sides and keeps the other monitors where they are.

## Threading

An async runtime (`tokio`) runs the server, the supervisor and the D-Bus calls. X11 input uses a blocking thread.
Capture children are killed on drop, so a crash of the daemon does not leave encoders running.

## Tests

- Unit tests for parsing, layout planning, access-unit assembly, origin checks and settings.
- `tests/e2e.rs`: synthetic source, software encode, decode check over a real WebSocket.
- `tests/managed_wayland.rs` (ignored): real GNOME session. Virtual monitor lifecycle, both encoders, touch
  injection, and `wayland_layout` for scale and the four placements.
- `tests/managed_x11.rs`, `tests/display_x11.rs` (ignored): the X11 backend on a real X server.
