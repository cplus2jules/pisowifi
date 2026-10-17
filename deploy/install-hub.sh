#!/usr/bin/env bash
# Installs the Piso Pass hub on a Linux server, or upgrades it. From the
# unpacked bundle:
#   sudo ./install-hub.sh
# An upgrade keeps hub.toml and the database, and puts the old version back
# by itself if the new one doesn't come up.
set -euo pipefail

cd "$(dirname "$0")"
say()  { printf '\n\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }
ask()  { local a; read -r -p "$1 [y/N] " a; [[ "$a" =~ ^[Yy] ]]; }

[[ $EUID -eq 0 ]] || { echo "Run as root: sudo ./install-hub.sh"; exit 1; }
[[ -x ./pisowifi ]] || { echo "pisowifi binary not found next to install-hub.sh"; exit 1; }
./pisowifi --version >/dev/null || { echo "The binary doesn't run on this server."; exit 1; }
NEW="$(./pisowifi --version | awk '{print $2}')"
CONF=/etc/pisowifi/hub.toml
LIB=/usr/local/lib/pisowifi
UNITS=(pisowifi-hub.service pisowifi-hub-update.service)

# ---- Upgrade ---------------------------------------------------------------------
if [[ -f "$CONF" && -e /usr/local/bin/pisowifi ]] && systemctl cat pisowifi-hub.service >/dev/null 2>&1; then
  OLD="$(/usr/local/bin/pisowifi --version 2>/dev/null | awk '{print $2}' || true)"
  echo "Installed: ${OLD:-unknown}    This bundle: $NEW"
  ALLOW=()
  if [[ -n "$OLD" && "$OLD" == "$NEW" ]]; then
    ask "Version $NEW is already installed. Reinstall it?" || exit 1
    ALLOW=(--allow-same)
  elif [[ -n "$OLD" && "$(printf '%s\n%s\n' "$OLD" "$NEW" | sort -V | tail -1)" == "$OLD" ]]; then
    echo "This bundle ($NEW) is older than the installed $OLD. Going back to an older version isn't supported."
    exit 1
  else
    ask "Upgrade the hub to $NEW? Vendos keep working on their own for the minute it takes." || exit 1
  fi
  # Runs from a root-only copy, detached from this SSH session.
  INC="$LIB/incoming"
  install -d -m 0755 "$LIB"
  rm -rf -- "${INC:?}"
  install -d -m 0700 "$INC"
  install -m 0755 ./pisowifi "$INC/"
  for f in "${UNITS[@]}" README.md; do
    if [[ -f "$f" ]]; then install -m 0644 "$f" "$INC/"; fi
  done
  UNIT="pisowifi-hub-upgrade-$(date +%s)"
  say "Upgrading to $NEW"
  journalctl -f -n0 -o cat -u "$UNIT" 2>/dev/null &
  JPID=$!
  RC=0
  systemd-run --quiet --wait --collect --unit "$UNIT" -p Type=oneshot -p TimeoutStartSec=15min \
    "$INC/pisowifi" hub --hub-config "$CONF" update --install-dir "$INC" ${ALLOW[@]+"${ALLOW[@]}"} || RC=$?
  sleep 1
  kill "$JPID" 2>/dev/null || true
  rm -rf -- "${INC:?}"
  if ((RC != 0)); then
    warn "The upgrade didn't go through:"
    /usr/local/bin/pisowifi hub --hub-config "$CONF" update --status || true
    exit 1
  fi
  say "Upgraded the hub to $NEW."
  exit 0
fi

# ---- First install ---------------------------------------------------------------
say "Installing the Piso Pass hub $NEW"
# Same layout as the vendos: releases/<version>, `current`, `previous`.
REL="releases/$NEW-$(date +%s)"
install -d -m 0755 "$LIB" "$LIB/releases" "$LIB/$REL" /etc/pisowifi
install -m 0755 ./pisowifi "$LIB/$REL/pisowifi"
for f in "${UNITS[@]}" README.md; do
  if [[ -f "$f" ]]; then install -m 0644 "$f" "$LIB/$REL/"; fi
done
ln -sfn "$REL" "$LIB/current"
ln -sfn "$LIB/current/pisowifi" /usr/local/bin/pisowifi
if [[ -f "$CONF" ]]; then echo "Keeping existing $CONF"; else install -m 0644 ./hub.example.toml "$CONF"; fi
install -m 0644 "${UNITS[@]}" /etc/systemd/system/
systemctl daemon-reload
systemctl enable -q pisowifi-hub-update.service
systemctl enable --now pisowifi-hub
sleep 2
systemctl --no-pager --lines=5 status pisowifi-hub || true

cat <<EOF

  The hub is running ($CONF).
  Next:     HTTPS with Caddy (see Caddyfile), then on this server:
            sudo pisowifi hub add-vendo --name "Barangay A"
  Updates:  sudo pisowifi hub update --file pisowifi-hub-x86_64.tar.gz
            (its .manifest next to it), or run this script from a newer bundle.

EOF
