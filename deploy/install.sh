#!/usr/bin/env bash
# Installs pisowifi on an Orange Pi running Armbian, replacing LPB.
# First run ./preflight.sh (changes nothing). Then, from the unpacked bundle:
#   sudo ./install.sh
# Nothing from LPB is deleted: its services are disabled and its settings are
# copied to /root/lpb-backup-<date>/ so you can roll back with uninstall.sh.
# Everything is checked before LPB is touched, and LPB is stopped as late as
# possible, so customers are offline for a minute or two at most.
set -euo pipefail

cd "$(dirname "$0")"
say()  { printf '\n\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }
ask()  { local a; read -r -p "$1 [y/N] " a; [[ "$a" =~ ^[Yy] ]]; }
setting() { sed -nE "s/^$1 *= *\"?([^\" #]*)\"?.*/\1/p" "$2" | head -1; }

[[ $EUID -eq 0 ]] || { echo "Run as root: sudo ./install.sh"; exit 1; }
[[ -x ./pisowifi ]] || { echo "pisowifi binary not found next to install.sh"; exit 1; }
./pisowifi --version >/dev/null || { echo "The binary doesn't run on this board."; exit 1; }

# The first run's copy of LPB's settings is the one to roll back to; a
# second run must not overwrite it with pisowifi's own settings.
BACKUP="$(ls -d /root/lpb-backup-* 2>/dev/null | head -1 || true)"
FIRST_RUN=0
if [[ -z "$BACKUP" ]]; then
  BACKUP="/root/lpb-backup-$(date +%Y%m%d-%H%M%S)"
  FIRST_RUN=1
fi
CHANGED=0
trap 'warn "The installer stopped (line $LINENO)."; ((CHANGED)) && warn "LPB may be stopped. Fix the problem and run ./install.sh again, or go back with: sudo ./uninstall.sh $BACKUP"' ERR

cat <<'EOF'

  Piso WiFi installer
  -------------------
  Before continuing, make a full copy of the SD card (e.g. balenaEtcher's
  "Clone drive"). That is your guaranteed way back to LPB.

  Customers keep using LPB until step 5; from then on they're offline for a
  minute or two while the new system starts.

EOF
ask "I have a backup of the SD card. Continue?" || exit 1

# ---- 1. Interfaces -----------------------------------------------------------
CONF=/etc/pisowifi/config.toml
SRC_CONF=./config.example.toml
[[ -f "$CONF" ]] && SRC_CONF="$CONF"
LAN_IF="${LAN_IF:-$(setting lan_iface "$SRC_CONF")}"
WAN_IF="${WAN_IF:-$(setting wan_iface "$SRC_CONF")}"
LAN_IP="$(setting portal_ip "$SRC_CONF")"

pick_iface() { # $1 = role description, $2 = current guess
  local name="$2"
  while ! ip link show "$name" >/dev/null 2>&1; do
    warn "There's no interface called '$name'. These exist:" >&2
    ip -br link | grep -v '^lo ' | sed 's/^/     /' >&2
    read -r -p "Which one $1? " name
  done
  echo "$name"
}
say "Network interfaces"
LAN_IF="$(pick_iface "goes to the access point (customers)" "$LAN_IF")"
WAN_IF="$(pick_iface "goes to the ISP modem" "$WAN_IF")"
[[ "$LAN_IF" != "$WAN_IF" ]] || { echo "The LAN and WAN interfaces must be different."; exit 1; }
echo "Customers: $LAN_IF   Modem: $WAN_IF"
LAN_PREFIX="$(setting lan_cidr "$SRC_CONF" | cut -s -d/ -f2)"
LAN_PREFIX="${LAN_PREFIX:-19}"
lan_ip_saved() {
  grep -rqsE "${LAN_IP//./\\.}(/| |$)" /etc/network /etc/NetworkManager/system-connections /etc/netplan /etc/systemd/network /etc/dhcpcd.conf
}
# A fresh Armbian Minimal card: netplan with systemd-networkd, and every
# ethernet port asking for an address by DHCP.
netplan_networkd() {
  command -v netplan >/dev/null && [[ -d /etc/netplan ]] \
    && systemctl is-enabled -q systemd-networkd 2>/dev/null \
    && ! grep -rqsiE 'renderer: *NetworkManager' /etc/netplan
}
if ip -4 addr show "$LAN_IF" | grep -q "inet $LAN_IP/"; then
  echo "OK: $LAN_IF has $LAN_IP"
  if ! lan_ip_saved; then
    warn "$LAN_IP isn't saved in any network config this installer knows. If LPB sets it at"
    warn "boot itself, it will be gone after a reboot once LPB is off. Make it permanent with"
    warn "armbian-config > Network, or tell whoever is helping you, before rebooting."
  fi
elif netplan_networkd; then
  say "Setting up $LAN_IF for customers ($LAN_IP/$LAN_PREFIX)"
  echo "  $WAN_IF keeps getting its address from the modem. $LAN_IF gets a fixed one;"
  echo "  the vendo hands out addresses there itself. Armbian's own netplan files"
  echo "  are moved to $BACKUP/netplan (uninstall.sh puts them back)."
  ask "Set up the network like this?" || exit 1
  mkdir -p "$BACKUP/netplan"
  MOVED=()
  for f in /etc/netplan/*.yaml; do
    [[ -e "$f" && "$f" != /etc/netplan/20-pisowifi.yaml ]] || continue
    mv "$f" "$BACKUP/netplan/" && MOVED+=("$(basename "$f")")
  done
  umask 077
  cat > /etc/netplan/20-pisowifi.yaml <<NETPLAN
# Written by the pisowifi installer.
# $WAN_IF: internet side, address from the modem by DHCP.
# $LAN_IF: customer side, a fixed address with no DHCP client or IPv6
# (dnsmasq serves DHCP there), kept even while no cable is plugged in.
network:
  version: 2
  renderer: networkd
  ethernets:
    $WAN_IF:
      dhcp4: true
    $LAN_IF:
      dhcp4: false
      dhcp6: false
      accept-ra: false
      link-local: []
      ignore-carrier: true
      addresses: [$LAN_IP/$LAN_PREFIX]
NETPLAN
  umask 022
  if ! netplan generate; then
    rm -f /etc/netplan/20-pisowifi.yaml
    for f in "${MOVED[@]}"; do mv "$BACKUP/netplan/$f" /etc/netplan/; done
    warn "netplan refused that setup. Nothing was changed. Send the output above to whoever is helping you."
    exit 1
  fi
  echo "  Applying. If your SSH connection drops, log in again and run ./install.sh again."
  netplan apply
  for _ in $(seq 1 15); do ip -4 addr show "$LAN_IF" | grep -q "inet $LAN_IP/" && break; sleep 1; done
  if ip -4 addr show "$LAN_IF" | grep -q "inet $LAN_IP/"; then
    echo "OK: $LAN_IF has $LAN_IP"
  else
    warn "$LAN_IF still doesn't have $LAN_IP. Check 'networkctl status $LAN_IF'."
    ask "Continue anyway?" || exit 1
  fi
else
  warn "$LAN_IF doesn't have $LAN_IP. Set it up as $LAN_IP/$LAN_PREFIX (armbian-config > Network) and run this again."
  ask "Continue anyway?" || exit 1
fi

# ---- 2. Packages -------------------------------------------------------------
PKGS=()
command -v nft >/dev/null || PKGS+=(nftables)
if ! command -v dnsmasq >/dev/null; then
  PKGS+=(dnsmasq)
  # Our settings go in first, so the package's first start only serves the
  # customer side and doesn't fight anything for port 53.
  mkdir -p /etc/dnsmasq.d
  sed "s/^interface=.*/interface=$LAN_IF/" ./dnsmasq-pisowifi.conf > /etc/dnsmasq.d/pisowifi.conf
fi
if ((${#PKGS[@]})); then
  say "Installing ${PKGS[*]}"
  apt-get update -qq || warn "apt-get update had problems; trying to install anyway"
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${PKGS[@]}" >/dev/null \
    || { warn "Couldn't install ${PKGS[*]}. Check the internet connection and apt sources. Nothing else was changed."; exit 1; }
fi

# ---- 3. Settings, and a dry run of the firewall --------------------------------
say "Settings"
install -d -m 0755 /etc/pisowifi /usr/local/share/doc/pisowifi
if [[ -f "$CONF" ]]; then
  echo "Keeping existing $CONF"
else
  install -m 0644 ./config.example.toml "$CONF"
fi
sed -i -E "s/^lan_iface *= *\"[^\"]*\"/lan_iface = \"$LAN_IF\"/; s/^wan_iface *= *\"[^\"]*\"/wan_iface = \"$WAN_IF\"/" "$CONF"
# SSH stays open from the modem's side on every port sshd listens on, plus
# the one this session came in on, or the new firewall would lock you out.
SSH_PORTS="$( { sshd -T 2>/dev/null | awk '$1 == "port" {print $2}'
               [[ -n "${SSH_CONNECTION:-}" ]] && echo "${SSH_CONNECTION##* }"
             } | grep -E '^[0-9]+$' | sort -un | paste -sd, - | sed 's/,/, /g' || true)"
SSH_PORTS="${SSH_PORTS:-22}"
if grep -qE '^ssh_ports *=' "$CONF"; then
  sed -i -E "s/^ssh_ports *=.*/ssh_ports = [$SSH_PORTS]/" "$CONF"
else
  sed -i -E "/^wan_iface *=/a ssh_ports = [$SSH_PORTS]" "$CONF"
fi
echo "SSH stays open from the modem's side on port $SSH_PORTS."
./pisowifi --config "$CONF" print-firewall > /tmp/pisowifi.nft
if ! nft -c -f /tmp/pisowifi.nft; then
  warn "This kernel refuses the firewall rules above. Nothing has been changed and LPB is still running."
  warn "Send the output of ./preflight.sh to whoever is helping you."
  exit 1
fi
echo "Firewall rules check out on this kernel."

# ---- 4. Owner password and phone (LPB still running) ---------------------------
say "Setting the dashboard password"
echo "  Pick a strong password: 4 or more random words (like \"mango tricycle lamp"
echo "  ocean\") or 14+ random characters. Use it nowhere else. Never reuse the old"
echo "  LPB password: LPB stored it in plain text on this SD card."
until ./pisowifi --config "$CONF" set-password; do
  warn "That password wasn't accepted. Try again."
done
./pisowifi --config "$CONF" admin-info

say "Locking the dashboard to your phone"
echo "  Customers share this network, so it's safest if only your phone can open"
echo "  the dashboard. In your phone's WiFi settings for this network, turn OFF"
echo "  the private/random address (iPhone: Private Wi-Fi Address; Android:"
echo "  MAC address type = Phone MAC), then copy the WiFi address shown there."
while true; do
  read -r -p "Your phone's WiFi MAC (e.g. a4:5e:60:11:22:33), or blank to skip: " OWNER_MAC
  if [[ -z "$OWNER_MAC" ]]; then
    warn "Skipped: any device on the customer WiFi can reach the sign-in page."
    warn "Add your MAC later under [admin] allow_macs in $CONF."
    break
  fi
  if [[ "$OWNER_MAC" =~ ^([0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}$ ]]; then
    OWNER_MAC="$(echo "$OWNER_MAC" | tr 'A-F-' 'a-f:')"
    sed -i -E "s/^allow_macs = .*/allow_macs = [\"$OWNER_MAC\"]/" "$CONF"
    echo "Dashboard locked to $OWNER_MAC."
    break
  fi
  warn "That doesn't look like a MAC address."
done

tz="$(timedatectl show -p Timezone --value 2>/dev/null || cat /etc/timezone 2>/dev/null || true)"
if [[ "$tz" != Asia/Manila ]] && ask "The time zone is '${tz:-unknown}'. Set it to Asia/Manila (so daily sales start at midnight)?"; then
  timedatectl set-timezone Asia/Manila 2>/dev/null || ln -sf /usr/share/zoneinfo/Asia/Manila /etc/localtime
fi

# ---- 5. Stop LPB ---------------------------------------------------------------
say "Looking for LPB"
mkdir -p "$BACKUP"
CHANGED=1
mapfile -t LPB_UNITS < <(systemctl list-unit-files --type=service --no-legend 2>/dev/null \
  | awk '{print $1}' | grep -Ei 'lpb|piso|nginx|php.*fpm|mariadb|mysql|apache2|lighttpd' | grep -v '^pisowifi\.service$' || true)
if ((${#LPB_UNITS[@]})); then
  printf '  %s\n' "${LPB_UNITS[@]}"
  if ask "Stop and disable these (they are not uninstalled)? Customers go offline from here."; then
    ((FIRST_RUN)) && printf '%s\n' "${LPB_UNITS[@]}" > "$BACKUP/disabled-services.txt"
    for u in "${LPB_UNITS[@]}"; do systemctl disable --now "$u" 2>/dev/null || true; done
  fi
else
  echo "No LPB services found."
fi
if [[ -f /etc/rc.local ]] && grep -vE '^\s*(#|$|exit 0)' /etc/rc.local | grep -q .; then
  ((FIRST_RUN)) && cp -a /etc/rc.local "$BACKUP/"
  warn "/etc/rc.local runs these at boot (LPB?). Check them with: nano /etc/rc.local"
  grep -vE '^\s*(#|$|exit 0)' /etc/rc.local | sed 's/^/     /'
fi
if crontab -l 2>/dev/null | grep -qvE '^\s*(#|$)'; then
  ((FIRST_RUN)) && crontab -l > "$BACKUP/root-crontab.txt"
  warn "Root has cron jobs (saved to $BACKUP/root-crontab.txt). Check for LPB jobs with: crontab -e"
fi
if [[ -s /root/.ssh/authorized_keys ]]; then
  warn "These SSH keys can log in as root (LPB's seller?). Remove any you don't know from /root/.ssh/authorized_keys:"
  awk '{print "     " $NF}' /root/.ssh/authorized_keys
fi
if ss -ltn 'sport = :80' 2>/dev/null | grep -q LISTEN; then
  warn "Something still serves port 80, which the portal needs:"
  ss -ltnp 'sport = :80' | sed 's/^/     /'
  ask "Continue anyway (the portal won't start until it's gone)?" || exit 1
fi

if ((FIRST_RUN)); then
  say "Saving LPB's firewall and DNS settings to $BACKUP"
  command -v iptables-save >/dev/null && { iptables-save > "$BACKUP/iptables.rules" 2>/dev/null || true; }
  command -v iptables-legacy-save >/dev/null && { iptables-legacy-save > "$BACKUP/iptables-legacy.rules" 2>/dev/null || true; }
  nft list ruleset > "$BACKUP/nftables.rules" 2>/dev/null || true
  systemctl is-enabled nftables > "$BACKUP/nftables-enabled.txt" 2>/dev/null || true
  [[ -f /etc/nftables.conf ]] && cp -a /etc/nftables.conf "$BACKUP/"
  cp -a /etc/dnsmasq.conf "$BACKUP/" 2>/dev/null || true
  cp -a /etc/default/dnsmasq "$BACKUP/dnsmasq.default" 2>/dev/null || true
  mkdir -p "$BACKUP/dnsmasq.d"
  for f in /etc/dnsmasq.d/*; do
    [[ -e "$f" && "$(basename "$f")" != pisowifi.conf && "$(basename "$f")" != README ]] && mv "$f" "$BACKUP/dnsmasq.d/"
  done
fi
# LPB's iptables rules could keep a bypass open. Policies go back to ACCEPT
# first (flushing alone keeps a DROP policy, which could cut off SSH);
# nftables does the enforcing from now on.
for ipt in iptables iptables-legacy; do
  command -v "$ipt" >/dev/null || continue
  if "$ipt"-save 2>/dev/null | grep -qE '^(-A|:[A-Z]+ DROP)'; then
    echo "Clearing LPB's $ipt rules"
    for c in INPUT FORWARD OUTPUT; do "$ipt" -P "$c" ACCEPT 2>/dev/null || true; done
    for t in filter nat mangle; do "$ipt" -t "$t" -F 2>/dev/null || true; "$ipt" -t "$t" -X 2>/dev/null || true; done
  fi
done
systemctl disable --now netfilter-persistent 2>/dev/null || true
[[ -f /etc/iptables/rules.v4 ]] && mv /etc/iptables/rules.v4 "$BACKUP/"

# ---- 6. Coin slot ----------------------------------------------------------------
say "Finding the coin slot pin"
if [[ -n "$(ls /sys/class/gpio 2>/dev/null | grep -E '^gpio[0-9]+$' || true)" ]]; then
  warn "Some pins are still claimed by LPB until the next reboot; the probe can't watch those."
fi
echo "Drop a few coins. Each burst of pulses is printed with its pin."
echo "Press Ctrl-C when done (it also stops by itself after 3 minutes)."
if ask "Run the coin probe now?"; then
  # Ctrl-C stops the probe only, not this installer.
  trap ':' INT
  timeout --foreground 180 ./pisowifi --config "$CONF" coin-probe || true
  trap - INT
  echo
  read -r -p "The line number it showed (blank to set it later): " LINE
  if [[ "$LINE" =~ ^[0-9]+$ ]]; then
    sed -i -E "s/^line = .*/line = $LINE/" "$CONF"
  fi
fi

# ---- 7. Switch over ----------------------------------------------------------------
say "Installing pisowifi"
install -m 0755 ./pisowifi /usr/local/bin/pisowifi
[[ -f README.md ]] && install -m 0644 README.md /usr/local/share/doc/pisowifi/
install -m 0644 ./pisowifi.service /etc/systemd/system/pisowifi.service

# dnsmasq: only our settings. bind-dynamic in pisowifi.conf copes with the LAN
# adapter coming up late; the drop-in restarts dnsmasq if it ever dies.
sed "s/^interface=.*/interface=$LAN_IF/" ./dnsmasq-pisowifi.conf > /etc/dnsmasq.d/pisowifi.conf
printf '# pisowifi: settings are in /etc/dnsmasq.d/pisowifi.conf\n# (LPB'"'"'s original is in %s)\n' "$BACKUP" > /etc/dnsmasq.conf
printf 'ENABLED=1\nCONFIG_DIR=/etc/dnsmasq.d,.dpkg-dist,.dpkg-old,.dpkg-new\n' > /etc/default/dnsmasq
mkdir -p /etc/systemd/system/dnsmasq.service.d
printf '[Unit]\nStartLimitIntervalSec=0\n[Service]\nRestart=on-failure\nRestartSec=5\n' > /etc/systemd/system/dnsmasq.service.d/pisowifi.conf
dnsmasq --test -C /etc/dnsmasq.conf -7 /etc/dnsmasq.d,.dpkg-dist,.dpkg-old,.dpkg-new

# Routing; no IPv6 for customers (prevents portal bypass over v6); a bigger
# connection table (the default is small on a 512 MB board).
echo nf_conntrack > /etc/modules-load.d/pisowifi.conf
modprobe nf_conntrack 2>/dev/null || true
cat > /etc/sysctl.d/90-pisowifi.conf <<EOF
net.ipv4.ip_forward = 1
net.ipv6.conf.$LAN_IF.disable_ipv6 = 1
net.ipv4.conf.all.rp_filter = 1
net.ipv4.conf.all.accept_redirects = 0
net.ipv4.conf.all.send_redirects = 0
net.netfilter.nf_conntrack_max = 65536
net.netfilter.nf_conntrack_tcp_timeout_established = 3600
EOF
sysctl -q --system || warn "Some kernel settings didn't apply (see above); continuing."

# Boot-time firewall: closed from the first second, before pisowifi starts.
{ echo '#!/usr/sbin/nft -f'; echo 'flush ruleset'; cat /tmp/pisowifi.nft; } > /etc/nftables.conf
rm -f /tmp/pisowifi.nft
nft -c -f /etc/nftables.conf

systemctl daemon-reload
systemctl enable -q nftables dnsmasq pisowifi
systemctl restart nftables dnsmasq
systemctl restart pisowifi
sleep 3
systemctl --no-pager --lines=10 status pisowifi || true

cat <<EOF

  Done. Customers can use the WiFi again.
  Customers:  connect to the WiFi; the portal opens at http://$LAN_IP/
  Owner:      https://$LAN_IP:8443/   (compare the certificate fingerprint above)
  Logs:       journalctl -u pisowifi -f
  LPB backup: $BACKUP
  Roll back:  sudo ./uninstall.sh $BACKUP

  Before rebooting:
  * SSH now only works from the modem's side (e.g. a laptop on the modem's own
    WiFi), on port $SSH_PORTS, not from the customer WiFi. Check that you can
    log in that way.
  * Then reboot once (sudo reboot) and check everything comes back by itself:
    the portal opens on a phone, and a coin gives time.

EOF
