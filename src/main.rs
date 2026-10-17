#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod admin;
mod app;
mod auth;
mod brand;
mod config;
mod db;
mod demo;
mod devices;
mod engine;
mod firewall;
mod gpio;
mod health;
mod homeicon;
mod hub;
mod hubclient;
mod keys;
mod net;
mod portal;
mod pwpolicy;
mod rates;
mod release;
mod server;
mod tls;
mod update;
mod wallet;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

const DEFAULT_CONFIG: &str = "/etc/pisowifi/config.toml";

#[derive(Parser)]
#[command(name = "pisowifi", version, about = "Open-source, security-first Piso WiFi")]
struct Cli {
    /// Path to config.toml
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    /// Run on a laptop: no GPIO, no firewall, localhost only, data in ./dev-data
    #[arg(long, global = true)]
    dev: bool,
    /// Dev mode only: run extra vendos side by side (ports +N, ./dev-data-N)
    #[arg(long, global = true, default_value_t = 0)]
    instance: u16,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the vending service
    Run,
    /// Set or reset the dashboard password (needs shell access to the box)
    SetPassword,
    /// Find which GPIO line the coin acceptor is wired to
    CoinProbe {
        #[arg(long, default_value = "/dev/gpiochip0")]
        chip: String,
        /// Stop by itself after this many seconds (default: until Ctrl-C)
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Show the dashboard URL and TLS certificate fingerprint
    AdminInfo,
    /// Let a device open the dashboard (adds it to [admin] allow_macs; restart to apply)
    AllowMac {
        /// The device's WiFi MAC address, e.g. a4:5e:60:11:22:33
        mac: String,
    },
    /// Print the nftables ruleset this config would install
    PrintFirewall,
    /// Write a passphrase-encrypted backup of the database
    Backup {
        #[arg(long)]
        out: PathBuf,
    },
    /// Connect this vendo to your hub so Piso Passes work here
    JoinHub {
        #[arg(long)]
        url: String,
        /// One-time code from `pisowifi hub add-vendo` on the hub
        #[arg(long)]
        code: String,
    },
    /// Disconnect this vendo from the hub
    LeaveHub,
    /// Run or manage the hub (on your cloud server)
    Hub {
        /// Path to hub.toml [default: /etc/pisowifi/hub.toml; with --dev, built-in dev settings]
        #[arg(long)]
        hub_config: Option<PathBuf>,
        #[command(subcommand)]
        cmd: HubCmd,
    },
    /// Restore a backup onto this board (service must be stopped)
    Restore {
        #[arg(long)]
        from: PathBuf,
    },
    /// Check for, or install, an update signed with your release key
    Update(UpdateArgs),
    /// (scripts/make-release-key.sh) New release key: the public half goes to
    /// FILE, the secret (hex) to stdout, which must not be a terminal
    #[command(hide = true)]
    ReleaseKey {
        #[arg(long, value_name = "FILE")]
        public_out: PathBuf,
    },
    /// (CI) Sign a bundle in place with the secret key from an environment variable
    #[command(hide = true)]
    ReleaseSign {
        #[arg(long, default_value = "PISOWIFI_RELEASE_KEY")]
        key_env: String,
        /// Refuse unless the key matches this public key file (release-key.pub)
        #[arg(long)]
        expect_public: PathBuf,
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        target: String,
        #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
        version: String,
        #[arg(long, default_value = "0.0.0")]
        min_from: String,
        #[arg(long)]
        notes_file: Option<PathBuf>,
    },
    /// Check a signed bundle against a public key file
    #[command(hide = true)]
    ReleaseVerify {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        public_key: PathBuf,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        target: String,
    },
    /// (CI) Write the signed update list (<out-dir>/<channel>.json and .sig) for signed bundles
    #[command(hide = true)]
    ReleaseChannel {
        #[arg(long, default_value = "PISOWIFI_RELEASE_KEY")]
        key_env: String,
        /// Refuse unless the key matches this public key file (release-key.pub)
        #[arg(long)]
        expect_public: PathBuf,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(required = true)]
        bundles: Vec<PathBuf>,
    },
    /// Dev mode only: fill a fresh dev vendo with sample sales and devices
    #[command(hide = true)]
    DemoData,
}

