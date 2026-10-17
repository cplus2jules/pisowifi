#!/usr/bin/env bash
# Builds the Orange Pi bundle on your Mac: dist/pisowifi-armv7.tar.gz
# (and the hub's). These are unsigned: releases are built and signed by
# GitHub CI (.github/workflows/release.yml), which runs this same script.
# Needs: rustup target armv7-unknown-linux-musleabihf, zig, cargo-zigbuild.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
# macOS tar would otherwise add ._ resource-fork files to the bundles.
export COPYFILE_DISABLE=1

TARGET=armv7-unknown-linux-musleabihf
# CI runs the tests in a step of their own.
[[ "${SKIP_TESTS:-}" == 1 ]] || cargo test --quiet
cargo zigbuild --release --target "$TARGET"

OUT=dist/pisowifi
rm -rf dist && mkdir -p "$OUT"
cp "target/$TARGET/release/pisowifi" "$OUT/"
cp deploy/preflight.sh deploy/install.sh deploy/uninstall.sh deploy/pisowifi.service deploy/pisowifi-update.service \
  deploy/pisowifi-update.path deploy/dnsmasq-pisowifi.conf deploy/config.example.toml README.md "$OUT/"
chmod +x "$OUT"/*.sh "$OUT/pisowifi"
(cd dist && tar czf pisowifi-armv7.tar.gz pisowifi && shasum -a 256 pisowifi-armv7.tar.gz > pisowifi-armv7.tar.gz.sha256)

# Hub for a regular cloud server (x86_64 Linux).
HUB_TARGET=x86_64-unknown-linux-musl
cargo zigbuild --release --target "$HUB_TARGET"
HUB=dist/pisowifi-hub
mkdir -p "$HUB"
cp "target/$HUB_TARGET/release/pisowifi" "$HUB/"
cp deploy/install-hub.sh deploy/hub.example.toml deploy/pisowifi-hub.service deploy/pisowifi-hub-update.service \
  deploy/pisowifi-hub-autoupdate.service deploy/pisowifi-hub-autoupdate.timer deploy/Caddyfile README.md "$HUB/"
chmod +x "$HUB/install-hub.sh" "$HUB/pisowifi"
(cd dist && tar czf pisowifi-hub-x86_64.tar.gz pisowifi-hub && shasum -a 256 pisowifi-hub-x86_64.tar.gz > pisowifi-hub-x86_64.tar.gz.sha256)
echo "Built dist/pisowifi-hub-x86_64.tar.gz ($(du -h dist/pisowifi-hub-x86_64.tar.gz | cut -f1))"
echo "Built dist/pisowifi-armv7.tar.gz ($(du -h dist/pisowifi-armv7.tar.gz | cut -f1))"
cat dist/pisowifi-armv7.tar.gz.sha256
