#!/usr/bin/env bash
# Read-only check of this board before installing pisowifi. Changes nothing.
# Run from the unpacked bundle:   sudo ./preflight.sh
# Then send the whole output to whoever is helping you install.
set -uo pipefail
cd "$(dirname "$0")"

CONFIG=./config.example.toml
[[ -f /etc/pisowifi/config.toml ]] && CONFIG=/etc/pisowifi/config.toml
setting() { sed -nE "s/^$1 *= *\"?([^\" #]*)\"?.*/\1/p" "$CONFIG" | head -1; }
LAN_IF="${LAN_IF:-$(setting lan_iface)}"
WAN_IF="${WAN_IF:-$(setting wan_iface)}"
PORTAL_IP="$(setting portal_ip)"
PROBLEMS=0; WARNINGS=0

section() { printf '\n== %s\n' "$*"; }
ok()      { printf '  ok    %s\n' "$*"; }
bad()     { printf '  FAIL  %s\n' "$*"; PROBLEMS=$((PROBLEMS + 1)); }
warn()    { printf '  WARN  %s\n' "$*"; WARNINGS=$((WARNINGS + 1)); }
info()    { printf '        %s\n' "$*"; }
show()    { sed 's/^/        /' | cut -c1-160; }

[[ $EUID -eq 0 ]] || { echo "Run as root: sudo ./preflight.sh"; exit 1; }
echo "pisowifi preflight, $(date '+%Y-%m-%d %H:%M %Z'), config $CONFIG"

