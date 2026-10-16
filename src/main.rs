#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod admin;
mod app;
mod auth;
mod brand;
mod config;
mod db;
mod engine;
mod firewall;
mod gpio;
mod hub;
mod hubclient;
mod keys;
mod net;
mod portal;
mod pwpolicy;
mod rates;
mod server;
mod tls;
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
    },
    /// Show the dashboard URL and TLS certificate fingerprint
    AdminInfo,
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

    if let Cmd::Hub { hub_config, cmd } = &cli.cmd {
        let cfg = match (hub_config, cli.dev) {
            (Some(path), _) => hub::HubConfig::load(path)?,
            (None, true) => hub::HubConfig::dev(std::env::current_dir()?.join("dev-hub")),
            (None, false) => hub::HubConfig::load(Path::new("/etc/pisowifi/hub.toml"))?,
        };
        return run_hub(cfg, cmd);
    }
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
    ensure_data_dir(&cfg.storage.data_dir)?;

    match cli.cmd {
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
        Cmd::CoinProbe { chip } => gpio::probe(&chip),
        Cmd::AdminInfo => {
            let t = tls::load_or_create(&cfg.storage.data_dir, tls_names(&cfg))?;
            println!("Dashboard:   https://{}:{}/", cfg.network.portal_ip, cfg.admin.listen.port());
            println!("Fingerprint: SHA-256 {}", t.fingerprint);
            println!("Check that your browser shows this same fingerprint the first time you connect.");
            Ok(())
        }
        Cmd::PrintFirewall => {
            print!("{}", firewall::base_ruleset(&cfg)?);
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
            println!("Left the hub. Restart the service.");
            Ok(())
        }
        Cmd::Hub { .. } => unreachable!(),
        Cmd::Restore { from } => {
            let k = keys::derive(&cfg.storage.data_dir, cli.dev)?;
            let pass = rpassword::prompt_password("Backup passphrase: ")?;
            db::import_backup(&from, &pass, &db_path(&cfg), &k.key)?;
            println!("Restored. Start the service again.");
            Ok(())
        }
    }
}

fn run_hub(cfg: hub::HubConfig, cmd: &HubCmd) -> Result<()> {
    ensure_data_dir(&cfg.data_dir)?;
    match cmd {
        HubCmd::Run => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(hub::run(cfg)),
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
    let k = keys::derive(&cfg.storage.data_dir, dev)?;
    let db = db::open_migrating(&db_path(cfg), &k.key, &k.legacy)?;
    tracing::info!("database unlocked (key from {})", k.source);
    Ok(db)
}

fn tls_names(cfg: &Config) -> Vec<String> {
    vec![cfg.network.portal_ip.to_string(), "pisowifi.local".into()]
}

async fn run(cfg: Config, dev: bool) -> Result<()> {
    let db = Arc::new(open_db(&cfg, dev)?);
    let fw = firewall::Firewall::start(&cfg, dev).await?;
    let enable = gpio::Enable::open(&cfg.coin)?.map(|e| (e, cfg.coin.enable_line.unwrap()));
    let engine = engine::Engine::new(db.clone(), fw, enable, cfg.coin.pesos_per_pulse, cfg.coin.insert_window_secs)?;

    // Coin pulses -> engine.
    let (tx, mut rx) = mpsc::unbounded_channel();
    if !dev {
        gpio::spawn_reader(&cfg.coin, tx);
    }
    // The acceptor sends one burst per coin (₱5 = 5 pulses). A quiet gap
    // ends the burst, which tells us exactly which coin went in.
    let e = engine.clone();
    let gap = Duration::from_millis(cfg.coin.coin_gap_ms);
    let ppp = cfg.coin.pesos_per_pulse;
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            let mut pulses = 1u32;
            while let Ok(Some(())) = tokio::time::timeout(gap, rx.recv()).await {
                pulses += 1;
            }
            e.on_coin(pulses * ppp, pulses);
        }
    });

    // Clock: expire sessions every second, save to disk every 30 s.
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
        let (e, db2) = (engine.clone(), db.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(if dev { 5 } else { 30 }));
            loop {
                tick.tick().await;
                let credits = db2.outbox_list(200).unwrap_or_default();
                match hub.sync(hub::proto::SyncReq { leases: e.lease_reports(), credits }).await {
                    Ok(r) => {
                        e.revoke_passes(&r.revoked);
                        let _ = db2.outbox_delete(&r.acked);
                    }
                    Err(err) => tracing::warn!("hub sync: {err}"),
                }
            }
        });
    }

    let admin_tls = if dev { None } else { Some(tls::load_or_create(&cfg.storage.data_dir, tls_names(&cfg))?) };
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

    let state = Arc::new(app::AppState {
        lan: cfg.lan_net()?,
        admin_macs: cfg.admin.allow_macs.iter().map(|m| net::Mac::parse(m)).collect::<Result<_>>()?,
        auth: auth::Auth::new(cfg.admin.session_hours),
        brand: brand::BrandStore::load(&db)?,
        hub: hub_client.clone(),
        engine: engine.clone(),
        db: db.clone(),
        fingerprint,
        cfg: cfg.clone(),
        dev,
    });

    let (stop_tx, stop_rx) = watch::channel(false);
    let portal_l = TcpListener::bind(cfg.portal.listen).await.with_context(|| format!("binding {}", cfg.portal.listen))?;
    let admin_l = TcpListener::bind(cfg.admin.listen).await.with_context(|| format!("binding {}", cfg.admin.listen))?;
    let scheme = if dev { "http" } else { "https" };
    tracing::info!("portal on http://{}  dashboard on {scheme}://{}", cfg.portal.listen, cfg.admin.listen);

    let p = tokio::spawn(server::serve(
        portal_l,
        portal::router(state.clone()),
        None,
        cfg.portal.max_connections,
        stop_rx.clone(),
    ));
    let a = tokio::spawn(server::serve(
        admin_l,
        admin::router(state.clone()),
        admin_tls.map(|t| t.acceptor),
        32,
        stop_rx,
    ));

    shutdown_signal().await;
    tracing::info!("shutting down; saving sessions");
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
