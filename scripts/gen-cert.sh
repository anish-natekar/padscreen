#!/usr/bin/env bash
# Make a private CA and a server certificate for padscreen. Safari's WebCodecs only works on HTTPS, so the
# iPad has to trust the server: install ca.crt on it once (see README), then the server uses leaf.crt/leaf.key.
#
# usage: scripts/gen-cert.sh [EXTRA_IP ...]
#   Files go to $PADSCREEN_CONFIG_DIR (default ~/.config/padscreen). Existing files are never overwritten;
#   delete them first to re-issue. The CA is limited to private address ranges, so even a leaked ca.key
#   cannot be used to impersonate a public website.
set -euo pipefail

dir=${PADSCREEN_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/padscreen}
days=800  # Apple rejects server certificates valid for much longer than 825 days
mkdir -p "$dir"
chmod 700 "$dir"
for f in ca.key ca.crt leaf.key leaf.crt; do
  if [ -e "$dir/$f" ]; then
    echo "$dir/$f already exists; remove ca.* and leaf.* to re-issue" >&2
    exit 1
  fi
done

# NetworkManager's "shared" mode hands the laptop 10.42.N.1; list the usual ones plus loopback and anything given.
sans="DNS:localhost,IP:127.0.0.1"
for n in 0 1 2 3 4 5; do sans+=",IP:10.42.$n.1"; done
for ip in "$@"; do sans+=",IP:$ip"; done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
umask 077

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$dir/ca.key"
openssl req -x509 -new -key "$dir/ca.key" -sha256 -days 3650 -subj "/CN=padscreen local CA" -out "$dir/ca.crt" \
  -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -addext "nameConstraints=critical,permitted;DNS:localhost,permitted;IP:127.0.0.0/255.0.0.0,permitted;IP:10.0.0.0/255.0.0.0,permitted;IP:172.16.0.0/255.240.0.0,permitted;IP:192.168.0.0/255.255.0.0"

openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$dir/leaf.key"
openssl req -new -key "$dir/leaf.key" -subj "/CN=padscreen" -out "$tmp/leaf.csr"
cat >"$tmp/ext.cnf" <<EOF
subjectAltName=$sans
extendedKeyUsage=serverAuth
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
EOF
openssl x509 -req -in "$tmp/leaf.csr" -CA "$dir/ca.crt" -CAkey "$dir/ca.key" -CAcreateserial -CAserial "$tmp/ca.srl" \
  -sha256 -days "$days" -extfile "$tmp/ext.cnf" -out "$dir/leaf.crt"

openssl verify -CAfile "$dir/ca.crt" "$dir/leaf.crt"
chmod 600 "$dir"/*.key
chmod 644 "$dir"/*.crt
echo
echo "Made in $dir:"
echo "  ca.crt            public; the iPad installs this (served by padscreen at /ca.crt)"
echo "  leaf.crt/leaf.key the server's certificate"
echo "  ca.key            private; keep it here, never share it"
