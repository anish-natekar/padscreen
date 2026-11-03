# padscreen

Use a Wi-Fi-only iPad as a real extended monitor for a Linux laptop, over a USB-C cable.

padscreen adds a virtual monitor to your desktop, encodes it as H.264 on the GPU, and streams it to Safari on
the iPad, which decodes it with WebCodecs. Touch and Apple Pencil come back as native multitouch. There is
nothing to install on the iPad: you open a web page, and can add it to the Home Screen.

```
laptop (GNOME on Wayland)                         iPad (Safari)
┌────────────────────────────────┐   USB-C cable  ┌──────────────────────┐
│ virtual monitor → PipeWire     │  (USB network, │ WebCodecs H.264      │
│ → GStreamer → NVENC/VAAPI/x264 │ ─────────────▶ │ decoder → canvas     │
│ → padscreen (HTTPS + WebSocket)│ ◀───────────── │ touch / Pencil input │
└────────────────────────────────┘                └──────────────────────┘
```

## Features

- Extended desktop, not a mirror: the iPad shows up in GNOME's display settings as another monitor.
- Hardware H.264 encoding with NVENC (NVIDIA) or VA-API (AMD/Intel), x264 as the software fallback.
- Settings on the iPad: UI scale (so text is readable on a 264 ppi screen), streamed resolution, a
  latency / balanced / quality / saver profile, which side of the laptop screen the iPad sits on, and a
  show/hide switch for the stats overlay.
- Finger and Pencil are true multitouch devices on Wayland. A mouse on the iPad stays a pointer.
- Rotates with the iPad and resizes the virtual monitor to match.
- The virtual monitor goes away when the iPad disconnects.
- Runs as a systemd user service with an app-grid launcher.

## Requirements

| | |
|---|---|
| Laptop | Ubuntu 24.04 (or similar) with GNOME 46 on **Wayland** (the main path). An X11 backend using a spare RandR output also exists but is less polished. |
| iPad | Safari with WebCodecs (iPadOS 16.4 or newer). Developed and tested on a 2160×1620 iPad running iPadOS 26. A Wi-Fi-only model is fine: the USB cable carries a network link, not Wi-Fi. |
| Cable | USB-C to USB-C (or USB-A) that carries data. |
| Packages | `gstreamer1.0-tools gstreamer1.0-pipewire gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly pipewire openssl network-manager` plus `ffmpeg` for the X11 backend |
| Build | Rust 1.85 or newer (`rustup`) |
| GPU encode | NVIDIA: the `nvcodec` GStreamer plugin (in `plugins-bad`). AMD/Intel: the `va` plugin. Check with `gst-inspect-1.0 nvh264enc` or `vah264enc`. Without either, x264 is used. |

## Quick start

```bash
git clone https://github.com/anish-natekar/padscreen.git
cd padscreen
```

