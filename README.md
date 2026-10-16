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
| Customer gets online without paying | nftables forward policy is **drop**. Only devices whose MAC **and** IP are both in the paid set are routed. If the service dies, the table stays: it **fails closed**. |
| Bypass over IPv6, DNS tunnels to outside resolvers | IPv6 from customers is dropped. Every DNS query is forced to the box's own resolver and rate-limited per device. |
| Customer pokes the ISP modem, SSH, or the box | Customers can reach only DHCP, DNS, the portal and the dashboard. Private ranges (192.168.x, 10.x, …) are unreachable. SSH works only from the modem side. |
| Stolen or cloned SD card | The DB key is derived from the **H3 chip's factory ID** plus a random salt, stretched with Argon2id (64 MiB, 3 passes) so guessing the ID is slow. The key is never stored, so the card can't be opened on another board. **Stealing the whole box (chip and card) still opens the database**; at that point the Argon2id-hashed admin password is what protects your account. |
| Admin password sniffed on the open WiFi | The dashboard is HTTPS only (TLS 1.2/1.3, rustls). The password is Argon2id-hashed (64 MiB, 3 passes). Sessions are bound to your IP with HttpOnly, Secure, SameSite=Strict cookies plus a CSRF token. |
| Password guessing from the customer WiFi | The installer locks the dashboard to your phone's MAC, and the dashboard port has its own flood limit. 5 wrong tries locks a device out for 15 minutes. After 20 failures box-wide (from any addresses), only one attempt per 30 s is allowed, and that is logged. Common passwords (the 10k list, sequences, keyboard runs, the WiFi or store name) are refused. |
| Customer "claims" a fresh box | The password can only be set from the device's shell (`set-password`), never from the web. |
| Web attacks (XSS, CSRF, clickjacking, DNS rebinding) | Strict CSP (no inline scripts), no `innerHTML`, custom-header CSRF checks, Host-header checks, `frame-ancestors 'none'`. |
| Slowloris / connection floods | Connection caps, 10 s header timeout, 4–16 KB body limits, one Argon2 hash at a time. |
| Ledger tampering | All SQL uses bound parameters. Voucher codes are stored hashed (shown once). Test coins exist only in dev mode. Every owner action goes to the audit log. |
| Compromised service | The systemd unit drops all capabilities except networking, the filesystem is read-only except `/var/lib/pisowifi`, and only GPIO devices are allowed. |

**Known limits** (true of every open-WiFi vendo, LPB included):
- **MAC cloning.** Someone who sniffs a paying phone's MAC and clones it can share that phone's time. Binding MAC+IP means both devices then fight and break. Fully preventing this needs WPA2 per-user keys.
- **Client isolation.** Turn it on in the access point so customers can't see each other's devices.

## Install on the Orange Pi

**0. Back up the SD card first** (balenaEtcher "clone", or `dd`). That's
your guaranteed way back to LPB.

**1. Build on the Mac:**

```bash
./scripts/build-pi.sh
```

**2. Copy to the Pi.** Find its IP on your modem's device list, or run `ip addr` on the Pi:

```bash
scp dist/pisowifi-armv7.tar.gz root@PI_IP:/root/
```

**3. On the Pi:**

```bash
cd /root && tar xzf pisowifi-armv7.tar.gz && cd pisowifi && sudo ./install.sh
```

