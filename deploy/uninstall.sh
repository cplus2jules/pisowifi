#!/usr/bin/env bash
# Stops pisowifi and re-enables the LPB services the installer disabled.
# Usage: sudo ./uninstall.sh /root/lpb-backup-YYYYmmdd-HHMMSS
# Your encrypted database in /var/lib/pisowifi is left in place.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "Run as root"; exit 1; }
BACKUP="${1:?pass the backup folder printed by install.sh}"
[[ -d "$BACKUP" ]] || { echo "No such folder: $BACKUP"; exit 1; }

systemctl disable --now pisowifi 2>/dev/null || true
rm -f /etc/systemd/system/pisowifi.service /etc/dnsmasq.d/pisowifi.conf /etc/sysctl.d/90-pisowifi.conf
nft delete table inet pisowifi 2>/dev/null || true

if [[ -f "$BACKUP/nftables.rules" ]]; then
  { echo 'flush ruleset'; cat "$BACKUP/nftables.rules"; } > /etc/nftables.conf
fi
[[ -f "$BACKUP/dnsmasq.conf" ]] && cp -a "$BACKUP/dnsmasq.conf" /etc/dnsmasq.conf
cp -a "$BACKUP"/dnsmasq.d/* /etc/dnsmasq.d/ 2>/dev/null || true
[[ -f "$BACKUP/rules.v4" ]] && { mkdir -p /etc/iptables; cp -a "$BACKUP/rules.v4" /etc/iptables/; }
[[ -f "$BACKUP/iptables.rules" ]] && iptables-restore < "$BACKUP/iptables.rules" 2>/dev/null || true

if [[ -f "$BACKUP/disabled-services.txt" ]]; then
  while read -r u; do systemctl enable --now "$u" 2>/dev/null || true; done < "$BACKUP/disabled-services.txt"
fi
systemctl daemon-reload
systemctl restart nftables dnsmasq 2>/dev/null || true
echo "pisowifi removed and LPB services re-enabled. Reboot to be sure everything is back."
