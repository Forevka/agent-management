//! `fleet`: one binary for every role in the agent fleet.
//!
//! - `fleet orchestrator`: the coordinator on the VPS (Postgres, Telegram, web UI, iroh hub).
//! - `fleet join <ticket>` / `fleet node`: a host that runs Claude Code agents.
//! - `fleet mcp`: the MCP server Claude starts inside each agent (not run by hand).
//! - `fleet project ...`, `fleet secret ...`, `fleet invite`: small helpers.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fleet_net::{
    Client, ClientConfig, EndpointAddr, InviteTicket, RelayChoice, SecretKey, Server, bind_endpoint,
};
use fleet_node::config::NodeConfig;
use fleet_orchestrator::{Config, MIGRATOR, Orchestrator, Vault, auth::DbAuth, tg::TelegramChat};
use fleet_proto::ALPN;
use iroh::protocol::Router;

#[derive(Parser)]
#[command(name = "fleet", version, about = "Claude Code agent fleet")]
struct Cli {
    /// Where this host keeps its identity key and state.
    #[arg(long, global = true, env = "FLEET_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Relay for NAT traversal: `none`, `n0` (public relays), or your relay's URL. Nodes default
    /// to the relay in their invite ticket.
    #[arg(long, global = true, env = "FLEET_RELAY")]
    relay: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the orchestrator (on the VPS). Configured through environment variables.
    Orchestrator {
        /// UDP address for the iroh endpoint.
        #[arg(long, default_value = "0.0.0.0:7700")]
        bind: SocketAddr,
        /// Public IP:port of this host, put in invite tickets.
        #[arg(long, env = "FLEET_PUBLIC_ADDR")]
        public_addr: Option<SocketAddr>,
        /// HTTP address for the web UI (put it behind a TLS reverse proxy).
        #[arg(long, default_value = "127.0.0.1:8080")]
        web_bind: SocketAddr,
    },
    /// Print a single-use invite ticket for a new node (run where the orchestrator runs).
    Invite {
        #[arg(long, env = "FLEET_PUBLIC_ADDR")]
        public_addr: Option<SocketAddr>,
    },
    /// Join the fleet with an invite ticket, then keep running as a node.
    Join {
        ticket: InviteTicket,
        #[arg(long)]
        name: Option<String>,
    },
    /// Run as a node that has already joined.
    Node {
        #[arg(long)]
        name: Option<String>,
    },
    /// Map server-side projects to directories on this host.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Read task secrets without printing whole files.
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Commands for agent sessions: milestones, questions, messages, feeds, memory.
    #[command(subcommand)]
    Agent(fleet_node::agent_cli::AgentCmd),
    /// MCP server for an agent session (started by Claude Code, speaks JSON-RPC on stdio).
    Mcp,
    /// Print this host's endpoint id.
    Id,
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Map a project slug (created in the web UI) to a local directory.
    Map {
        slug: String,
        dir: PathBuf,
    },
    Unmap {
        slug: String,
    },
    List,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Print one value from a secrets file (a name like `jira`, or a path).
    Get { file: String, key: String },
    /// Run a command with the variables of one or more secrets files in its environment.
    Exec {
        #[arg(long = "file", required = true)]
        files: Vec<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Logs go to stderr: for `fleet mcp`, stdout is the protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn,noq=warn,iroh_relay=warn,sqlx=warn".into()),
        )
        .init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    match cli.cmd {
        Cmd::Mcp => fleet_node::mcp::serve_stdio(fleet_node::mcp::ShimEnv::from_env()?).await,
        Cmd::Agent(cmd) => std::process::exit(fleet_node::agent_cli::run(cmd).await?),
        Cmd::Secret { cmd } => secret(cmd),
        Cmd::Project { cmd } => project(&data_dir, cmd),
        Cmd::Id => {
            println!("{}", load_or_create_key(&data_dir)?.public());
            Ok(())
        }
        Cmd::Invite { public_addr } => {
            let key = load_or_create_key(&data_dir)?;
            let addr = orchestrator_addr(&key, cli.relay.as_deref(), public_addr)?;
            let db = connect_db().await?;
            let ticket = InviteTicket::new(addr);
            sqlx::query("insert into invites (secret) values ($1)")
                .bind(&ticket.secret)
                .execute(&db)
                .await?;
            println!("fleet join {ticket}");
            Ok(())
        }
        Cmd::Orchestrator {
            bind,
            public_addr,
            web_bind,
        } => orchestrator(&data_dir, cli.relay.as_deref(), bind, public_addr, web_bind).await,
        Cmd::Join { ticket, name } => {
            std::fs::create_dir_all(&data_dir)?;
            write_atomic(
                &data_dir.join("orchestrator.addr"),
                &hex(&ticket.orchestrator)?,
            )?;
            node(
                &data_dir,
                cli.relay.as_deref(),
                ticket.orchestrator,
                name,
                Some(ticket.secret),
            )
            .await
        }
        Cmd::Node { name } => {
            let path = data_dir.join("orchestrator.addr");
            let text = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "{} not found; run `fleet join <ticket>` first",
                    path.display()
                )
            })?;
            node(&data_dir, cli.relay.as_deref(), unhex(&text)?, name, None).await
        }
    }
}