section "Board and system"
. /etc/os-release 2>/dev/null; info "OS:      ${PRETTY_NAME:-unknown}"
[[ -f /etc/armbian-release ]] && info "Armbian: $(. /etc/armbian-release; echo "${BOARD_NAME:-?} ${VERSION:-?} (${BRANCH:-?})")"
info "Kernel:  $(uname -r) $(uname -m)"
info "Memory:  $(free -m | awk '/^Mem:/ {print $2 " MB, " $7 " MB free"}')   Disk: $(df -h / | awk 'NR==2 {print $4 " free of " $2}')"
case "$(uname -m)" in armv7l|armv6l) ok "CPU matches this bundle (ARMv7)";; *) bad "CPU $(uname -m): this bundle is built for ARMv7";; esac
if ./pisowifi --version >/dev/null 2>&1; then ok "the pisowifi binary runs ($(./pisowifi --version))"; else bad "the pisowifi binary doesn't run here"; fi
kv="$(uname -r | cut -d. -f1-2)"
if [[ "$(printf '%s\n5.2\n' "$kv" | sort -V | head -1)" == 5.2 ]]; then ok "kernel $kv supports the firewall's NAT"
else bad "kernel $kv is older than 5.2: the firewall's NAT (inet family) needs 5.2+"; fi
tz="$(timedatectl show -p Timezone --value 2>/dev/null || cat /etc/timezone 2>/dev/null)"
[[ "$tz" == Asia/Manila ]] && ok "time zone Asia/Manila" || warn "time zone is '${tz:-unknown}': daily sales would roll over at the wrong hour (installer can fix)"
[[ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null)" == yes ]] && ok "clock synced" || warn "clock not synced by NTP (no internet yet?)"

section "Tools"
if command -v nft >/dev/null; then ok "$(nft --version 2>&1 | head -1)"; else info "nftables not installed yet (the installer adds it)"; fi
if command -v dnsmasq >/dev/null; then ok "$(dnsmasq --version | head -1)"; else info "dnsmasq not installed yet (the installer adds it)"; fi
command -v iptables >/dev/null && info "$(iptables --version 2>&1)"
grep -qE '^deb|^URIs' /etc/apt/sources.list /etc/apt/sources.list.d/* 2>/dev/null && info "apt sources: $(grep -hE '^deb ' /etc/apt/sources.list /etc/apt/sources.list.d/*.list 2>/dev/null | awk '{print $2, $3}' | sort -u | tr '\n' ';')"

section "Network"
ip -br link | show
ip -4 -br addr | show
if ip link show "$LAN_IF" >/dev/null 2>&1; then
  ok "LAN interface $LAN_IF exists"
  if ip -4 addr show "$LAN_IF" | grep -q "inet $PORTAL_IP/"; then ok "$LAN_IF has $PORTAL_IP"
  elif command -v netplan >/dev/null && systemctl is-enabled -q systemd-networkd 2>/dev/null; then
    ok "$LAN_IF doesn't have $PORTAL_IP yet; install.sh sets it up (netplan + networkd, a fresh card)"
  else bad "$LAN_IF doesn't have $PORTAL_IP (customers' DHCP, DNS and portal need it)"; fi
else
  bad "LAN interface '$LAN_IF' doesn't exist. Which one goes to the access point? Rerun with LAN_IF=name"
fi
if ip link show "$WAN_IF" >/dev/null 2>&1; then
  ok "WAN interface $WAN_IF exists"
  ip -4 addr show "$WAN_IF" | grep -q inet && ok "$WAN_IF has an address: $(ip -4 -o addr show "$WAN_IF" | awk '{print $4}' | head -1)" || warn "$WAN_IF has no IPv4 address (modem off?)"
else
  bad "WAN interface '$WAN_IF' doesn't exist. Which one goes to the modem? Rerun with WAN_IF=name"
fi
info "default route: $(ip route show default | head -1)"
info "network managers running: $(for s in NetworkManager systemd-networkd networking; do systemctl is-active -q "$s" 2>/dev/null && printf '%s ' "$s"; done)"
found=$(grep -rlsE "${PORTAL_IP//./\\.}(/| |$)" /etc/network /etc/NetworkManager/system-connections /etc/netplan /etc/systemd/network /etc/rc.local /etc/dhcpcd.conf 2>/dev/null)
if [[ -n "$found" ]]; then ok "$PORTAL_IP is saved in: $(echo $found)"; else warn "$PORTAL_IP isn't saved in any network config found; LPB may set it at boot itself, and it could vanish once LPB is off"; fi

section "Ports in use (53 DNS, 67 DHCP, 80 portal, 8443 dashboard)"
ss -ltnup 2>/dev/null | grep -E ':(53|67|80|8443)\s' | awk '{print $1, $5, $7}' | show

section "LPB and other things that start at boot"
systemctl list-unit-files --type=service --state=enabled --no-legend 2>/dev/null | awk '{print $1}' | tr '\n' ' ' | fold -w 150 | show
[[ -f /etc/rc.local ]] && { info "/etc/rc.local:"; grep -vE '^\s*(#|$|exit 0)' /etc/rc.local | show; }
crontab -l 2>/dev/null | grep -vE '^\s*(#|$)' | sed 's/^/root cron: /' | show
grep -vE '^\s*(#|$|SHELL|PATH|MAILTO)' /etc/crontab 2>/dev/null | sed 's/^/crontab: /' | show
ls /etc/cron.d 2>/dev/null | tr '\n' ' ' | sed 's/^/cron.d: /' | show
crontab -l -u www-data 2>/dev/null | grep -vE '^\s*(#|$)' | sed 's/^/www-data cron: /' | show
for f in /etc/dnsmasq.conf /etc/dnsmasq.d/*; do [[ -f "$f" ]] && { info "$f:"; grep -vE '^\s*(#|$)' "$f" | show; }; done
[[ -f /etc/default/dnsmasq ]] && grep -vE '^\s*(#|$)' /etc/default/dnsmasq | sed 's#^#/etc/default/dnsmasq: #' | show
info "SSH listens on port(s): $(sshd -T 2>/dev/null | awk '$1 == "port" {print $2}' | paste -sd' ' -)${SSH_CONNECTION:+ (this session: ${SSH_CONNECTION##* })}"
if [[ -s /root/.ssh/authorized_keys ]]; then warn "root accepts $(grep -c . /root/.ssh/authorized_keys) SSH key(s) (from LPB's seller?): $(awk '{print $NF}' /root/.ssh/authorized_keys | tr '\n' ' ')"; fi

section "Old firewall rules"
if command -v iptables-save >/dev/null; then
  info "iptables rules: $(iptables-save 2>/dev/null | grep -c '^-A'), policies: $(iptables -S 2>/dev/null | grep '^-P' | tr '\n' ' ')"
  command -v iptables-legacy-save >/dev/null && info "iptables-legacy rules: $(iptables-legacy-save 2>/dev/null | grep -c '^-A')"
fi
command -v nft >/dev/null && info "nftables tables: $(nft list tables 2>/dev/null | tr '\n' ' ')"

section "Coin slot (GPIO)"
ls /dev/gpiochip* 2>/dev/null | tr '\n' ' ' | show
command -v gpiodetect >/dev/null && gpiodetect | show
exported=$(ls /sys/class/gpio 2>/dev/null | grep -E '^gpio[0-9]+$' | tr '\n' ' ')
[[ -n "$exported" ]] && warn "pins still claimed through /sys/class/gpio (LPB?): $exported. A reboot after the install frees them." || ok "no pins claimed through /sys/class/gpio"
if [[ -r /sys/bus/nvmem/devices/sunxi-sid0/nvmem ]] && od -An -tx1 -N16 /sys/bus/nvmem/devices/sunxi-sid0/nvmem | grep -qvE '^( 00)+$'; then
  ok "chip ID readable: the database will be locked to this board"
else
  warn "chip ID not readable: the database key will be a file on the SD card instead"
fi

section "Firewall check (nothing is applied)"
if command -v nft >/dev/null; then
  if out=$(./pisowifi --config "$CONFIG" print-firewall 2>&1 | nft -c -f /dev/stdin 2>&1); then ok "the pisowifi firewall rules are accepted by this kernel"
  else bad "the firewall rules were refused:"; echo "$out" | show; fi
else
  info "skipped until nftables is installed"
fi
info "conntrack table size: $(cat /proc/sys/net/netfilter/nf_conntrack_max 2>/dev/null || echo 'module not loaded')"

section "Summary"
if ((PROBLEMS)); then echo "  $PROBLEMS problem(s), $WARNINGS warning(s). Don't install until the FAIL lines are sorted out."
else echo "  No problems, $WARNINGS warning(s). OK to run ./install.sh"; fi
