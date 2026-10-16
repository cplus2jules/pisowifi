# pisowifi

Open-source, security-first Piso WiFi for an Orange Pi. One static Rust
binary (about 6 MB) replaces LPB's nginx + PHP + MariaDB stack.

- **Customer portal** on `http://10.0.0.1/`: tap *Insert coin*, drop coins,
  get online. Pause and resume to save time. Voucher codes. Each coin you drop
  animates on screen as the real ₱1/₱5/₱10/₱20 coin it is. First-time visitors
  get a three-step onboarding (reopen it with the **?** button).
- **Your branding**: name, tagline, accent colour, logo and an advert poster,
  set on the dashboard's *Customer page*. That page also prints a QR sign: one
  code joins the WiFi from the phone camera, the other opens the portal.
- **Owner dashboard** on `https://10.0.0.1:8443/`: earnings, connected
  devices, coin slot status, rates, printable vouchers, activity log.
- **Encrypted database**: SQLCipher (AES-256 with an HMAC-SHA512 check on every page).

## What's in the box

| Part | Your hardware |
|---|---|
| `eth0` | built-in port → ISP modem (DHCP client) |
| `eth1` | USB LAN adapter → outdoor access point, `10.0.0.1/19` |
| Coin slot | ALLAN universal coin selector → one GPIO line (find it with `coin-probe`) |
| Board | Orange Pi One (Allwinner H3), Armbian |

## Security model

| Threat | What stops it |
|---|---|
| Customer gets online without paying | nftables forward policy is **drop**. Only devices whose MAC **and** IP are both in the paid set are routed. If the service dies, the table stays: it **fails closed**. Every minute the service checks the kernel still matches: if something flushed the table (an nftables reload, a leftover LPB script), it's reinstalled with every customer in one step, and the audit log says so. |
| Bypass over IPv6, DNS tunnels ("SlowDNS" apps) | IPv6 from customers is dropped. Every DNS query is forced to the box's own resolver; unpaid devices get only 10 lookups a second, enough to find the portal but too little for a usable tunnel. |
| Customer pokes the ISP modem, SSH, or the box | Customers can reach only DHCP, DNS, the portal and the dashboard. Private ranges (192.168.x, 10.x, …) are unreachable. SSH works only from the modem side. |
| Stolen or cloned SD card | The DB key is derived from the **H3 chip's factory ID** plus a random salt, stretched with Argon2id (64 MiB, 3 passes) so guessing the ID is slow. The key is never stored, so the card can't be opened on another board. **Stealing the whole box (chip and card) still opens the database**; at that point the Argon2id-hashed admin password is what protects your account. |
| Admin password sniffed on the open WiFi | The dashboard is HTTPS only (TLS 1.2/1.3, rustls). The password is Argon2id-hashed (64 MiB, 3 passes). Sessions are bound to your IP with HttpOnly, Secure, SameSite=Strict cookies plus a CSRF token. |
| Password guessing from the customer WiFi | The installer locks the dashboard to your phone's MAC. Other devices are turned away before any TLS work, from a separate small pool, so customers can't tie up the dashboard; they see a page explaining the lock. The dashboard port has its own flood limit. 5 wrong tries locks a device out for 15 minutes. After 20 failures box-wide (from any addresses), only one attempt per 30 s is allowed, and that is logged. Common passwords (the 10k list, sequences, keyboard runs, the WiFi or store name) are refused. |
| Customer "claims" a fresh box | The password can only be set from the device's shell (`set-password`), never from the web. |
| Web attacks (XSS, CSRF, clickjacking, DNS rebinding) | Strict CSP (no inline scripts), no `innerHTML`, custom-header CSRF checks, Host-header checks, `frame-ancestors 'none'`. |
| Slowloris / connection floods | Connection caps, 10 s header timeout, 4–16 KB body limits, one Argon2 hash at a time. |
| Ledger tampering | All SQL uses bound parameters. Voucher codes are stored hashed (shown once). Test coins exist only in dev mode. Every owner action goes to the audit log. |
| Compromised service | The systemd unit drops all capabilities except networking, the filesystem is read-only except `/var/lib/pisowifi`, and only GPIO devices are allowed. |

