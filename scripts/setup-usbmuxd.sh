#!/usr/bin/env bash
# Build usbmuxd from git into /usr/local and run it in device mode 3, which exposes the iPad as a USB network
# adapter (CDC-NCM). Ubuntu's usbmuxd package is too old to do this.
# Run as your user; it uses sudo to install packages and files under /usr/local and /etc/systemd.
# It builds the current default branch of the libimobiledevice projects: read it before running it.
set -euo pipefail
sudo apt-get install -y build-essential git autoconf automake libtool-bin pkg-config \
  libusb-1.0-0-dev libudev-dev libssl-dev libcurl4-openssl-dev libreadline-dev libxml2-dev libzip-dev python3-dev cython3
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig LD_LIBRARY_PATH=/usr/local/lib
W=$(mktemp -d); cd "$W"
for r in libplist libimobiledevice-glue libusbmuxd libtatsu libimobiledevice usbmuxd; do
  git clone --depth 1 "https://github.com/libimobiledevice/$r"; (cd $r
  ./autogen.sh --prefix=/usr/local --without-cython >/dev/null; make -j"$(nproc)" >/dev/null; sudo make install >/dev/null); echo "built $r"
done
sudo ldconfig
sudo mkdir -p /etc/systemd/system/usbmuxd.service.d
sudo tee /etc/systemd/system/usbmuxd.service.d/ncm.conf >/dev/null <<'U'
[Service]
Environment=USBMUXD_DEFAULT_DEVICE_MODE=3
ExecStart=
ExecStart=/usr/local/sbin/usbmuxd --user usbmux --systemd
U
sudo systemctl daemon-reload && sudo systemctl restart usbmuxd
echo "now plug iPad, tap Trust, then: ip -br link | grep -iE 'enx|usb'; nmcli dev"