async fn connect_db() -> Result<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await
        .context("connecting to Postgres")
}

async fn orchestrator(
    data_dir: &Path,
    relay: Option<&str>,
    bind: SocketAddr,
    public_addr: Option<SocketAddr>,
    web_bind: SocketAddr,
) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let key = load_or_create_key(data_dir)?;
    let invite_addr = orchestrator_addr(&key, relay, public_addr)?;
    let cfg = Config::from_env()?;
    let vault = Vault::from_hex(
        &std::env::var("FLEET_MASTER_KEY").context("FLEET_MASTER_KEY is required")?,
    )?;
    let tg_token = std::env::var("TELEGRAM_ORCHESTRATOR_TOKEN")
        .context("TELEGRAM_ORCHESTRATOR_TOKEN is required")?;
    let chat_id: i64 = std::env::var("TELEGRAM_CHAT_ID")
        .context("TELEGRAM_CHAT_ID is required")?
        .parse()
        .context("TELEGRAM_CHAT_ID must be a number")?;

    let db = connect_db().await?;
    MIGRATOR
        .run(&db)
        .await
        .context("running database migrations")?;
    let auth = DbAuth::new(db.clone());

    let ep = bind_endpoint(
        key,
        parse_relay(relay.unwrap_or("n0"))?,
        Some(bind),
        vec![ALPN.to_vec()],
    )
    .await?;
    let (server, events) = Server::new(auth.clone());
    let router = Router::builder(ep.clone())
        .accept(ALPN, server.clone())
        .spawn();

    // A self-hosted Bot API server (or a test double) can stand in for api.telegram.org.
    let api_url = std::env::var("TELEGRAM_API_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let bot = match &api_url {
        Some(url) => fleet_telegram::Bot::with_base_url(tg_token, url.clone())?,
        None => fleet_telegram::Bot::new(tg_token)?,
    };
    let me = bot
        .get_me()
        .await
        .context("the orchestrator bot token is invalid")?;
    let chat = Arc::new(TelegramChat::new(bot.clone(), chat_id, api_url));
    let o = Orchestrator::new(db, vault, server, chat.clone(), cfg, auth);
    *o.invite_addr.write().unwrap() = Some(invite_addr);
    o.load_bots().await?;

    tokio::spawn(o.clone().run_node_events(events));
    tokio::spawn(o.clone().run_housekeeping());
    tokio::spawn(fleet_orchestrator::tg::run_updates(o.clone(), bot, chat_id));
    tokio::spawn(fleet_orchestrator::tg::run_agent_bot_callbacks(
        o.clone(),
        chat.clone(),
        chat_id,
    ));
    tokio::spawn(fleet_orchestrator::feeds::run_slack(o.clone()));

    tracing::info!(id = %ep.id(), %bind, %web_bind, bot = ?me.username, "orchestrator up");
    let listener = tokio::net::TcpListener::bind(web_bind).await?;
    let web = axum::serve(listener, fleet_orchestrator::web::router(o.clone()));
    tokio::select! {
        r = web => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    router.shutdown().await?;
    Ok(())
}

/// The address nodes dial: our key, the public IP:port, and our relay if it is a custom one.
fn orchestrator_addr(
    key: &SecretKey,
    relay: Option<&str>,
    public_addr: Option<SocketAddr>,
) -> Result<EndpointAddr> {
    let mut addr = EndpointAddr::new(key.public());
    if let Some(ip) = public_addr {
        addr = addr.with_ip_addr(ip);
    }
    if let RelayChoice::Custom(url) = parse_relay(relay.unwrap_or("n0"))? {
        addr = addr.with_relay_url(url);
    }
    if addr.is_empty() {
        bail!(
            "set FLEET_PUBLIC_ADDR (and/or FLEET_RELAY to your relay) so nodes can find the orchestrator"
        );
    }
    Ok(addr)
}