**Known limits** (true of every open-WiFi vendo, LPB included):
- **MAC cloning.** Someone who sniffs a paying phone's MAC and clones it can share that phone's time. Binding MAC+IP means both devices then fight and break, and a customer's time only moves to a new IP that dnsmasq actually leased to that MAC (anything else is refused and logged as a possible MAC clone). A clone that also copies the IP still gets through; fully preventing this needs WPA2 per-user keys.
- **Client isolation.** Turn it on in the access point so customers can't see each other's devices.
- **Piso Pass codes are bearer secrets sent over the open WiFi.** Whoever has a code has its time, and the portal (like every vendo's) is plain HTTP, so someone sniffing nearby could copy a code as the phone claims it or buys more time, and use the time first. Tell customers to treat the code like cash. See [Piso Pass](#piso-pass-one-payment-all-your-locations) before turning it on.
- **SSH works only from the modem's side** (e.g. a laptop on the modem's own WiFi), never from the customer WiFi.

## Install on the Orange Pi

**0. Back up the SD card first** (balenaEtcher "clone", or `dd`). That's
your guaranteed way back to LPB.

**1. Build on the Mac:**

```bash
./scripts/build-pi.sh
```

**2. Copy to the Pi.** Connect your computer to the **modem's** network (not
the Piso WiFi), find the Pi's IP on the modem's device list, then:

```bash
scp dist/pisowifi-armv7.tar.gz root@PI_IP:/root/
```

**3. Check the board first.** `preflight.sh` changes nothing. It prints the
OS, kernel, network interfaces, what LPB starts at boot, and whether this
kernel accepts the firewall rules. Any `FAIL` line must be sorted out before
installing:

```bash
cd /root && tar xzf pisowifi-armv7.tar.gz && cd pisowifi && sudo ./preflight.sh
```

**4. Install:**

```bash
sudo ./install.sh
```