#[derive(clap::Args)]
struct UpdateArgs {
    /// Only check whether a newer version is out
    #[arg(long, conflicts_with_all = ["file", "install"])]
    check: bool,
    /// Download the newest version and install it
    #[arg(long, conflicts_with = "file")]
    install: bool,
    /// (the hub's 3 AM timer) Like --install, quiet when up to date
    #[arg(long, hide = true, conflicts_with = "file")]
    auto: bool,
    /// Install a bundle file instead (it carries its signature), for a box without internet
    #[arg(long, value_name = "BUNDLE")]
    file: Option<PathBuf>,
    /// Use another update list (…/stable.json); on its own it installs from there
    #[arg(long, value_name = "URL", num_args = 0..=1, default_missing_value = "", conflicts_with = "file")]
    url: Option<String>,
    /// Show the installed version and how the last update went (the default)
    #[arg(long)]
    status: bool,
    /// (pisowifi-update.service) Install what's staged, or finish an interrupted update
    #[arg(long, hide = true)]
    run_pending: bool,
    /// (install.sh) Install from an unpacked bundle folder
    #[arg(long, hide = true, value_name = "DIR")]
    install_dir: Option<PathBuf>,
    /// (install.sh) Allow reinstalling the installed version
    #[arg(long, hide = true)]
    allow_same: bool,
}

impl From<UpdateArgs> for update::Args {
    fn from(a: UpdateArgs) -> Self {
        update::Args {
            check: a.check,
            install: a.install,
            auto: a.auto,
            file: a.file,
            url: a.url,
            run_pending: a.run_pending,
            install_dir: a.install_dir,
            allow_same: a.allow_same,
        }
    }
}