async fn node(
    data_dir: &Path,
    relay: Option<&str>,
    orchestrator: EndpointAddr,
    name: Option<String>,
    join_secret: Option<String>,
) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let key = load_or_create_key(data_dir)?;
    // Without an explicit relay, use the one the orchestrator advertises.
    let relay = match relay {
        Some(r) => parse_relay(r)?,
        None => orchestrator
            .addrs
            .iter()
            .find_map(|a| match a {
                iroh::TransportAddr::Relay(url) => Some(RelayChoice::Custom(url.clone())),
                _ => None,
            })
            .unwrap_or(RelayChoice::N0Public),
    };
    let ep = bind_endpoint(key, relay, None, vec![]).await?;
    let (client, events) = Client::new(orchestrator.clone());
    let cfg = ClientConfig {
        orchestrator,
        node_name: name.unwrap_or_else(default_node_name),
        app_version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        join_secret,
    };
    let exe = std::env::current_exe()?;
    let node = fleet_node::Node::new(data_dir.to_path_buf(), exe, client.clone())?;
    tracing::info!(id = %ep.id(), name = %cfg.node_name, "node starting");
    let link = tokio::spawn(async move { client.run(&ep, cfg).await });
    let agents = node.clone();
    tokio::select! {
        r = node.run(events) => r,
        r = link => {
            let r = r?;
            // The link only ends in an error when the orchestrator refuses this node (revoked).
            // Its agent sessions must not linger on, with their secrets, once it is out.
            if r.is_err() {
                agents.stop_all(std::time::Duration::from_secs(15)).await;
            }
            r
        }
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

fn project(data_dir: &Path, cmd: ProjectCmd) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let mut cfg = NodeConfig::load(data_dir)?;
    match cmd {
        ProjectCmd::Map { slug, dir } => {
            cfg.map_project(&slug, &dir)?;
            cfg.save(data_dir)?;
            println!(
                "mapped {slug} -> {}",
                cfg.project_dir(&slug).unwrap_or_default().display()
            );
        }
        ProjectCmd::Unmap { slug } => {
            if !cfg.unmap_project(&slug) {
                bail!("{slug} is not mapped");
            }
            cfg.save(data_dir)?;
        }
        ProjectCmd::List => {
            for p in &cfg.projects {
                println!("{}\t{}", p.project, p.path);
            }
        }
    }
    Ok(())
}

fn secret(cmd: SecretCmd) -> Result<()> {
    let resolve = |f: &str| -> PathBuf {
        let p = PathBuf::from(f);
        if p.components().count() > 1 || p.extension().is_some() {
            return p;
        }
        // A bare name refers to the current task's file.
        let dir = std::env::var_os("FLEET_SECRETS_DIR")
            .map(PathBuf::from)
            .unwrap_or_default();
        dir.join(format!("{f}.env"))
    };
    match cmd {
        SecretCmd::Get { file, key } => {
            println!("{}", fleet_node::secrets::get(&resolve(&file), &key)?);
            Ok(())
        }
        SecretCmd::Exec { files, cmd } => {
            let paths: Vec<PathBuf> = files.iter().map(|f| resolve(f)).collect();
            std::process::exit(fleet_node::secrets::exec(&paths, &cmd)?);
        }
    }
}

fn default_data_dir() -> PathBuf {
    // Per-user, because a node runs Claude with its user's login.
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("fleet"))
            .unwrap_or_else(|| PathBuf::from("fleet-data"))
    } else {
        std::env::var_os("HOME")
            .map(|p| PathBuf::from(p).join(".local/share/fleet"))
            .unwrap_or_else(|| PathBuf::from("fleet-data"))
    }
}

fn load_or_create_key(dir: &Path) -> Result<SecretKey> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("node.key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let bytes: [u8; 32] = data_encoding::HEXLOWER
            .decode(text.trim().as_bytes())
            .ok()
            .and_then(|b| b.try_into().ok())
            .with_context(|| format!("{} is not a 32-byte hex key", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    write_atomic(&path, &data_encoding::HEXLOWER.encode(&key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

fn parse_relay(s: &str) -> Result<RelayChoice> {
    Ok(match s {
        "none" => RelayChoice::Disabled,
        "n0" | "" => RelayChoice::N0Public,
        url => RelayChoice::Custom(
            url.parse()
                .with_context(|| format!("bad relay URL {url}"))?,
        ),
    })
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn hex<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(data_encoding::HEXLOWER.encode(&postcard::to_stdvec(v)?))
}

fn unhex<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(postcard::from_bytes(
        &data_encoding::HEXLOWER.decode(s.trim().as_bytes())?,
    )?)
}

fn default_node_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "node".into())
}
