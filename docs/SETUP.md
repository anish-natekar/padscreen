# Setup

Four one-time steps. Everything lives in your user account except the `usbmuxd` build in step 1.

## 1. USB network link

A Wi-Fi-only iPad has no way to listen on a USB connection from a web page, so padscreen uses the iPad's *USB
tethering* personality (CDC-NCM): the cable becomes a small Ethernet network between the laptop and the iPad, and
Safari opens a page served by the laptop.

Ubuntu's `usbmuxd` package cannot switch the iPad into that mode; a recent build can.

```bash
scripts/setup-usbmuxd.sh
```

The script installs build dependencies with `apt`, builds `libplist`, `libimobiledevice-glue`, `libusbmuxd`,
`libtatsu`, `libimobiledevice` and `usbmuxd` from the upstream `libimobiledevice` GitHub projects into
`/usr/local`, and adds a systemd drop-in that starts `usbmuxd` with `USBMUXD_DEFAULT_DEVICE_MODE=3`. It uses `sudo`.
It builds each project's default branch, so read it first, and consider pinning tags if you want reproducibility.

Then:

1. Plug the iPad in and tap **Trust** on it.
2. Two new wired connections appear in NetworkManager (named like `Wired connection 2` and `3`; one of them
   carries the traffic, and which one changes per plug). Set both to share the connection:
   ```bash
   nmcli connection modify "Wired connection 2" ipv4.method shared
   nmcli connection modify "Wired connection 3" ipv4.method shared
   ```
   (or *Settings → Network → the wired connection → IPv4 → Share to other computers*), then unplug and replug.
3. Check that the laptop got an address in `10.42.x.1`:
   ```bash
   ip -4 -br addr | grep 10.42
   ```
   and that the iPad answers: its address shows up in `ip neigh`, and `ping` on it should reply in a few ms.

NetworkManager's shared mode runs a small DHCP server, which is why the iPad gets an address by itself. Shared mode also NATs the
iPad onto the laptop's uplink, so the iPad gets internet through the cable as a side effect.

## 2. Install padscreen

```bash
scripts/install.sh
```

This builds the release binary and installs:

| What | Where |
|---|---|
| `padscreen`, `padscreenctl` | `~/.local/bin` |
| Certificates (made on first run) and `padscreen.env` | `~/.config/padscreen/` |
| systemd user unit, enabled for your graphical session | `~/.config/systemd/user/padscreen.service` |
| App-grid launcher and icon | `~/.local/share/applications`, `~/.local/share/icons` |

`scripts/install.sh --no-start` installs without starting the service. To remove everything:
`systemctl --user disable --now padscreen`, then delete those files.

### Certificates

`scripts/gen-cert.sh` creates a private CA (`ca.crt`, `ca.key`) and a server certificate (`leaf.crt`, `leaf.key`),
all with P-256 keys, in `~/.config/padscreen/`. The CA is restricted to private address ranges (name
constraints), so it cannot vouch for a public website even if `ca.key` leaked. The server certificate is valid for
`localhost`, `127.0.0.1` and `10.42.0.1` to `10.42.5.1`. If your laptop's address on the cable is different, or
you want a LAN address too, delete the four files and run `scripts/gen-cert.sh 192.168.1.20` (any extra IPs).
Then repeat step 3 for the new CA.

## 3. Trust the certificate on the iPad

Safari exposes WebCodecs only on secure origins, and a self-signed certificate is not enough for a trusted
connection, so the iPad must trust the CA once.

1. In Safari on the iPad open `https://10.42.1.1:8443/ca.crt` (use your address). Safari warns about the
   certificate; continue anyway for this one page. Allow the profile download.
2. *Settings → General → VPN & Device Management → padscreen local CA → Install.*
3. *Settings → General → About → Certificate Trust Settings →* switch on **padscreen local CA**.

Reload `https://10.42.1.1:8443/` and the warning is gone. Use *Share → Add to Home Screen* to run it full screen.

The CA is yours alone, and its key never leaves your laptop. Remove the profile again in *VPN & Device
Management* whenever you like.

## 4. Configure (optional)

Everything has a default. To change one, put it in `~/.config/padscreen/padscreen.env` (service) or `./.env`
(running from a checkout). The names and meanings are in [`.env.example`](../.env.example). After editing:

```bash
padscreenctl restart
```

## Without GNOME Wayland

On an X11 session the backend defaults to RandR: padscreen needs a spare, disconnected output (for example
`DisplayPort-0`) to turn into the virtual monitor, and `ffmpeg` for capture. Choose it with
`PADSCREEN_BACKEND=x11`, and an output with `PADSCREEN_OUTPUT=...`. UI scaling is a Wayland feature; the X11
backend places the monitor from `PADSCREEN_LAYOUT` and `PADSCREEN_OFFSET`.
