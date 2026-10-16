#!/usr/bin/env bash
# Installs pisowifi on an Orange Pi running Armbian, replacing LPB.
# Run from the unpacked bundle:  sudo ./install.sh
# Nothing from LPB is deleted: its services are disabled and its configs are
# copied to /root/lpb-backup-<date>/ so you can roll back.
set -euo pipefail

cd "$(dirname "$0")"
BACKUP="/root/lpb-backup-$(date +%Y%m%d-%H%M%S)"
LAN_IF="${LAN_IF:-eth1}"
LAN_IP="${LAN_IP:-10.0.0.1}"

say()  { printf '\n\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }
ask()  { local a; read -r -p "$1 [y/N] " a; [[ "$a" =~ ^[Yy] ]]; }

[[ $EUID -eq 0 ]] || { echo "Run as root: sudo ./install.sh"; exit 1; }
[[ "$(uname -m)" == armv7l || "$(uname -m)" == armv6l || "$(uname -m)" == aarch64 ]] || warn "Unexpected CPU $(uname -m); this bundle is built for ARMv7."
[[ -x ./pisowifi ]] || { echo "pisowifi binary not found next to install.sh"; exit 1; }

cat <<'EOF'

  Piso WiFi installer
  -------------------
  Before continuing, make a full copy of the SD card (e.g. with balenaEtcher
  or `dd` on another computer). That is your guaranteed way back to LPB.

EOF
ask "I have a backup of the SD card. Continue?" || exit 1

./pisowifi --version >/dev/null || { echo "The binary doesn't run on this board."; exit 1; }

# ---- 1. LAN address ---------------------------------------------------------
say "Checking $LAN_IF has $LAN_IP"
if ip -4 addr show "$LAN_IF" 2>/dev/null | grep -q "inet $LAN_IP/"; then
  echo "OK: $(ip -4 -o addr show "$LAN_IF" | awk '{print $4}')"
else
  warn "$LAN_IF doesn't have $LAN_IP. LPB normally sets this up; if you changed it,"
  warn "configure $LAN_IF as $LAN_IP/19 (armbian-config > Network) and run this again."
  ask "Continue anyway?" || exit 1
fi

# ---- 2. Packages ------------------------------------------------------------
say "Installing nftables and dnsmasq"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nftables dnsmasq >/dev/null

# ---- 3. Stop LPB ------------------------------------------------------------
mkdir -p "$BACKUP"
say "Looking for LPB services"
mapfile -t LPB_UNITS < <(systemctl list-unit-files --type=service --no-legend 2>/dev/null \
  | awk '{print $1}' | grep -Ei 'lpb|piso|nginx|php.*fpm|mariadb|mysql|apache2|lighttpd' | grep -v '^pisowifi\.service$' || true)
if ((${#LPB_UNITS[@]})); then
  printf '  %s\n' "${LPB_UNITS[@]}"
  if ask "Stop and disable these (they are not uninstalled)?"; then
    printf '%s\n' "${LPB_UNITS[@]}" > "$BACKUP/disabled-services.txt"
    for u in "${LPB_UNITS[@]}"; do systemctl disable --now "$u" 2>/dev/null || true; done
  fi
else
  echo "None found."
fi

if crontab -l 2>/dev/null | grep -q .; then
  crontab -l > "$BACKUP/root-crontab.txt"
  warn "Root has cron jobs (saved to $BACKUP/root-crontab.txt). Check for LPB jobs with: crontab -e"
fi

say "Saving old firewall rules"
if command -v iptables-save >/dev/null; then iptables-save > "$BACKUP/iptables.rules" 2>/dev/null || true; fi
nft list ruleset > "$BACKUP/nftables.rules" 2>/dev/null || true
if [[ -s "$BACKUP/iptables.rules" ]] && grep -qE '^-A' "$BACKUP/iptables.rules"; then
  if ask "Old iptables rules found (LPB's). Remove them so they can't open a bypass?"; then
    for t in filter nat mangle; do iptables -t "$t" -F 2>/dev/null || true; iptables -t "$t" -X 2>/dev/null || true; done
    iptables -P FORWARD DROP 2>/dev/null || true
    systemctl disable --now netfilter-persistent 2>/dev/null || true
    [[ -f /etc/iptables/rules.v4 ]] && mv /etc/iptables/rules.v4 "$BACKUP/"
  fi
fi

# ---- 4. Files ---------------------------------------------------------------
say "Installing files"
install -m 0755 ./pisowifi /usr/local/bin/pisowifi
install -d -m 0755 /etc/pisowifi /usr/local/share/doc/pisowifi
if [[ -f /etc/pisowifi/config.toml ]]; then
  echo "Keeping existing /etc/pisowifi/config.toml"
else
  install -m 0644 ./config.example.toml /etc/pisowifi/config.toml
fi
[[ -f README.md ]] && install -m 0644 README.md /usr/local/share/doc/pisowifi/
install -m 0644 ./pisowifi.service /etc/systemd/system/pisowifi.service

# dnsmasq: move LPB's configs aside, install ours.
mkdir -p "$BACKUP/dnsmasq.d"
cp -a /etc/dnsmasq.conf "$BACKUP/" 2>/dev/null || true
for f in /etc/dnsmasq.d/*; do
  [[ -e "$f" && "$(basename "$f")" != pisowifi.conf && "$(basename "$f")" != README ]] && mv "$f" "$BACKUP/dnsmasq.d/"
done
grep -qE '^\s*(dhcp-range|interface)=' /etc/dnsmasq.conf 2>/dev/null && {
  sed -i -E 's/^\s*(dhcp-range|interface|dhcp-option|address|server|listen-address)=/#lpb# &/' /etc/dnsmasq.conf
  echo "Commented out LPB settings in /etc/dnsmasq.conf (original saved)."
}
sed "s/^interface=.*/interface=$LAN_IF/" ./dnsmasq-pisowifi.conf > /etc/dnsmasq.d/pisowifi.conf
dnsmasq --test

# Routing, and no IPv6 for customers (prevents portal bypass over v6).
cat > /etc/sysctl.d/90-pisowifi.conf <<EOF
net.ipv4.ip_forward = 1
net.ipv6.conf.$LAN_IF.disable_ipv6 = 1
net.ipv4.conf.all.rp_filter = 1
net.ipv4.conf.all.accept_redirects = 0
net.ipv4.conf.all.send_redirects = 0
EOF
sysctl -q --system

# Boot-time firewall: closed from the first second, before pisowifi starts.
./pisowifi --config /etc/pisowifi/config.toml print-firewall > /tmp/pisowifi.nft
{ echo '#!/usr/sbin/nft -f'; echo 'flush ruleset'; cat /tmp/pisowifi.nft; } > /etc/nftables.conf
rm -f /tmp/pisowifi.nft
nft -c -f /etc/nftables.conf
systemctl enable -q nftables dnsmasq
systemctl restart nftables dnsmasq

# ---- 5. Owner password + TLS ------------------------------------------------
say "Setting the dashboard password"
echo "  Pick a strong password: 4 or more random words (like \"mango tricycle lamp"
echo "  ocean\") or 14+ random characters. Use it nowhere else. Never reuse the old"
echo "  LPB password: LPB stored it in plain text on this SD card."
until /usr/local/bin/pisowifi --config /etc/pisowifi/config.toml set-password; do
  warn "That password wasn't accepted. Try again."
done
/usr/local/bin/pisowifi --config /etc/pisowifi/config.toml admin-info

# ---- 5b. Lock the dashboard to the owner's phone ---------------------------
say "Locking the dashboard to your phone"
echo "  Customers share this network, so it's safest if only your phone can open"
echo "  the dashboard. In your phone's WiFi settings for this network, turn OFF"
echo "  the private/random address (iPhone: Private Wi-Fi Address; Android:"
echo "  MAC address type = Phone MAC), then copy the WiFi address shown there."
while true; do
  read -r -p "Your phone's WiFi MAC (e.g. a4:5e:60:11:22:33), or blank to skip: " OWNER_MAC
  if [[ -z "$OWNER_MAC" ]]; then
    warn "Skipped: any device on the customer WiFi can reach the sign-in page."
    warn "Add your MAC later under [admin] allow_macs in /etc/pisowifi/config.toml."
    break
  fi
  if [[ "$OWNER_MAC" =~ ^([0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}$ ]]; then
    OWNER_MAC="$(echo "$OWNER_MAC" | tr 'A-F-' 'a-f:')"
    sed -i -E "s/^allow_macs = .*/allow_macs = [\"$OWNER_MAC\"]/" /etc/pisowifi/config.toml
    echo "Dashboard locked to $OWNER_MAC."
    break
  fi
  warn "That doesn't look like a MAC address."
done

# ---- 6. Coin slot -----------------------------------------------------------
say "Finding the coin slot pin"
echo "Next, drop a few coins. Lines that pulse are printed. Press Ctrl-C when done."
if ask "Run the coin probe now?"; then
  /usr/local/bin/pisowifi coin-probe || true
  echo
  echo "Put the line number you saw into /etc/pisowifi/config.toml  ([coin] line = N)"
  read -r -p "Line number (blank to edit later): " LINE
  if [[ "$LINE" =~ ^[0-9]+$ ]]; then
    sed -i -E "s/^line = .*/line = $LINE/" /etc/pisowifi/config.toml
  fi
fi

# ---- 7. Start ---------------------------------------------------------------
systemctl daemon-reload
systemctl enable --now pisowifi
sleep 2
systemctl --no-pager --lines=8 status pisowifi || true

cat <<EOF

  Done.
  Customers:  connect to the WiFi; the portal opens at http://$LAN_IP/
  Owner:      https://$LAN_IP:8443/   (compare the certificate fingerprint above)
  Logs:       journalctl -u pisowifi -f
  LPB backup: $BACKUP
  Roll back:  sudo ./uninstall.sh $BACKUP

EOF
