#!/usr/bin/env bash
# Builds and signs a release on your Mac. Nothing leaves this computer unless
# you run the separate --publish step and say yes.
#
#   scripts/release.sh --init-key            create your release key (once)
#   scripts/release.sh [--notes-file F] [--min-from X.Y.Z]
#                                            build both bundles and sign them
#   scripts/release.sh --publish             upload the signed build to GitHub
#
# The private key lives in ~/.config/pisowifi/release.key (mode 0600, never
# in the repo; back it up somewhere safe and offline). Its public half is
# release-key.pub, which is compiled into every build: vendos and hubs only
# install updates signed with this key. Lose the key and you'll have to
# reinstall every box with install.sh to trust a new one.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

KEY="${PISOWIFI_RELEASE_KEY:-$HOME/.config/pisowifi/release.key}"
PUB=release-key.pub
REPO=cplus2jules/pisowifi
VERSION="$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -1)"
ASSETS=(dist/pisowifi-armv7.tar.gz dist/pisowifi-hub-x86_64.tar.gz)

say()  { printf '\n\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

MODE=build NOTES_FILE="" MIN_FROM=0.0.0
while (($#)); do
  case "$1" in
    --init-key) MODE=init ;;
    --publish) MODE=publish ;;
    --notes-file) NOTES_FILE="${2:?}"; shift ;;
    --min-from) MIN_FROM="${2:?}"; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
  shift
done

# A host build of the same code does the signing, so there's one Ed25519
# implementation, and it's the one the boxes run.
tool() {
  cargo build --release --quiet
  target/release/pisowifi "$@"
}

public_of_key() { tool release-key --key "$KEY"; }

if [[ $MODE == init ]]; then
  if [[ -f "$KEY" ]]; then
    echo "Release key already exists: $KEY"
  else
    say "Creating your release key"
  fi
  pub="$(public_of_key)"
  printf '# Piso WiFi release key (Ed25519). Vendos and hubs built with this file\n# only install updates signed by the matching private key.\n%s\n' "$pub" > "$PUB"
  echo "Private key: $KEY (mode 0600; back it up offline, never commit it)"
  echo "Public key:  $PUB = $pub"
  echo
  echo "Next: commit $PUB, so every build from now on trusts this key:"
  echo "  git add $PUB && git commit -m 'Add release signing key'"
  exit 0
fi

if [[ $MODE == publish ]]; then
  command -v gh >/dev/null || die "Install the GitHub CLI (brew install gh) first."
  for f in "${ASSETS[@]}"; do
    [[ -f "$f" && -f "$f.manifest" ]] || die "$f or its .manifest is missing; run scripts/release.sh first."
  done
  [[ -f "$PUB" ]] || die "$PUB is missing."
  # Re-check what's about to go out against the committed public key.
  tool release-verify --bundle "${ASSETS[0]}" --public-key "$PUB" --kind vendo --target armv7-unknown-linux-musleabihf
  tool release-verify --bundle "${ASSETS[1]}" --public-key "$PUB" --kind hub --target x86_64-unknown-linux-musl
  built="$(sed -nE 's/.*\\"version\\":\\"([^\\]+)\\".*/\1/p' "${ASSETS[0]}.manifest" | head -1)"
  [[ "$built" == "$VERSION" ]] || die "dist/ holds version $built but Cargo.toml says $VERSION. Rebuild first."
  echo
  echo "About to create the GitHub release v$VERSION in $REPO with:"
  for f in "${ASSETS[@]}"; do echo "  $f  $f.manifest"; done
  read -r -p "Publish now? Every vendo checking for updates will see it. [y/N] " a
  [[ "$a" =~ ^[Yy] ]] || { echo "Not published."; exit 1; }
  notes=()
  [[ -f dist/RELEASE_NOTES.txt ]] && notes=(--notes-file dist/RELEASE_NOTES.txt)
  ((${#notes[@]})) || notes=(--notes "")
  gh release create "v$VERSION" --repo "$REPO" --title "v$VERSION" "${notes[@]}" \
    "${ASSETS[0]}" "${ASSETS[0]}.manifest" "${ASSETS[1]}" "${ASSETS[1]}.manifest"
  echo "Published v$VERSION."
  exit 0
fi

# ---- build and sign ----------------------------------------------------------------
[[ -f "$KEY" ]] || die "No release key yet. Run: scripts/release.sh --init-key"
[[ -f "$PUB" ]] || die "$PUB is missing. Run: scripts/release.sh --init-key"
pub="$(public_of_key)"
grep -qx "$pub" "$PUB" || die "$PUB doesn't match $KEY. Builds would trust a different key than the one signing them."
git ls-files --error-unmatch "$PUB" >/dev/null 2>&1 || warn "$PUB isn't committed yet. Commit it, or builds made elsewhere won't trust this key."
if ! git diff --quiet HEAD -- . 2>/dev/null; then
  warn "You have uncommitted changes; the release will include them."
fi

if [[ -n "$NOTES_FILE" ]]; then
  [[ -f "$NOTES_FILE" ]] || die "No such file: $NOTES_FILE"
else
  NOTES_FILE="$(mktemp)"
  echo "What changed in $VERSION? (shown to owners in the dashboard; end with Ctrl-D)"
  cat > "$NOTES_FILE"
fi

say "Building version $VERSION"
scripts/build-pi.sh
cp "$NOTES_FILE" dist/RELEASE_NOTES.txt

say "Signing"
tool release-sign --key "$KEY" --bundle "${ASSETS[0]}" --kind vendo --target armv7-unknown-linux-musleabihf \
  --version "$VERSION" --min-from "$MIN_FROM" --notes-file "$NOTES_FILE"
tool release-sign --key "$KEY" --bundle "${ASSETS[1]}" --kind hub --target x86_64-unknown-linux-musl \
  --version "$VERSION" --min-from "$MIN_FROM" --notes-file "$NOTES_FILE"
tool release-verify --bundle "${ASSETS[0]}" --public-key "$PUB" --kind vendo --target armv7-unknown-linux-musleabihf
tool release-verify --bundle "${ASSETS[1]}" --public-key "$PUB" --kind hub --target x86_64-unknown-linux-musl

cat <<EOF

  Signed version $VERSION:
    ${ASSETS[0]}  (+ .manifest)
    ${ASSETS[1]}  (+ .manifest)

  Install by hand (works while the repo is private):
    scp ${ASSETS[0]} ${ASSETS[0]}.manifest root@VENDO:/root/
    ssh root@VENDO pisowifi update --file /root/pisowifi-armv7.tar.gz

  Or publish to GitHub Releases so vendos find it (asks before uploading):
    scripts/release.sh --publish
EOF