#[derive(Subcommand)]
enum HubCmd {
    /// Start the hub server
    Run,
    /// Create a one-time code to enrol a vendo (valid 24 h)
    AddVendo {
        #[arg(long)]
        name: String,
    },
    /// List enrolled vendos
    Vendos,
    /// Remove a vendo; time it was holding goes back to the passes
    RemoveVendo {
        #[arg(long)]
        id: String,
    },
    /// Check for, or install, an update signed with your release key
    Update(UpdateArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "pisowifi=info".into()),
        )
        .with_target(false)
        .init();
    // Everything we create (db, salt, keys) is owner-only.
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077);
    }

    let cmd = match cli.cmd {
        Cmd::Hub { hub_config, cmd } => {
            let cfg = match (hub_config, cli.dev) {
                (Some(path), _) => hub::HubConfig::load(&path)?,
                (None, true) => hub::HubConfig::dev(std::env::current_dir()?.join("dev-hub")),
                (None, false) => hub::HubConfig::load(Path::new("/etc/pisowifi/hub.toml"))?,
            };
            return run_hub(cfg, cmd, cli.dev);
        }
        Cmd::AllowMac { mac } => {
            if cli.dev {
                bail!("allow-mac edits the vendo's config file; dev mode doesn't use one");
            }
            let mac = net::Mac::parse(&mac).context("that isn't a WiFi (MAC) address like a4:5e:60:11:22:33")?;
            if config::add_admin_mac(&cli.config, mac)? {
                println!("{mac} can open the dashboard once the service restarts:\n  sudo systemctl restart pisowifi");
            } else {
                println!("{mac} is already allowed. Still locked out? Check the address on the locked page matches.");
            }
            return Ok(());
        }
        Cmd::ReleaseKey { public_out } => {
            use std::io::IsTerminal;
            if std::io::stdout().is_terminal() {
                bail!("this writes the secret key to stdout; pipe it somewhere instead (scripts/make-release-key.sh does)");
            }
            let (secret, public) = release::new_key();
            std::fs::write(&public_out, release::public_file(&public))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&public_out, std::fs::Permissions::from_mode(0o644))?;
            }
            print!("{}", secret.as_str());
            return Ok(());
        }
        Cmd::ReleaseSign { key_env, expect_public, bundle, kind, target, version, min_from, notes_file } => {
            let key = release::key_from_env(&key_env)?;
            let public = std::fs::read_to_string(&expect_public).with_context(|| format!("reading {}", expect_public.display()))?;
            if key.verifying_key() != release::parse_public(&public)? {
                bail!("{key_env} doesn't match {}, so vendos would refuse what it signs. Nothing was signed.", expect_public.display());
            }
            let notes = match notes_file {
                Some(f) => std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?,
                None => String::new(),
            };
            let args = release::SignArgs { kind: release::Kind::parse(&kind)?, target: &target, version: &version, min_from: &min_from, notes: &notes };
            let sha = release::sign_bundle(&key, &bundle, &args)?;
            println!("Signed {} (sha256 {sha})", bundle.display());
            return Ok(());
        }
        Cmd::ReleaseChannel { key_env, expect_public, channel, out_dir, bundles } => {
            let key = release::key_from_env(&key_env)?;
            let public = std::fs::read_to_string(&expect_public).with_context(|| format!("reading {}", expect_public.display()))?;
            if key.verifying_key() != release::parse_public(&public)? {
                bail!("{key_env} doesn't match {}, so vendos would refuse what it signs. Nothing was signed.", expect_public.display());
            }
            let c = release::write_channel(&key, &channel, &bundles, &out_dir)?;
            println!("Wrote {}/{channel}.json for version {} ({} bundles)", out_dir.display(), c.version, c.files.len());
            return Ok(());
        }
        Cmd::ReleaseVerify { bundle, public_key, kind, target } => {
            let key = release::parse_public(&std::fs::read_to_string(&public_key)?)?;
            let expect = release::Expect { kind: release::Kind::parse(&kind)?, target: &target, current: None, allow_same: false };
            let v = release::verify(&std::fs::read(&bundle)?, Some(&key), &expect)?;
            let m = &v.manifest;
            println!("OK: {} is version {} for {}, {} signed files", bundle.display(), m.version, m.target, m.files.len());
            return Ok(());
        }
        cmd => cmd,
    };
    let cfg = if cli.dev {
        let n = cli.instance;
        let dir = if n == 0 { "dev-data".to_string() } else { format!("dev-data-{n}") };
        let mut c = Config::dev(std::env::current_dir()?.join(dir));
        c.portal.listen.set_port(c.portal.listen.port() + n);
        c.admin.listen.set_port(c.admin.listen.port() + n);
        c
    } else {
        Config::load(&cli.config)?
    };
    // Read-only commands (used by preflight.sh) don't create anything; the
    // update command creates only what it needs (`--check` works unprivileged).
    if !matches!(cmd, Cmd::PrintFirewall | Cmd::CoinProbe { .. } | Cmd::Update(_)) {
        ensure_data_dir(&cfg.storage.data_dir)?;
    }

    match cmd {
        Cmd::Run => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(run(cfg, cli.dev)),
        Cmd::SetPassword => {
            let db = open_db(&cfg, cli.dev)?;
            println!("{}\n", pwpolicy::GUIDANCE);
            let a = zeroize::Zeroizing::new(rpassword::prompt_password(format!(
                "New dashboard password (at least {} characters): ",
                pwpolicy::MIN_LEN
            ))?);
            let b = zeroize::Zeroizing::new(rpassword::prompt_password("Repeat: ")?);
            if *a != *b {
                bail!("passwords don't match");
            }
            let name = db.get_setting("portal_name")?.unwrap_or_default();
            let ssid = db.get_setting("wifi_name")?.unwrap_or_default();
            let hash = auth::hash_password(&a, &[&name, &ssid]).map_err(|e| anyhow::anyhow!("{e} Nothing was changed."))?;
            db.set_setting(auth::PASSWORD_KEY, &hash)?;
            db.audit("shell", "password_set", "");
            println!("Password saved. Restart the service to sign out existing sessions.");
            Ok(())
        }
        Cmd::CoinProbe { chip, seconds } => gpio::probe(&chip, seconds.map(Duration::from_secs)),
        Cmd::AdminInfo => {
            let t = tls::load_or_create(&cfg.storage.data_dir, tls_names(&cfg))?;
            println!("Dashboard:   https://{}:{}/", cfg.network.portal_ip, cfg.admin.listen.port());
            println!("Fingerprint: SHA-256 {}", t.fingerprint);
            println!("Check that your browser shows this same fingerprint the first time you connect.");
            Ok(())
        }
        Cmd::PrintFirewall => {
            print!("{}", firewall::base_ruleset(&cfg, &[])?);
            Ok(())
        }
        Cmd::Backup { out } => {
            let db = open_db(&cfg, cli.dev)?;
            let a = rpassword::prompt_password("Backup passphrase (min 12 chars): ")?;
            if a.chars().count() < 12 || a != rpassword::prompt_password("Repeat: ")? {
                bail!("passphrases must match and be at least 12 characters");
            }
            db.export_backup(&out, &a)?;
            println!("Backup written to {}. Keep the passphrase safe; it can't be recovered.", out.display());
            Ok(())
        }
        Cmd::JoinHub { url, code } => {
            let db = open_db(&cfg, cli.dev)?;
            let r = hubclient::join(&db, &cfg.storage.data_dir, &url, &code, cli.dev)?;
            println!("Joined the hub as \"{}\" ({}). Restart the service to start syncing.", r.name, r.vendo_id);
            Ok(())
        }
        Cmd::LeaveHub => {
            hubclient::leave(&open_db(&cfg, cli.dev)?)?;
            println!("Left the hub. Restart the service. Phones keep the time they have here; it just stops being a Piso Pass.");
            Ok(())
        }
        Cmd::Update(a) => {
            let p = update::Profile::vendo(&cfg, cli.dev);
            let configured = || update::vendo_url(&open_db(&cfg, cli.dev)?);
            update::cli(&p, a.into(), cli.dev, &configured)
        }
        Cmd::DemoData => {
            if !cli.dev {
                bail!("demo-data only works with --dev: it would mix made-up sales into real ones");
            }
            demo::fill(&open_db(&cfg, true)?)
        }
        Cmd::Hub { .. } | Cmd::AllowMac { .. } | Cmd::ReleaseKey { .. } | Cmd::ReleaseSign { .. } | Cmd::ReleaseVerify { .. } | Cmd::ReleaseChannel { .. } => unreachable!(),
        Cmd::Restore { from } => {
            let source = if keys::chip_id_readable(cli.dev) { keys::Source::Chip } else { keys::Source::File };
            let k = keys::derive(&cfg.storage.data_dir, source, false)?;
            let pass = rpassword::prompt_password("Backup passphrase: ")?;
            db::import_backup(&from, &pass, &db_path(&cfg), &k.key)?;
            keys::record_source(&cfg.storage.data_dir, source)?;
            println!("Restored. Start the service again.");
            Ok(())
        }
    }
}