1. **Make the iPad reachable over USB.** Follow [docs/SETUP.md](docs/SETUP.md#1-usb-network-link). In short: build a
   newer `usbmuxd` once with `scripts/setup-usbmuxd.sh`, plug the iPad in, tap *Trust*, and set the two new wired
   connections to *Shared to other computers*. The laptop then has an address like `10.42.1.1` on the cable.
2. **Install padscreen** (builds it, makes certificates, installs the service and launcher):
   ```bash
   scripts/install.sh
   ```
3. **Trust the certificate on the iPad, once.** Safari only allows WebCodecs on HTTPS, so the iPad has to trust
   padscreen's private CA. Open `https://10.42.1.1:8443/ca.crt` on the iPad, install the profile, then enable it
   under *Settings → General → About → Certificate Trust Settings*. Details in
   [docs/SETUP.md](docs/SETUP.md#3-trust-the-certificate-on-the-ipad).
4. **Open `https://10.42.1.1:8443/` in Safari on the iPad** (the address is printed in the log and by the launcher).
   Use *Share → Add to Home Screen* for a full-screen app.

The iPad now appears as a monitor to the right of the laptop screen. Drag windows onto it. The ⚙ button on the iPad
opens its settings.

## Running without installing

```bash
cargo run --release -- serve
```

Settings come from flags, environment variables, or a `.env` file. Copy [`.env.example`](.env.example) to `.env`
and edit it; the file is git-ignored. `padscreen serve --help` lists every option. Precedence, highest first:
flags, real environment variables, `./.env`, `~/.config/padscreen/padscreen.env`, built-in defaults.

## Daily use

The launcher *padscreen* in the app grid starts the service and shows the iPad's address. Right-click it for Stop,
Restart and Status. From a terminal:

```bash
padscreenctl status      # also: start | stop | restart
journalctl --user -u padscreen -f
```

To stop it starting at login: `systemctl --user disable --now padscreen`.

## Settings on the iPad

| Setting | What it does |
|---|---|
| Preset | One tap for common combinations: *Sharp, large UI*, *Sharp, small UI*, *Fast*, *Battery*. |
| Resolution | 25–100 % of the iPad's pixels. Lower is lighter on the encoder and the cable. Restarts the stream. |
| UI size | 1× or 2× scale of the virtual monitor, so menus and text are the size you want. Applies live. |
| Position | Which side of the laptop screen the iPad sits on. Applies live. |
| Speed vs quality | `latency` (smallest buffers), `balanced`, `quality` (highest bitrate), `saver` (30 fps, low bitrate). Restarts the stream. |
| Stats | Shows or hides the overlay at the top left (fps, bitrate, latency). Tap the overlay to hide it. |

The server's defaults for these come from the environment (`PADSCREEN_UI_SCALE`, `PADSCREEN_LAYOUT`,
`PADSCREEN_PROFILE`, `PADSCREEN_RES`); the iPad remembers its own choices.

## Security

padscreen streams your screen and lets the client move your pointer, so by default it only serves **loopback and
the USB cable**. Wi-Fi/LAN peers get `403`. The WebSocket also rejects cross-site pages. `--allow-lan` turns
that off and is almost never what you want. See [SECURITY.md](SECURITY.md).

## Troubleshooting

| Symptom | Check |
|---|---|
| Safari says the page is not private / cannot connect | The CA is not installed *and enabled* under Certificate Trust Settings, or you opened an address that is not in the certificate. Re-run `scripts/gen-cert.sh <extra-ip>` for other addresses. |
| Black page, "connecting…" | `systemctl --user status padscreen`; is the iPad address one of those the log prints? |
| Everything is tiny on the iPad | Raise *UI size* in ⚙ (default 2×). |
| The monitor appears but stays black | `gst-inspect-1.0 pipewiresrc` must find the element and PipeWire must be running (`systemctl --user status pipewire`). |
| No hardware encoder | `gst-inspect-1.0 nvh264enc` / `vah264enc`; otherwise the log says `encoder=X264`. |
| Windows jump back to the laptop after changing resolution or speed | Expected: those two settings restart the stream and recreate the virtual monitor. |
| Address of the laptop on the cable changed | It can change per plug; the log and `padscreenctl status` print the current one. |

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                         # unit tests and a software-encode end-to-end test
cargo test --test managed_wayland -- --ignored     # needs a real GNOME Wayland session
```

The ignored tests create a virtual monitor and, for `wayland_layout`, rearrange your displays for a few
seconds, so run them in a session where you have saved your work. Architecture notes are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); contribution notes in [CONTRIBUTING.md](CONTRIBUTING.md).

## Limitations

- GNOME on Wayland is the supported path (it relies on Mutter's ScreenCast and RemoteDesktop D-Bus APIs).
  KDE and wlroots compositors are not supported.
- No audio, no keyboard passthrough, no Pencil pressure or tilt yet.
- Fractional UI scaling needs a GNOME experimental feature and is not used; scales are 1× and 2×.
- Changing the resolution or the speed profile restarts the stream.
- One iPad at a time is the intended use. A second client shares the same stream and the same virtual monitor,
  and both can send input.
- The page's Content-Security-Policy and the WebCodecs API need a recent Safari (iPadOS 16.4 or newer).

## License

[MIT](LICENSE)
