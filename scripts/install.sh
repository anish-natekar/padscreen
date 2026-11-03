#!/usr/bin/env bash
# Install padscreen for the current user: binary in ~/.local/bin, certificates and settings in ~/.config/padscreen,
# systemd user unit that starts with the graphical session, and an app-grid launcher.
# usage: scripts/install.sh [--no-start]
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
bin=$HOME/.local/bin
cfg=$HOME/.config/padscreen
unit=$HOME/.config/systemd/user

cargo build --release --manifest-path "$root/Cargo.toml"
install -Dm755 "$root/target/release/padscreen" "$bin/padscreen"

mkdir -p "$cfg"
chmod 700 "$cfg"
if [ ! -e "$cfg/leaf.crt" ]; then
  "$root/scripts/gen-cert.sh"
fi
if [ ! -e "$cfg/padscreen.env" ]; then
  install -m600 "$root/.env.example" "$cfg/padscreen.env"
  echo "wrote $cfg/padscreen.env (all commented out); edit it to change defaults"
fi

install -Dm755 "$root/packaging/padscreenctl" "$bin/padscreenctl"
install -Dm644 "$root/packaging/padscreen.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/padscreen.svg"
install -Dm644 "$root/packaging/padscreen.desktop" "$HOME/.local/share/applications/padscreen.desktop"
if command -v update-desktop-database >/dev/null; then update-desktop-database "$HOME/.local/share/applications" || true; fi
if command -v gtk-update-icon-cache >/dev/null; then gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" || true; fi

install -Dm644 "$root/packaging/padscreen.service" "$unit/padscreen.service"
systemctl --user daemon-reload
systemctl --user enable padscreen.service
if [ "${1:-}" != "--no-start" ]; then
  systemctl --user restart padscreen.service
  sleep 1
  systemctl --user --no-pager status padscreen.service | head -12
fi
echo "logs: journalctl --user -u padscreen -f"
