#!/usr/bin/env bash
# Installs or upgrades baocode-relay on a Linux VPS (systemd). Run as root
# from an unpacked release (baocode-relay-<version>-<arch>.tar.gz), which
# has the binary next to this deploy/ directory:
#
#   sudo ./deploy/install.sh relay-hk1.baocode.dev ops@baocode.dev
#
# Again with a newer release to upgrade: config, bans and certificates are
# kept.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then echo "run as root" >&2; exit 1; fi
if [[ $# -lt 2 ]]; then
  echo "usage: $0 <hostname> <contact-email>" >&2
  exit 1
fi
HOSTNAME_=$1
CONTACT=$2
HERE=$(cd "$(dirname "$0")" && pwd)
BINARY=${BINARY:-$HERE/../baocode-relay}
[[ -x $BINARY ]] || { echo "no binary at $BINARY (set BINARY=...)" >&2; exit 1; }

echo "== user and directories"
id baocode-relay >/dev/null 2>&1 ||
  useradd --system --no-create-home --shell /usr/sbin/nologin baocode-relay
install -d -m 0755 /etc/baocode-relay
install -d -m 0750 -o baocode-relay -g baocode-relay /var/lib/baocode-relay

echo "== binary"
install -m 0755 "$BINARY" /usr/local/bin/baocode-relay.new
mv -f /usr/local/bin/baocode-relay.new /usr/local/bin/baocode-relay
/usr/local/bin/baocode-relay --version

echo "== config"
if [[ ! -f /etc/baocode-relay/relay.toml ]]; then
  sed -e "s/^hostname = .*/hostname = \"$HOSTNAME_\"/" \
      -e "s/^contact = .*/contact = \"$CONTACT\"/" \
      "$HERE/relay.toml" > /etc/baocode-relay/relay.toml
  echo "wrote /etc/baocode-relay/relay.toml"
else
  echo "kept /etc/baocode-relay/relay.toml"
fi
[[ -f /etc/baocode-relay/banned.txt ]] || install -m 0644 "$HERE/banned.txt" /etc/baocode-relay/banned.txt

echo "== kernel buffers for QUIC"
cat > /etc/sysctl.d/90-baocode-relay.conf <<'EOF'
net.core.rmem_max = 7500000
net.core.wmem_max = 7500000
EOF
sysctl -q --system || true

echo "== firewall"
if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  ufw allow 80/tcp; ufw allow 443/tcp; ufw allow 7842/udp
elif command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port=80/tcp --add-port=443/tcp --add-port=7842/udp
  firewall-cmd --reload
else
  echo "no active ufw/firewalld; make sure TCP 80, TCP 443, UDP 7842 are open (cloud security group too)"
fi

echo "== service"
install -m 0644 "$HERE/baocode-relay.service" /etc/systemd/system/baocode-relay.service
systemctl daemon-reload
systemctl enable baocode-relay >/dev/null
systemctl restart baocode-relay
sleep 3
systemctl --no-pager --lines=15 status baocode-relay || true

echo "== checks"
if curl -fsS http://127.0.0.1:9090/metrics | grep -q baocode_relay_admitted; then
  echo "metrics: ok"
else
  echo "metrics: not answering yet (see journalctl -u baocode-relay)"
fi
echo "certificate: the first one comes from Let's Encrypt within a minute;"
echo "  then  curl -fsS https://$HOSTNAME_/healthz  should answer from anywhere."
