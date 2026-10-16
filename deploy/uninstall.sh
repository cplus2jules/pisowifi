#!/usr/bin/env bash
# Stops pisowifi and puts back what the installer changed, then re-enables
# the LPB services it disabled.
# Usage: sudo ./uninstall.sh /root/lpb-backup-YYYYmmdd-HHMMSS
# Your encrypted database in /var/lib/pisowifi is left in place.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "Run as root"; exit 1; }
BACKUP="${1:?pass the backup folder printed by install.sh}"
[[ -d "$BACKUP" ]] || { echo "No such folder: $BACKUP"; exit 1; }

systemctl disable --now pisowifi 2>/dev/null || true
rm -f /etc/systemd/system/pisowifi.service /etc/dnsmasq.d/pisowifi.conf /etc/sysctl.d/90-pisowifi.conf \
  /etc/modules-load.d/pisowifi.conf /etc/systemd/system/dnsmasq.service.d/pisowifi.conf
nft delete table inet pisowifi 2>/dev/null || true

# Firewall: back to how LPB had it. If LPB didn't use the nftables service,
# switch it off (stopping it flushes nftables) before restoring iptables.
if [[ -f "$BACKUP/nftables.conf" ]]; then cp -a "$BACKUP/nftables.conf" /etc/nftables.conf; else rm -f /etc/nftables.conf; fi
if grep -qx enabled "$BACKUP/nftables-enabled.txt" 2>/dev/null; then
  systemctl restart nftables 2>/dev/null || true
else
  systemctl disable --now nftables 2>/dev/null || true
fi
[[ -s "$BACKUP/iptables.rules" ]] && { iptables-restore < "$BACKUP/iptables.rules" 2>/dev/null || true; }
[[ -s "$BACKUP/iptables-legacy.rules" ]] && { iptables-legacy-restore < "$BACKUP/iptables-legacy.rules" 2>/dev/null || true; }
if [[ -f "$BACKUP/rules.v4" ]]; then
  mkdir -p /etc/iptables && cp -a "$BACKUP/rules.v4" /etc/iptables/
  systemctl enable netfilter-persistent 2>/dev/null || true
fi

# Network: Armbian's own netplan files, if the installer set up the
# customer side itself (a fresh card).
if [[ -d "$BACKUP/netplan" ]] && compgen -G "$BACKUP/netplan/*.yaml" >/dev/null; then
  rm -f /etc/netplan/20-pisowifi.yaml
  cp -a "$BACKUP"/netplan/*.yaml /etc/netplan/
  netplan apply 2>/dev/null || true
fi

# DNS/DHCP: LPB's dnsmasq settings.
[[ -f "$BACKUP/dnsmasq.conf" ]] && cp -a "$BACKUP/dnsmasq.conf" /etc/dnsmasq.conf
[[ -f "$BACKUP/dnsmasq.default" ]] && cp -a "$BACKUP/dnsmasq.default" /etc/default/dnsmasq
cp -a "$BACKUP"/dnsmasq.d/* /etc/dnsmasq.d/ 2>/dev/null || true

if [[ -f "$BACKUP/disabled-services.txt" ]]; then
  while read -r u; do systemctl enable --now "$u" 2>/dev/null || true; done < "$BACKUP/disabled-services.txt"
fi
systemctl daemon-reload
systemctl restart dnsmasq 2>/dev/null || true
echo "pisowifi removed and LPB services re-enabled. Reboot to be sure everything is back."