fn run_hub(cfg: hub::HubConfig, cmd: HubCmd, dev: bool) -> Result<()> {
    if let HubCmd::Update(a) = cmd {
        let p = update::Profile::hub(&cfg.data_dir, cfg.listen, dev);
        return update::cli(&p, a.into(), dev, &|| update::hub_url(cfg.update_url.as_deref(), dev));
    }
    ensure_data_dir(&cfg.data_dir)?;
    match &cmd {
        HubCmd::Run => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(hub::run(cfg, dev)),
        HubCmd::AddVendo { name } => {
            let name = brand::clean_text(name, 40);
            let code = hub::HubDb::open(&cfg.data_dir)?.add_enroll_code(&name)?;
            println!("Enrolment code for \"{name}\" (valid 24 hours, single use):\n\n  {code}\n");
            println!("On that vendo run:\n  sudo pisowifi join-hub --url https://YOUR-HUB --code {code}");
            Ok(())
        }
        HubCmd::Vendos => {
            let db = hub::HubDb::open(&cfg.data_dir)?;
            let (passes, held, day) = db.summary()?;
            println!("{passes} passes · {} h of customer time held · ₱{day} in the last 24 h\n", held / 3600);
            for v in db.vendos()? {
                let seen = v.last_seen.map(|t| format!("{}s ago", db::now() - t)).unwrap_or_else(|| "never".into());
                println!("{:<20} {:<28} last sync {:<12} {} passes here", v.id, v.name, seen, v.leases);
            }
            Ok(())
        }
        HubCmd::RemoveVendo { id } => {
            if hub::HubDb::open(&cfg.data_dir)?.remove_vendo(id)? {
                println!("Removed {id}. Its customers' time is back on their passes.");
            } else {
                println!("No vendo {id}.");
            }
            Ok(())
        }
        HubCmd::Update(_) => unreachable!(),
    }
}

fn ensure_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn db_path(cfg: &Config) -> PathBuf {
    cfg.storage.data_dir.join("pisowifi.db")
}

