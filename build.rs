// Compile-time facts the updater checks bundles against:
// * PISOWIFI_TARGET: the target triple this binary is built for, so a bundle
//   for another board is refused;
// * PISOWIFI_RELEASE_PUBKEY: the owner's Ed25519 release key (hex), from
//   release-key.pub. scripts/release.sh writes that file the first time it
//   runs. Without it, the build still works but refuses every update.
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=release-key.pub");
    println!("cargo:rustc-env=PISOWIFI_TARGET={}", env::var("TARGET").unwrap());
    let key = match fs::read_to_string("release-key.pub") {
        Ok(text) => {
            let key = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .unwrap_or("")
                .to_ascii_lowercase();
            // A malformed key must fail the build, not quietly turn updates off.
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                panic!("release-key.pub must hold one 64-character hex Ed25519 public key");
            }
            key
        }
        Err(_) => String::new(),
    };
    println!("cargo:rustc-env=PISOWIFI_RELEASE_PUBKEY={key}");
}
