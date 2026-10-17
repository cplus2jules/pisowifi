#!/usr/bin/env bash
# One-time setup of the release signing key. Run it on your Mac, in the repo:
#   scripts/make-release-key.sh
#
# It makes a new Ed25519 key and
#   * writes the public half to release-key.pub. Commit it: every build
#     compiles it in, and vendos then only accept updates signed with this key;
#   * saves the secret half as the GitHub secret PISOWIFI_RELEASE_KEY, which
#     the release workflow signs with;
#   * keeps a backup of the secret in your login Keychain, named
#     "pisowifi-release-key".
# The secret is never written to disk, put on a command line, or shown.
#
# Put the GitHub secret back from the Keychain backup (if it was deleted):
#   security find-generic-password -s pisowifi-release-key -w | gh secret set PISOWIFI_RELEASE_KEY --repo cplus2jules/pisowifi
#
# Lost the key everywhere? Run this again with --replace, commit the new
# release-key.pub and make a release. Vendos still trust the old key, so each
# one needs a single reinstall over SSH from that release's bundle
# (./install.sh, which keeps settings, sales and customers' time). After that
# they take updates from the dashboard again.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

REPO=cplus2jules/pisowifi
SECRET=PISOWIFI_RELEASE_KEY
ITEM=pisowifi-release-key
PUB=release-key.pub
REPLACE=0
[[ "${1:-}" == --replace ]] && REPLACE=1

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$(uname)" == Darwin ]] || die "Run this on your Mac (it uses the macOS Keychain)."
command -v gh >/dev/null || die "Install the GitHub CLI first: brew install gh"
gh auth status >/dev/null 2>&1 || die "Sign in to GitHub first: gh auth login"
if ((!REPLACE)); then
  [[ ! -e "$PUB" ]] || die "$PUB already exists. A new key means every vendo needs one reinstall over SSH. If that's what you want: $0 --replace"
  if security find-generic-password -s "$ITEM" >/dev/null 2>&1; then
    die "Your Keychain already has a $ITEM item. To start over anyway: $0 --replace"
  fi
fi

say "Building the key tool"
cargo build --quiet

say "Making the key"
rm -f "$PUB"
# The secret comes back on stdout into this variable only.
KEY="$(target/debug/pisowifi release-key --public-out "$PUB")"
trap 'KEY=""' EXIT
[[ ${#KEY} -eq 64 && -s "$PUB" ]] || die "Making the key failed."

say "Saving it as the GitHub secret $SECRET"
printf '%s' "$KEY" | gh secret set "$SECRET" --repo "$REPO"

say "Backing it up in your login Keychain ($ITEM)"
# Fed to security's own command prompt on stdin, so it never shows up in ps.
printf 'add-generic-password -U -a %s -s %s -l %s -w %s\n' "$REPO" "$ITEM" "$ITEM" "$KEY" | security -i >/dev/null
KEY=""
security find-generic-password -s "$ITEM" -a "$REPO" >/dev/null || die "The Keychain backup didn't work. The GitHub secret is set; run this again with --replace."

cat <<EOF

  Done. Public key: $PUB
  Secret: GitHub secret $SECRET, backed up in your Keychain as "$ITEM".

  Next, commit the public key so every build trusts it:
    git add $PUB
    git commit -m "Add the release signing key"
    git push

  Then make the first release (README, "Updating").
EOF
