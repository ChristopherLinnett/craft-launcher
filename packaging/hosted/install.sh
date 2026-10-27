#!/usr/bin/env bash
# Install or update ArtCraft Launcher's hosted mode on this Linux server, as a systemd service.
# Run as root (packaging/hosted/deploy.sh does this over SSH).
#
# Env:
#   STAGING        folder holding the new bin/artcraft-launcher-host and launcher/ (default: here)
#   ROOT           install folder: bin/, launcher/, data/ (default /opt/artcraft-launcher-host)
#   SERVICE        systemd service name (default artcraft-launcher-host)
#   RUN_USER       account the server runs as; must exist (default artcraft)
#   LISTEN         address to serve on (default 127.0.0.1:8120). Put HTTPS in front of it.
#   HOST_PASSWORD  the team password; required on the first install only
set -euo pipefail

STAGING="${STAGING:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
ROOT="${ROOT:-/opt/artcraft-launcher-host}"
SERVICE="${SERVICE:-artcraft-launcher-host}"
RUN_USER="${RUN_USER:-artcraft}"
LISTEN="${LISTEN:-127.0.0.1:8120}"
BIN="$ROOT/bin/artcraft-launcher-host"

die() {
  echo "install.sh: $*" >&2
  exit 1
}

[[ "$(id -u)" == 0 ]] || die "run as root"
id "$RUN_USER" >/dev/null 2>&1 || die "no account called $RUN_USER"
[[ -x "$STAGING/bin/artcraft-launcher-host" ]] || die "$STAGING/bin/artcraft-launcher-host is missing"
[[ -f "$STAGING/launcher/index.html" ]] || die "$STAGING/launcher/index.html is missing"

install -d -m 0755 "$ROOT" "$ROOT/bin"
install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$ROOT/data"

# The binary and the launcher's web build, each swapped in by a rename.
install -m 0755 "$STAGING/bin/artcraft-launcher-host" "$BIN.new"
mv -f "$BIN.new" "$BIN"
rm -rf "$ROOT/launcher.new" "$ROOT/launcher.old"
cp -R "$STAGING/launcher" "$ROOT/launcher.new"
chmod -R a+rX "$ROOT/launcher.new"
if [[ -d "$ROOT/launcher" ]]; then mv "$ROOT/launcher" "$ROOT/launcher.old"; fi
mv "$ROOT/launcher.new" "$ROOT/launcher"
rm -rf "$ROOT/launcher.old"

if [[ ! -f "$ROOT/data/config.json" ]]; then
  [[ -n "${HOST_PASSWORD:-}" ]] || die "HOST_PASSWORD is required on the first install"
  printf '%s\n' "$HOST_PASSWORD" | runuser -u "$RUN_USER" -- "$BIN" set-password --data "$ROOT/data"
fi

cat >"/etc/systemd/system/$SERVICE.service" <<EOF
[Unit]
Description=ArtCraft Launcher host ($SERVICE): the Craft Apps' browser builds for a team
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$RUN_USER
Group=$RUN_USER
ExecStart=$BIN serve --data $ROOT/data --launcher $ROOT/launcher --listen $LISTEN
Restart=always
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=$ROOT/data

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable "$SERVICE" >/dev/null 2>&1
systemctl restart "$SERVICE"
sleep 1
systemctl is-active "$SERVICE" >/dev/null || {
  journalctl -u "$SERVICE" -n 20 --no-pager >&2
  die "$SERVICE didn't start"
}
echo "$SERVICE is running on $LISTEN"
