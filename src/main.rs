use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(version, about = "Nextcloud-compatible server")]
struct Cli {
    /// Optional TOML config file; `RUSTBUCKET_*` environment variables override it.
    #[arg(long, env = "RUSTBUCKET_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server.
    Serve,
    /// Apply pending database migrations.
    Migrate,
    /// Reconcile the file index with the disk (like `occ files:scan`); also imports new files.
    Scan {
        /// only this user (default: all)
        uid: Option<String>,
        /// show what would change, change nothing
        #[arg(long)]
        dry_run: bool,
        /// allow removing (almost) all index entries
        #[arg(long)]
        force: bool,
    },
    /// Write a consistent snapshot of the database to a new file (safe while running).
    Backup { dest: PathBuf },
    /// Probe /health on the local listener (for container HEALTHCHECK).
    Healthcheck,
    /// Print version and build number (what Admin > About shows).
    Version,
    #[command(subcommand)]
    User(UserCmd),
    #[command(subcommand)]
    Group(GroupCmd),
    /// Trash bin administration.
    #[command(subcommand)]
    Trash(TrashCmd),
    /// Apply the trash and version retention settings now (the server also does this hourly).
    Expire,
}

#[derive(Subcommand)]
enum TrashCmd {
    /// List a user's trash.
    List { uid: String },
    /// Remove items from a user's trash for good: everything, or only the entries with this original name.
    Empty {
        uid: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Create a user; the password is read from the first line of stdin.
    Create {
        uid: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        admin: bool,
    },
    /// Change a user. Give only what should change; `--password-stdin` reads the new password
    /// from the first line of stdin.
    Modify {
        uid: String,
        #[arg(long)]
        display_name: Option<String>,
        /// empty string clears it
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        password_stdin: bool,
        #[arg(long, conflicts_with = "no_admin")]
        admin: bool,
        #[arg(long)]
        no_admin: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// List users.
    List,
}

#[derive(Subcommand)]
enum GroupCmd {
    Create { gid: String },
    AddMember { gid: String, uid: String },
}

fn init_logging(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,sqlx=warn".into());
    let b = tracing_subscriber::fmt().with_env_filter(filter);
    if json { b.json().init() } else { b.init() }
}

async fn healthcheck(cfg: &Config) -> anyhow::Result<()> {
    let port = cfg.listen.port();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).await?;
    anyhow::ensure!(
        buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200"),
        "unhealthy: {}",
        buf.lines().next().unwrap_or("")
    );
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("sigterm handler");
    tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref(), |k| std::env::var(k).ok())?;
    init_logging(cfg.log_json);

