# Security

padscreen shows a screen to a browser and takes mouse and touch input from it, so anything that can talk to it
can look at and drive your desktop. It is built so that, by default, only your own iPad on the USB cable can.

## What it does about that

- **Peer filter.** Every request is checked against the connection's source address. Only loopback and the
  `/24` of an attached USB network link (CDC-NCM, `ipheth`, and similar drivers) are served. Everything else gets
  `403`, including peers on Wi-Fi and Ethernet, even though the socket listens on `0.0.0.0`. `--allow-lan`
  disables this; do not use it on networks you do not control.
- **No cross-site control.** Browsers send an `Origin` header on WebSocket handshakes. A handshake whose origin
  is not the page's own host and port is refused, so a web page open on the laptop cannot connect to
  `localhost:8443` and move the pointer.
- **TLS.** The page is HTTPS-only (required for WebCodecs). The certificate comes from a private CA that
  `scripts/gen-cert.sh` limits to private address ranges with name constraints. Keys are written with mode 600 and
  the server warns if its key file is readable by others.
- **Hardening headers.** A strict Content-Security-Policy, `nosniff`, `no-referrer`, `no-store`.
- **Bounded input.** Client messages are capped at 16 KiB, settings and sizes are clamped, input slots are
  tracked so malformed sequences cannot wedge the injector.
- **View-only mode.** `PADSCREEN_NO_INPUT=1` (or `--no-input`) drops all input from the iPad.
- **No secrets in the repository.** Settings and keys live in `~/.config/padscreen` or a git-ignored `.env`.

## What it does not do

- There is no user login. Trust comes from physical access to the cable and from the iPad trusting your CA. Anyone
  you let onto the USB network segment (for example another device plugged into the same shared link) is treated as
  the iPad.
- Traffic on the cable is protected by TLS, but the stream is plain pixels of what is on that monitor. Do not put
  secrets on it while someone else holds the iPad.
- The `scripts/setup-usbmuxd.sh` helper builds third-party projects from their default branches with `sudo`. Read it
  before running it.

## Reporting a vulnerability

Please do not open a public issue for security problems. Use GitHub's private vulnerability reporting on the
repository (Security → Report a vulnerability), or contact the maintainer privately. Include the version, what you
saw, and how to reproduce it. A fix and an advisory will follow as quickly as is practical.
