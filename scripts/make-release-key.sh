#!/usr/bin/env bash
# One-time setup for releases. Run it on your Mac, in the repo:
#   scripts/make-release-key.sh
#
# 1. The release signing key (Ed25519):
#    * the public half goes to release-key.pub. Commit it: every build
#      compiles it in, and vendos then only accept updates signed with it;
#    * the secret half becomes the GitHub secret PISOWIFI_RELEASE_KEY, which
#      the release workflow signs with;
#    * a backup of the secret goes into your login Keychain, named
#      "pisowifi-release-key".
#    The secret is never written to disk, put on a command line, or shown.
#
# 2. The deploy key that lets the release workflow publish to the public
#    repository cplus2jules/pisowifi-releases (and nothing else):
#    * its public half is added to that repository with write access;
#    * its secret half becomes the GitHub secret RELEASES_DEPLOY_KEY.
#    It sits in a private temporary folder for the moment this takes, then
#    is deleted. It needs no backup: if it's ever lost or leaked, make a new
#    one with  scripts/make-release-key.sh --deploy-key  (vendos don't care).
#
# Put the signing secret back from the Keychain backup (if it was deleted):
#   security find-generic-password -s pisowifi-release-key -w | gh secret set PISOWIFI_RELEASE_KEY --repo cplus2jules/pisowifi
#
# Lost the signing key everywhere? Run this again with --replace, commit the
# new release-key.pub and make a release. Vendos still trust the old key, so
# each one needs a single reinstall over SSH from that release's bundle
# (./install.sh, which keeps settings, sales and customers' time). After that
# they update by themselves again.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

REPO=cplus2jules/pisowifi
RELEASES=cplus2jules/pisowifi-releases
SECRET=PISOWIFI_RELEASE_KEY
DEPLOY_SECRET=RELEASES_DEPLOY_KEY
DEPLOY_TITLE="pisowifi release workflow"
ITEM=pisowifi-release-key
PUB=release-key.pub
MODE=all
case "${1:-}" in
  "") ;;
  --replace) MODE=replace ;;
  --deploy-key) MODE=deploy ;;
  *) echo "Usage: $0 [--replace | --deploy-key]" >&2; exit 2 ;;
esac

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$(uname)" == Darwin ]] || die "Run this on your Mac (it uses the macOS Keychain)."
command -v gh >/dev/null || die "Install the GitHub CLI first: brew install gh"
gh auth status >/dev/null 2>&1 || die "Sign in to GitHub first: gh auth login"
gh repo view "$RELEASES" >/dev/null 2>&1 || die "Can't see $RELEASES. Is gh signed in as its owner?"

KEY=""
TMP=""
cleanup() {
  KEY=""
  if [[ -n "$TMP" ]]; then rm -rf -- "${TMP:?}"; fi
}
trap cleanup EXIT

signing_key() {
  if [[ "$MODE" != replace ]]; then
    [[ ! -e "$PUB" ]] || die "$PUB already exists. A new key means every vendo needs one reinstall over SSH. If that's what you want: $0 --replace (or only the deploy key: $0 --deploy-key)"
    if security find-generic-password -s "$ITEM" >/dev/null 2>&1; then
      die "Your Keychain already has a $ITEM item. To start over anyway: $0 --replace"
    fi
  fi

  say "Building the key tool"
  cargo build --quiet

  say "Making the release signing key"
  rm -f "$PUB"
  # The secret comes back on stdout into this variable only.
  KEY="$(target/debug/pisowifi release-key --public-out "$PUB")"
  [[ ${#KEY} -eq 64 && -s "$PUB" ]] || die "Making the key failed."

  say "Saving it as the GitHub secret $SECRET"
  printf '%s' "$KEY" | gh secret set "$SECRET" --repo "$REPO"

  say "Backing it up in your login Keychain ($ITEM)"
  # Fed to security's own command prompt on stdin, so it never shows up in ps.
  printf 'add-generic-password -U -a %s -s %s -l %s -w %s\n' "$REPO" "$ITEM" "$ITEM" "$KEY" | security -i >/dev/null
  KEY=""
  security find-generic-password -s "$ITEM" -a "$REPO" >/dev/null || die "The Keychain backup didn't work. The GitHub secret is set; run this again with --replace."
}

deploy_key() {
  say "Making the deploy key for $RELEASES"
  TMP="$(mktemp -d)"
  chmod 700 "$TMP"
  ssh-keygen -q -t ed25519 -N "" -C "$DEPLOY_TITLE" -f "$TMP/deploy"
  # An older one from a previous run goes first.
  for id in $(gh repo deploy-key list --repo "$RELEASES" --json id,title --jq ".[] | select(.title == \"$DEPLOY_TITLE\") | .id"); do
    gh repo deploy-key delete "$id" --repo "$RELEASES"
  done
  gh repo deploy-key add "$TMP/deploy.pub" --repo "$RELEASES" --allow-write --title "$DEPLOY_TITLE" >/dev/null
  say "Saving it as the GitHub secret $DEPLOY_SECRET"
  gh secret set "$DEPLOY_SECRET" --repo "$REPO" < "$TMP/deploy"
  rm -rf -- "${TMP:?}"
  TMP=""
}

if [[ "$MODE" != deploy ]]; then signing_key; fi
deploy_key

if [[ "$MODE" == deploy ]]; then
  printf '\n  Done. The release workflow can publish to %s again.\n\n' "$RELEASES"
  exit 0
fi

cat <<EOF

  Done.
  Signing key: public half in $PUB; secret in the GitHub secret $SECRET,
               backed up in your Keychain as "$ITEM".
  Deploy key:  GitHub secret $DEPLOY_SECRET, with write access to $RELEASES only.

  Next, commit the public key so every build trusts it:
    git add $PUB
    git commit -m "Add the release signing key"
    git push

  Then make the first release (README, "Updating").
EOF