    match cli.cmd {
        Cmd::Version => {
            let b = rustbucket::buildinfo::get();
            println!("rustbucket {}", b.version);
            println!("build date: {} ({})", b.build_date_display(), b.build_date);
            println!("commit:     {}", b.commit);
            println!("build id:   {}", b.hash);
            Ok(())
        }
        Cmd::Healthcheck => healthcheck(&cfg).await,
        Cmd::Scan {
            uid,
            dry_run,
            force,
        } => {
            let pool = db::connect(&cfg.database_url).await?;
            let store = rustbucket::storage::Store::new(pool.clone(), cfg.data_dir.clone());
            let users: Vec<(i64, String)> = match &uid {
                Some(u) => {
                    sqlx::query_as("SELECT id, uid FROM users WHERE uid = $1")
                        .bind(u)
                        .fetch_all(&pool)
                        .await?
                }
                None => {
                    sqlx::query_as("SELECT id, uid FROM users ORDER BY uid")
                        .fetch_all(&pool)
                        .await?
                }
            };
            anyhow::ensure!(!users.is_empty(), "no such user");
            for (id, uid) in users {
                let r = store
                    .scan(
                        &rustbucket::storage::Owner {
                            id,
                            uid: uid.clone(),
                        },
                        dry_run,
                        force,
                    )
                    .await?;
                println!(
                    "{uid}: added {} updated {} removed {}{}",
                    r.added,
                    r.updated,
                    r.removed,
                    if dry_run { " (dry run)" } else { "" }
                );
            }
            Ok(())
        }
        Cmd::Backup { dest } => {
            let pool = db::connect(&cfg.database_url).await?;
            db::backup(&pool, &dest).await?;
            println!("database snapshot written to {}", dest.display());
            Ok(())
        }
        Cmd::Migrate => {
            let pool = db::connect(&cfg.database_url).await?;
            db::migrate(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::Trash(cmd) => {
            let pool = db::connect(&cfg.database_url).await?;
            let store = rustbucket::storage::Store::new(pool.clone(), cfg.data_dir.clone());
            let uid = match &cmd {
                TrashCmd::List { uid } | TrashCmd::Empty { uid, .. } => uid.clone(),
            };
            let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = $1")
                .bind(&uid)
                .fetch_optional(&pool)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no such user: {uid}"))?;
            let o = rustbucket::storage::Owner { id, uid };
            match cmd {
                TrashCmd::List { .. } => {
                    for i in store.trash_list(&o).await? {
                        println!(
                            "{}\t{}\t{} bytes\tfrom /{}",
                            i.deleted_at,
                            i.entry_name(),
                            i.size,
                            i.orig_dir
                        );
                    }
                }
                TrashCmd::Empty { .. } => {
                    println!("removed {} item(s)", store.trash_empty(&o).await?)
                }
            }
            Ok(())
        }
        Cmd::Expire => {
            let pool = db::connect(&cfg.database_url).await?;
            let store = rustbucket::storage::Store::new(pool, cfg.data_dir.clone());
            let (t, v) = store
                .expire(cfg.trash_retention_days, cfg.version_retention_days)
                .await?;
            println!("expired {t} trash item(s) and {v} old version(s)");
            Ok(())
        }
        Cmd::User(UserCmd::Modify {
            uid,
            display_name,
            email,
            password_stdin,
            admin,
            no_admin,
            enable,
            disable,
        }) => {
            let password = if password_stdin {
                let mut pw = String::new();
                std::io::stdin().lock().read_line(&mut pw)?;
                Some(pw.trim_end_matches(['\r', '\n']).to_string())
            } else {
                None
            };
            let pool = db::connect(&cfg.database_url).await?;
            db::update_user(
                &pool,
                &uid,
                &db::UserEdit {
                    display_name,
                    email,
                    password,
                    is_admin: if admin {
                        Some(true)
                    } else if no_admin {
                        Some(false)
                    } else {
                        None
                    },
                    enabled: if enable {
                        Some(true)
                    } else if disable {
                        Some(false)
                    } else {
                        None
                    },
                    keep_session: None,
                },
            )
            .await?;
            println!("user {uid} updated");
            Ok(())
        }
        Cmd::User(UserCmd::List) => {
            let pool = db::connect(&cfg.database_url).await?;
            for u in db::list_users(&pool).await? {
                println!(
                    "{}\t{}\t{}\t{}{}",
                    u.uid,
                    u.display_name,
                    if u.email.is_empty() { "-" } else { &u.email },
                    if u.is_admin { "admin" } else { "user" },
                    if u.enabled { "" } else { "\tDISABLED" }
                );
            }
            Ok(())
        }
        Cmd::User(UserCmd::Create {
            uid,
            display_name,
            admin,
        }) => {
            let mut pw = String::new();
            std::io::stdin().lock().read_line(&mut pw)?;
            let pw = pw.trim_end_matches(['\r', '\n']);
            let pool = db::connect(&cfg.database_url).await?;
            db::create_user(
                &pool,
                &uid,
                display_name.as_deref().unwrap_or(&uid),
                pw,
                admin,
            )
            .await?;
            println!("user {uid} created");
            Ok(())
        }
        Cmd::Group(GroupCmd::Create { gid }) => {
            let pool = db::connect(&cfg.database_url).await?;
            db::create_group(&pool, &gid).await?;
            println!("group {gid} created");
            Ok(())
        }
        Cmd::Group(GroupCmd::AddMember { gid, uid }) => {
            let pool = db::connect(&cfg.database_url).await?;
            db::add_member(&pool, &gid, &uid).await?;
            println!("{uid} added to {gid}");
            Ok(())
        }
        Cmd::Serve => {
            let pool = db::connect(&cfg.database_url)
                .await
                .context("database connection")?;
            if cfg.auto_migrate {
                db::migrate(&pool).await.context("migrations")?;
            }
            // refuses to start if the data dir looks wrong/unmounted (marker missing but index non-empty)
            rustbucket::storage::Store::new(pool.clone(), cfg.data_dir.clone())
                .check_data_dir()
                .await
                .context("data_dir")?;
            let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
            tracing::info!(listen = %cfg.listen, base_url = ?cfg.base_url, "serving");
            let state = AppState::new(pool, Arc::new(cfg));
            // abandoned chunk uploads and temp files older than a day: at startup and then hourly
            {
                let (pool, store) = (state.pool.clone(), state.store.clone());
                let (trash_days, version_days) = (
                    state.config.trash_retention_days,
                    state.config.version_retention_days,
                );
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
                    loop {
                        tick.tick().await;
                        match store.expire(trash_days, version_days).await {
                            Ok((0, 0)) => {}
                            Ok((t, v)) => tracing::info!(
                                trash = t,
                                versions = v,
                                "expired trash and versions"
                            ),
                            Err(e) => tracing::warn!(error = %e, "expiry failed"),
                        }
                        let pruned = rustbucket::preview::prune_cache(
                            store.appdata_dir(),
                            std::time::Duration::from_secs(30 * 24 * 3600),
                        )
                        .await;
                        if pruned > 0 {
                            tracing::info!(removed = pruned, "pruned old previews");
                        }
                        match rustbucket::uploads::cleanup_stale(
                            &pool,
                            &store,
                            std::time::Duration::from_secs(24 * 3600),
                        )
                        .await
                        {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(removed = n, "removed stale uploads"),
                            Err(e) => tracing::warn!(error = %e, "upload cleanup failed"),
                        }
                    }
                });
            }
            let app = router(state);
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown())
            .await?;
            Ok(())
        }
    }
}