The installer asks before each step. It:
- disables (doesn't delete) LPB's nginx, PHP, MariaDB and any `lpb` services
- saves the old firewall and dnsmasq configs to `/root/lpb-backup-<date>/`
- installs a fail-closed boot firewall and DHCP/DNS for `eth1`
- prompts for your dashboard password
- runs the coin probe and starts the service

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
| 5–12 V | Put a PC817 optocoupler (or a 10k/4.7k voltage divider) in between. |

Also connect the 12 V supply's ground to the Pi's ground.

**Program the acceptor** for 1 pulse = ₱1 (₱1 = 1, ₱5 = 5, ₱10 = 10, ₱20 = 20 pulses).
That's the usual LPB setting. If yours differs, change `pesos_per_pulse`.
Each coin arrives as one burst of pulses; a quiet gap (`coin_gap_ms`, default
250 ms) ends the burst, which is how the box knows which coin it was. If two
quick coins ever show up as one, lower it; if one coin shows up as two, raise it.

**Find the pin:**

```bash
sudo systemctl stop pisowifi && sudo pisowifi coin-probe
```

Drop a coin and it prints something like `line 12 = PA12, header pin 3`. Put
that number in `/etc/pisowifi/config.toml` under `[coin] line = …`, then:

```bash
sudo systemctl start pisowifi
```

**Optional: gate the acceptor.** Wire a relay or transistor to the acceptor's
power (or INHIBIT input) and set `enable_line`. The slot then only accepts
coins after a customer taps *Insert coin*, so no more "unclaimed" coins.

## Daily use

| Task | How |
|---|---|
| Dashboard | `https://10.0.0.1:8443/` from a phone on the WiFi. Accept the certificate once **after checking the fingerprint** from `sudo pisowifi admin-info`. |
| Lock the dashboard to your phone | Add your phone's MAC to `admin.allow_macs` in the config. Turn off "private address" for this network on the phone. |
| Logs | `journalctl -u pisowifi -f` |
| Backup | `sudo pisowifi backup --out /root/pisowifi-backup.db` makes a passphrase-encrypted copy. Keep it off the box. |
| Restore onto a replacement board | Stop the service, move `/var/lib/pisowifi/pisowifi.db` aside, then run `sudo pisowifi restore --from …`. |
| Reset the password | `sudo pisowifi set-password`. Use 4+ random words or 14+ random characters, used nowhere else. Never reuse the old LPB password: LPB stored it in plain text in MariaDB on the SD card. |

Because the DB key comes from the chip, **a dead board means the database
can only be recovered from a `backup` file.** Make one after setup and
whenever you change rates.

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
binding, lockout, rate math, ARP parsing and the firewall rules.

## Piso Pass: one payment, all your locations

Run several vendos? Customers' time can follow them. Pay ₱20 at your store
in Barangay A, use an hour, and the remaining time is waiting at your
branch in Barangay B.

```
 Vendo A ──┐  outbound HTTPS only, every request signed (Ed25519)
 Vendo B ──┼──────────────►  Hub (small cloud server, encrypted DB)
 Vendo C ──┘                 holds every pass's time balance
```

- **The pass.** The first time a phone pays, it gets a Piso Pass
  (`PASS-XXXX-XXXX-XXXX-XXXX`). The pass is saved in the phone's browser and
  shown on screen to screenshot. All vendos use the same portal address
  (10.0.0.1), so a phone's saved pass is picked up automatically at any
  location. It can also be typed into *Have a voucher or Piso Pass?*.
- **Moving between locations.** A vendo holds a pass's time while the customer is
  there and reports it to the hub every 30 s. Claiming the pass somewhere
  else moves the time there, and the old vendo cuts that phone off on its next
  report. Paused time moves intact; running time keeps counting.
- **If the hub is down**, every vendo keeps working on its own. Coins and
  vouchers work as usual, and passes resume syncing when the hub is back.
- **Security.** Vendos enrol with a single-use code and an Ed25519 key that
  never leaves the box. Requests are signed over the method, path, time,
  nonce and body hash; stale (±5 min) or replayed requests are refused.
  Vendos never store pass codes, and the hub only stores HMACs of them.

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
| `src/` | Rust daemon: engine, firewall, gpio, db, auth, portal, admin |
| `web/portal/` | Customer UI (compiled into the binary) |
| `web/admin/` | Owner dashboard (compiled into the binary) |
| `deploy/` | install/uninstall scripts, systemd unit, dnsmasq config, sample config |
| `scripts/build-pi.sh` | Cross-compile and bundle for the Orange Pi |