The installer asks before each step, and checks everything before touching
LPB, so customers keep using LPB until step 5 of 7 and are offline for only a
minute or two. It:
- checks the interfaces (and asks if `eth0`/`eth1` are named differently) and dry-runs the firewall
- installs nftables and dnsmasq if they're missing
- prompts for your dashboard password and your phone's WiFi address
- disables (doesn't delete) LPB's nginx, PHP, MariaDB and any `lpb` services, and lists other things LPB starts at boot (`rc.local`, cron, SSH keys) for you to check
- saves LPB's firewall and dnsmasq settings to `/root/lpb-backup-<date>/`
- runs the coin probe
- installs a fail-closed boot firewall and DHCP/DNS for the customer side, and starts the service

**5. Before you leave it:** check SSH works from the modem side, then
`sudo reboot` once and check that the portal opens on a phone and a coin
gives time.

Roll back any time with `sudo ./uninstall.sh /root/lpb-backup-<date>`.

## Wiring the coin slot (read this before plugging anything in)

The H3's GPIO pins are **3.3 V only, and 5 V or 12 V on a pin kills the chip.**
Your acceptor runs on 12 V. Its COIN output is normally *open-collector*,
meaning it only pulls the wire to ground. That is safe with the Pi's pull-up,
and it's how LPB had it wired.

**Keep the existing wiring.** It already worked with LPB. Before rewiring
anything, measure the coin wire against ground with the Pi disconnected:

| Measurement | Meaning |
|---|---|
| ~0 V, or floating | Safe to connect. |
| 5–12 V | Put a PC817 optocoupler in between (best). A resistor divider also works: 10k on top and 3.3k to ground for 12 V, or 10k and 20k for 5 V. Never connect it straight to the pin. |

Also connect the 12 V supply's ground to the Pi's ground. The Pi's built-in
pull-up is weak; if coins are missed or counted twice, add a 10k resistor
from the coin pin to 3.3 V (header pin 1).

**Pin choice.** Only the PA and PG pins (header pins 3, 5, 7, 8, 10, 11, 13,
15, 22, 26–29, 31–33, 35–38, 40) can signal each pulse instantly. Pins 12, 16,
18, 19, 21, 23 and 24 work too, but are checked every millisecond instead.

**Program the acceptor** for 1 pulse = ₱1 (₱1 = 1, ₱5 = 5, ₱10 = 10, ₱20 = 20 pulses).
That's the usual LPB setting. If yours differs, change `pesos_per_pulse`.
Each coin arrives as one burst of pulses; a quiet gap (`coin_gap_ms`, default
250 ms) ends the burst, which is how the box knows which coin it was. If two
quick coins ever show up as one, lower it; if one coin shows up as two, raise it.

**Find the pin:**

```bash
sudo systemctl stop pisowifi && sudo pisowifi coin-probe
```

Drop a coin and it prints something like
`header pin 3 (PA12): 5 pulses, each 48–52 ms long → set [coin] line = 12`.
Put that number in `/etc/pisowifi/config.toml` under `[coin] line = …`, then:

```bash
sudo systemctl start pisowifi
```

**Optional: check pulse widths.** `coin-probe` prints how long each pulse
lasts (ALLAN: usually 20–100 ms). Once you've seen yours, set
`pulse_width_ms = [15, 150]` (shortest, longest) under `[coin]`: pulses
outside that range, like electrical noise or a wire held low, are then
ignored and logged instead of counted. Leave it out until you've measured.

**Optional: gate the acceptor.** Wire a relay or transistor to the acceptor's
power (or INHIBIT input) and set `enable_line`. The slot then only accepts
coins after a customer taps *Insert coin*, so no more "unclaimed" coins.

## Daily use

| Task | How |
|---|---|
| Dashboard | `https://10.0.0.1:8443/` from a phone on the WiFi. Accept the certificate once **after checking the fingerprint** from `sudo pisowifi admin-info`. |
| Lock the dashboard to your phone | Turn off "private address" for this network on the phone, then `sudo pisowifi allow-mac a4:5e:60:11:22:33` and `sudo systemctl restart pisowifi`. Locked out? Open the dashboard anyway: the page shows the address to allow. |
| Logs | `journalctl -u pisowifi -f` |
| Backup | `sudo pisowifi backup --out /root/pisowifi-backup.db` makes a passphrase-encrypted copy. Keep it off the box. It includes the vendo's Piso Pass key, so a replacement board restored from it stays in your chain without re-joining. |
| Restore onto a replacement board | Stop the service, move `/var/lib/pisowifi/pisowifi.db` aside, then run `sudo pisowifi restore --from …`. |
| Changed `config.toml`? | Restart (`sudo systemctl restart pisowifi`). If you changed interfaces or ports, also refresh the boot firewall: `sudo sh -c '{ echo "flush ruleset"; pisowifi print-firewall; } > /etc/nftables.conf'`. |
| Give someone time | Dashboard › Devices. Works for any device on the WiFi, even with no time yet. A paused customer stays paused: the time waits with theirs. |
| Coin slot offline | The dashboard says so, and the portal refuses coins instead of losing them. Check `[coin]` in the config and `journalctl -u pisowifi`. |
| Dashboard certificate | Valid for 800 days and renewed automatically a month before it expires; your browser then warns once more, so check the new fingerprint with `sudo pisowifi admin-info`. |
| Reset the password | `sudo pisowifi set-password`. Use 4+ random words or 14+ random characters, used nowhere else. Never reuse the old LPB password: LPB stored it in plain text in MariaDB on the SD card. |

Because the DB key comes from the chip, **a dead board means the database
can only be recovered from a `backup` file.** Make one after setup and
whenever you change rates, and before any kernel upgrade (`apt upgrade`):
the database stays locked to whichever key opened it first, and a kernel
that can no longer read the chip ID stops the service with a clear message
rather than starting an empty database.

## Develop on the Mac

```bash
cargo run -- --dev run
```

| Address | What |
|---|---|
| `http://127.0.0.1:8080` | Customer portal |
| `http://127.0.0.1:8443` | Dashboard (dev password: `dev-data/dev-password.txt`) |

Dev mode fakes the firewall and the coin slot. The dashboard shows *Test ₱1/₱5/₱10* buttons.

Run the tests with `cargo test`. They cover the encryption-at-rest check,
wrong-key rejection, one-time vouchers, backup round-trip, Argon2, session
binding, lockout, rate math, ARP and lease parsing, the firewall rules and
their self-repair, coins surviving a power cut, the MAC-clone check, the
dashboard lock, Piso Pass (transfers between two vendos against a real hub
over HTTP, syncs crossing claims, lease caps, pruning, offline vendos) and
wallet passes (JWT claims, .pkpass manifest and signature, single-use links).

To try wallet passes locally, write a hub config whose `[wallet]` section
points at the throwaway keys in `testdata/wallet/` and run
`cargo run -- --dev hub --hub-config my-hub.toml run`. The passes it makes
are signed with test keys, so real phones refuse them.

## Piso Pass: one payment, all your locations

Run several vendos? Customers' time can follow them. Pay ₱20 at your store
in Barangay A, use an hour, and the remaining time is waiting at your
branch in Barangay B.

A vendo that never joins a hub is unaffected: all Piso Pass code stays
switched off.

```
 Vendo A ──┐  outbound HTTPS only, every request signed (Ed25519)
 Vendo B ──┼──────────────►  Hub (small cloud server, encrypted DB)
 Vendo C ──┘                 holds every pass's time balance
```

- **The pass.** The first time a phone pays, it gets a Piso Pass
  (`PASS-XXXX-XXXX-XXXX-XXXX`). The pass is saved in the phone's browser and
  shown on screen to screenshot. All vendos use the same portal address
  (10.0.0.1), so a phone's saved pass is picked up automatically at any of
  your locations. It can also be typed into *Have a voucher or Piso Pass?*.
  Phones keep passes per hub, so another owner's vendo never sees yours, and
  a saved pass is never deleted because something went wrong.
- **Moving between locations.** A vendo holds a pass's time while the customer is
  there and reports it to the hub every 30 s. Claiming the pass somewhere
  else moves the time there, and the old vendo cuts that phone off on its next
  report. Running time keeps counting. Paused time moves intact and stays
  paused until the customer taps *Resume*, so a phone that joins the WiFi in
  a pocket doesn't use it up. Buying coins with a pass saved on the phone adds
  to that pass; it never makes a second one.
- **If the hub is down**, every vendo keeps working on its own. Coins and
  vouchers work as usual, and passes resume syncing when the hub is back.
  One exception: a vendo that hasn't reached the hub for 2 minutes won't
  restart *paused* pass time, because the pass may have been claimed at
  another location meanwhile. The customer sees a message to try again; their
  time is safe on the pass.
- **Wallet passes.** Optionally, customers can add the pass to Google Wallet
  or Apple Wallet (see [Wallet passes](#wallet-passes)).
- **Security.** Vendos enrol with a single-use code and an Ed25519 key that
  never leaves the box. Requests are signed over the method, path, time,
  nonce and body hash; stale (±5 min) or replayed requests are refused, and
  the hub checks who is asking before it reads a request. Vendos never store
  pass codes, and the hub only stores HMACs of them.
- **A stolen vendo.** Its signing key is kept inside the encrypted database,
  which is locked to the board's chip, so a copied SD card alone doesn't give
  it away (unless the chip ID can't be read and the service warned at start
  that it fell back to a key file on the card). (Older versions kept it in a plain `hub-signing.key` file; it is
  moved into the database and the file removed on the first start.) Someone
  with the whole box does have it, but the hub only lets a vendo's reports
  *lower* a pass's time, or raise it by sales the vendo reported for that
  pass, so the key can't mint free time, and every refused report is logged.
  If a box goes missing, run `pisowifi hub remove-vendo` for it straight away.

### Set up the hub (once)

1. **Get a server and a domain.** Any small Linux VPS works (1 vCPU / 512 MB is plenty). Point a DNS name such as `hub.yourstore.ph` at it.
2. **Install the hub.** Build with `./scripts/build-pi.sh` (it also makes `dist/pisowifi-hub-x86_64.tar.gz`), copy that bundle to the server, then:

   ```bash
   sudo install -m 0755 pisowifi /usr/local/bin/ && sudo mkdir -p /etc/pisowifi && sudo cp hub.example.toml /etc/pisowifi/hub.toml && sudo cp pisowifi-hub.service /etc/systemd/system/ && sudo systemctl enable --now pisowifi-hub
   ```

3. **Turn on HTTPS.** Install Caddy, put your domain in `Caddyfile`, and copy it to `/etc/caddy/Caddyfile`. Caddy gets the certificate automatically.

### Add each vendo

1. **On the hub:**

   ```bash
   sudo pisowifi hub --hub-config /etc/pisowifi/hub.toml add-vendo --name "Barangay A"
   ```

2. **On that vendo,** using the printed code, then restart the service:

   ```bash
   sudo pisowifi join-hub --url https://hub.yourstore.ph --code CODE
   ```

3. **Check it.** The dashboard's **Settings → Piso Pass locations** shows the connection.

To see every vendo, its last sync and the passes it holds, run `pisowifi hub vendos` on the hub. `hub remove-vendo --id …` retires a box and returns its customers' time to their passes.

`leave-hub` (or joining another hub) turns Piso Pass off on that vendo after
the restart: phones there keep the time they have, but it no longer follows
them. Retire a vendo with both `leave-hub` and `hub remove-vendo`, when no
customer there has pass time running: time held there at that moment stays
on the phones *and* goes back to the passes.

## Wallet passes

Customers can keep their Piso Pass in **Google Wallet** (Android, Samsung
included) or **Apple Wallet** (iPhone), next to their cards, and use it at any
of your locations.

> **Not verified yet.** This follows Google's and Apple's published formats and
> is tested with throwaway keys: the JWT's claims and RS256 signature, the
> .pkpass manifest hashes, its CMS signature (also checked with
> `openssl smime -verify`) and the one-time links. It has **not** been tried
> with a real Google issuer account or Apple pass certificate, or on a real
> phone. Add a pass to your own phone before telling customers about it. See
> [what isn't verified](#what-isnt-verified-yet) below.

How it works:

- After paying, the Piso Pass sheet shows **Add to Apple Wallet** on iPhones
  and **Add to Google Wallet** on Android, or both on other devices. Buttons
  only appear for wallets the hub has set up.
- The vendo asks the hub (a signed request). The hub checks the code, builds
  and signs the pass, and returns a link that works **once, for 10 minutes**.
  The phone downloads the pass from the hub over the internet, so the
  customer's time must be running. Links never contain the code.
- The pass shows the code as a QR code and as text. Its **Use my time** link
  opens `http://10.0.0.1/?pass=CODE`: on any of your vendos' WiFi, the portal
  asks before claiming it (a planted link can't collect someone's time), then
  saves the pass on the phone. The pass doesn't show the balance, which
  changes every second.
- The Google key and Apple certificate live **only on the hub**. Vendos never
  see them. On the hub, systemd hands them over with `LoadCredential`, so the
  files can stay readable by root only.

Both wallets are configured in the `[wallet]` section of `hub.toml` (see
`hub.example.toml`). Each one turns on only when all of its lines are set, and
the hub checks every key and certificate when it starts.

### Google Wallet (free)

1. **Become an issuer.** Sign in to the [Google Pay & Wallet Console](https://pay.google.com/business/console)
   with your business Google account, open **Google Wallet API** and complete
   the business profile. Note your **Issuer ID** (a long number).
2. **Turn on the API.** In the [Google Cloud console](https://console.cloud.google.com/),
   create a project and enable the **Google Wallet API**.
3. **Make a service account.** In Cloud console → **IAM & Admin → Service
   accounts**, create one (it needs no roles), then **Keys → Add key → JSON**.
   This downloads a key file. Keep it secret: it can issue passes in your name.
4. **Let it issue passes.** Back in the Pay & Wallet Console → **Users**,
   invite the service account's email (`…@….iam.gserviceaccount.com`) with
   **Developer** access.
5. **Give the key to the hub:**

   ```bash
   sudo install -D -m 0600 key.json /etc/pisowifi/wallet/google-service-account.json
   ```

   In `/etc/pisowifi/hub.toml`, uncomment `[wallet]`, `google_issuer_id`,
   `google_service_account_json` and, to show your logo, `public_url`. In
   `/etc/systemd/system/pisowifi-hub.service`, uncomment the `google-wallet.json`
   `LoadCredential` line. Then restart:

   ```bash
   sudo systemctl daemon-reload && sudo systemctl restart pisowifi-hub && journalctl -u pisowifi-hub -n 5
   ```

   The log should say `Google Wallet on`.
6. **Try it, then go live.** New issuer accounts start in demo mode, where only
   you and test accounts you add can save passes. Once a pass works on your own
   phone, use **Request publishing access** in the console. The pass class
   (`<issuer id>.piso_pass`) is created automatically the first time a pass is saved.

### Apple Wallet (US$99 a year)

1. **Join the [Apple Developer Program](https://developer.apple.com/programs/).**
   Your **Team ID** is under *Membership details*.
2. **Register a Pass Type ID.** In *Certificates, Identifiers & Profiles* →
   **Identifiers → +** → **Pass Type IDs**, e.g. `pass.ph.yourstore.pisopass`.
3. **Make a key and signing request** on any computer with OpenSSL:

   ```bash
   openssl req -new -newkey rsa:2048 -nodes -keyout pass-key.pem -out pass.csr -subj "/CN=Piso Pass"
   ```

4. **Get the certificate.** *Certificates → +* → **Pass Type ID Certificate**,
   pick your pass type ID, upload `pass.csr`, download `pass.cer`, then:

   ```bash
   openssl x509 -inform der -in pass.cer -out pass-cert.pem
   ```

   Made the request in Keychain Access instead? Export the certificate with
   its key as `Certificates.p12`, then split it (OpenSSL 3 may need `-legacy`):

   ```bash
   openssl pkcs12 -in Certificates.p12 -clcerts -nokeys -out pass-cert.pem && openssl pkcs12 -in Certificates.p12 -nocerts -nodes -out pass-key.pem
   ```

5. **Get Apple's intermediate certificate.** From
   [Apple PKI](https://www.apple.com/certificateauthority/), download the
   *Worldwide Developer Relations* certificate that issued yours (currently
   **G4**), then `openssl x509 -inform der -in AppleWWDRCAG4.cer -out wwdr.pem`.
   The hub refuses to start with the wrong one and says so.
6. **Give them to the hub:**

   ```bash
   sudo install -D -m 0600 -t /etc/pisowifi/wallet/ pass-cert.pem pass-key.pem wwdr.pem
   ```

   Set the `apple_*` lines in `hub.toml`, uncomment the three Apple
   `LoadCredential` lines in the service file, and restart as above. The log
   should say `Apple Wallet on`. Add up to 10 `[[wallet.locations]]` so iPhones
   can suggest the pass near your vendos (depending on the iOS version).
7. **Renew yearly.** The pass certificate lasts one year. The hub warns 30 days
   before it expires. If it lapses, Apple Wallet turns off (the hub and Piso
   Pass keep running) until you repeat steps 3 to 6. Passes customers already
   added stay in their wallets.

### Samsung Wallet (later)

Samsung phones run Android, so they already get **Add to Google Wallet**
(Google Wallet is preinstalled on most of them, or free from the Play Store).
Samsung's own Samsung Wallet needs a partner account that Samsung approves,
with its own card templates and keys, so it isn't built yet. When you have
that approval, it can plug into the same `/v1/wallet` hub endpoint as a third
wallet.

### What isn't verified yet

- **The real services.** Without a Google issuer account and an Apple pass
  certificate, it's untested whether Google accepts the save link (the JWT
  creates the pass class itself, and the *Use my time* link is plain `http://`)
  and whether iPhones accept the .pkpass. The menu names in the steps above
  may also have changed.
- **Real phones.** The captive-portal pop-up browsers on iOS and Android may
  not open a .pkpass download or Google's save page. If the buttons do nothing
  there, customers need to open `10.0.0.1` in Safari or Chrome. Also untested:
  whether wallet apps open the `http://10.0.0.1` link in the browser.
- **Hub deployment.** The systemd `LoadCredential` setup hasn't been tried on a
  real server.
- **Badges.** Apple's and Google's brand guidelines ask for their official
  *Add to Wallet* badges. The buttons here match the portal's style. Swap in
  the official artwork before a wide launch.

## Roadmap (the LPB feature list)

| Feature | Plan |
|---|---|
| Unlimited users / sub-accounts | No user limit today. Sub-accounts (partners who see only their vendo's sales) come next, with per-account roles. |
| Unlimited PPPoE | `accel-ppp` or `pppoe-server` on a VLAN, with monthly accounts managed from the dashboard. |
| VLAN + auto-select coinslot | Each sub-vendo on its own VLAN (managed switch). The coin slot that's reserved follows the VLAN the customer is on. |
| GCash / Maya | Needs a merchant account (PayMongo / Xendit / Maya Business). Payment webhooks would credit time just like coins. |
| E-loading, cash-in, phone rental, PisoNet, kiosk | Separate modules on the same engine and ledger. |
| Remote monitoring | The hub now sees every vendo's heartbeat; a hub web dashboard (chain-wide earnings, offline alerts) is next. |
| Multi-owner networks | Phase 2 of Piso Pass: different owners sharing passes, with settlement for time used at someone else's vendo. |

## Layout

| Path | What's there |
|---|---|
| `src/` | Rust daemon: engine, firewall, gpio, db, auth, portal, admin, hub, wallet |
| `web/portal/` | Customer UI (compiled into the binary) |
| `web/admin/` | Owner dashboard (compiled into the binary) |
| `web/wallet/` | Wallet pass images (drawn from `web/shared/icon.svg`) |
| `testdata/wallet/` | Throwaway keys and certificates for the wallet tests |
| `deploy/` | install/uninstall scripts, systemd unit, dnsmasq config, sample config |
| `scripts/build-pi.sh` | Cross-compile and bundle for the Orange Pi |