fn open_db(cfg: &Config, dev: bool) -> Result<db::Db> {
    use keys::Source;
    let dir = &cfg.storage.data_dir;
    let exists = db_path(cfg).exists();
    let chip = keys::chip_id_readable(dev);
    let candidates = match keys::recorded_source(dir) {
        Some(s) => vec![s],
        None if !exists => vec![if chip { Source::Chip } else { Source::File }],
        // Made before the source was recorded: whichever key opens it.
        None => [(chip, Source::Chip), (dir.join("db.key").exists(), Source::File)]
            .into_iter()
            .filter_map(|(ok, s)| ok.then_some(s))
            .collect(),
    };
    let mut last = anyhow::anyhow!("no key material for {}", db_path(cfg).display());
    for source in candidates {
        if source == Source::File && !dev {
            tracing::warn!(
                "no readable SoC ID; using a key file on the SD card. \
                 The database is still encrypted, but someone with the card can decrypt it."
            );
        }
        let k = keys::derive(dir, source, exists)?;
        match db::open_migrating(&db_path(cfg), &k.key, &k.legacy) {
            Ok(db) => {
                keys::record_source(dir, source)?;
                tracing::info!("database unlocked (key from {})", k.source);
                return Ok(db);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn tls_names(cfg: &Config) -> Vec<String> {
    vec![cfg.network.portal_ip.to_string(), "pisowifi.local".into()]
}

/// Listens even before the address is on the interface (IP_FREEBIND), so a
/// LAN adapter that comes up late doesn't stop the service.
fn listen(addr: std::net::SocketAddr) -> Result<TcpListener> {
    let sock = if addr.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    sock.set_reuseaddr(true)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let on: libc::c_int = 1;
        // SAFETY: plain setsockopt on a socket we own, with a correctly sized int.
        let r = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_FREEBIND,
                &on as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if r != 0 {
            tracing::warn!("IP_FREEBIND: {}", std::io::Error::last_os_error());
        }
    }
    sock.bind(addr).with_context(|| format!("binding {addr} (is something else, like LPB's web server, using it?)"))?;
    Ok(sock.listen(1024)?)
}

async fn run(cfg: Config, dev: bool) -> Result<()> {
    let db = Arc::new(open_db(&cfg, dev)?);
    // Everything that can fail goes before the firewall is replaced, so a
    // failing start can't cut paying customers off over and over.
    let portal_l = listen(cfg.portal.listen)?;
    let admin_l = listen(cfg.admin.listen)?;
    let admin_tls = if dev { None } else { Some(tls::load_or_create(&cfg.storage.data_dir, tls_names(&cfg))?) };
    let enable = gpio::Enable::open(&cfg.coin)?.map(|e| (e, cfg.coin.enable_line.unwrap()));
    let (fw, fw_start) = firewall::Firewall::new(&cfg, dev);
    // Pass links from another hub enrolment (join-hub, leave-hub) are dropped
    // before the engine loads them.
    if db.reset_pass_state_if_hub_changed(db.get_setting("hub_vendo_id")?.as_deref())? {
        tracing::info!("Piso Pass: hub changed; old pass links dropped (customers keep their time here)");
    }
    // In dev mode coins come from the dashboard's test buttons.
    let reader_ok = Arc::new(std::sync::atomic::AtomicBool::new(dev));
    let engine = engine::Engine::new(
        db.clone(),
        fw,
        enable,
        cfg.coin.pesos_per_pulse,
        cfg.coin.insert_window_secs,
        reader_ok.clone(),
        (!dev).then(|| cfg.network.dhcp_leases.clone()),
    )?;
    // Base ruleset and every restored customer in one transaction.
    let db2 = db.clone();
    fw_start
        .start(&engine.grants(), move |what| db2.audit("system", "firewall_restored", what))
        .await?;

    // Coin pulses -> engine.
    let (tx, mut rx) = mpsc::unbounded_channel();
    if !dev {
        gpio::spawn_reader(&cfg.coin, tx, reader_ok);
    }
    // The acceptor sends one burst per coin (₱5 = 5 pulses). A quiet gap
    // ends the burst, which tells us exactly which coin went in.
    let e = engine.clone();
    let gap = Duration::from_millis(cfg.coin.coin_gap_ms);
    let ppp = cfg.coin.pesos_per_pulse;
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            e.pulse();
            let mut pulses = 1u32;
            while let Ok(Some(())) = tokio::time::timeout(gap, rx.recv()).await {
                pulses += 1;
            }
            e.on_coin(pulses.saturating_mul(ppp), pulses);
        }
    });

    // Firewall: every minute, check nothing else flushed or changed it.
    let e = engine.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await; // (fires at once; it was just installed)
        loop {
            tick.tick().await;
            e.reconcile_firewall();
        }
    });

    // Clock: expire sessions every second, checkpoint running clocks every 30 s.
    let e = engine.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut n = 0u32;
        loop {
            tick.tick().await;
            e.tick();
            n += 1;
            if n % 30 == 0 {
                e.persist();
            }
        }
    });

    // Piso Pass: heartbeat to the hub every 30 s.
    let hub_client = hubclient::HubClient::load(&db, &cfg.storage.data_dir)?;
    if let Some(hub) = hub_client.clone() {
        tracing::info!("Piso Pass: connected to hub {} as {}", hub.status().url, hub.status().name);
        let e = engine.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(if dev { 5 } else { 30 }));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(err) = hub.sync_once(&e).await {
                    tracing::warn!("hub sync: {err}");
                }
            }
        });
    }

    let fingerprint = admin_tls.as_ref().map(|t| t.fingerprint.clone()).unwrap_or_default();
    if db.get_setting(auth::PASSWORD_KEY)?.is_none() {
        if dev {
            let pw = hex::encode(rand::random::<[u8; 12]>());
            db.set_setting(auth::PASSWORD_KEY, &auth::hash_password(&pw, &[])?)?;
            let note = cfg.storage.data_dir.join("dev-password.txt");
            std::fs::write(&note, format!("Dev-mode dashboard password (Mac testing only):\n{pw}\n"))?;
            tracing::warn!("dev mode: dashboard password is {pw} (also saved to {})", note.display());
        } else {
            tracing::warn!("no dashboard password yet: run `sudo pisowifi set-password`");
        }
    }

    let updater = update::Updater::new(update::Profile::vendo(&cfg, dev), dev, db.clone());
    update::spawn_daily(updater.clone(), engine.clone());

    let state = Arc::new(app::AppState {
        updater,
        lan: cfg.lan_net()?,
        admin_macs: cfg.admin.allow_macs.iter().map(|m| net::Mac::parse(m)).collect::<Result<_>>()?,
        auth: auth::Auth::new(cfg.admin.session_hours),
        brand: brand::BrandStore::load(&db)?,
        devices: devices::DeviceBook::load(&db)?,
        hub: hub_client.clone(),
        engine: engine.clone(),
        db: db.clone(),
        fingerprint,
        cfg: cfg.clone(),
        dev,
    });

    let (stop_tx, stop_rx) = watch::channel(false);
    let scheme = if dev { "http" } else { "https" };
    tracing::info!("portal on http://{}  dashboard on {scheme}://{}", cfg.portal.listen, cfg.admin.listen);

    let p = tokio::spawn(server::serve(
        portal_l,
        portal::router(state.clone()),
        None,
        server::Limits { max_conns: cfg.portal.max_connections, per_ip: 24, request_timeout: Duration::from_secs(20) },
        None,
        stop_rx.clone(),
    ));
    // Devices not on admin.allow_macs are turned away before TLS, from a
    // pool of their own, so customers can't use up the owner's connections.
    let gate = (!state.admin_macs.is_empty()).then(|| {
        let st = state.clone();
        server::Gate {
            allow: Box::new(move |addr| st.identify(addr).is_ok_and(|(mac, _)| st.admin_macs.contains(&mac))),
            locked: admin::locked_router(state.clone()),
        }
    });
    let a = tokio::spawn(server::serve(
        admin_l,
        admin::router(state.clone()),
        admin_tls.map(|t| t.acceptor),
        // Uploads (logo, poster) get longer.
        server::Limits { max_conns: 32, per_ip: 8, request_timeout: Duration::from_secs(60) },
        gate,
        stop_rx,
    ));

    // An update is only kept once the new version gets this far.
    if let Err(e) = update::report_healthy(&cfg.storage.data_dir, !dev) {
        tracing::warn!("writing health report: {e:#}");
    }

    shutdown_signal().await;
    update::clear_health(&cfg.storage.data_dir);
    tracing::info!("shutting down; saving sessions");
    engine.finish_open_slot();
    engine.persist();
    let _ = stop_tx.send(true);
    let _ = tokio::join!(p, a);
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
