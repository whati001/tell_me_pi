use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use tell_me_where::{
    api::{self, AppState},
    config::Config,
    repo::RepoTool,
    session::SessionManager,
};

const DEFAULT_CONFIG: &str = "/etc/omp-proxy/proxy.toml";
const DEFAULT_USER: &str = "omp";
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config_path = std::env::var_os("OMP_PROXY_CONFIG").map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from);
    let cfg = Arc::new(Config::load(&config_path)?);
    // Secrets are read while still privileged; afterwards the files are unreadable.
    let api_key = read_secret(cfg.server.api_key_file.as_deref())?;
    let git_token = read_secret(cfg.git.token_file.as_deref())?;
    if api_key.is_none() {
        tracing::warn!("no server.api_key_file configured: the API is unauthenticated");
    }
    drop_privileges(&std::env::var("OMP_PROXY_USER").unwrap_or_else(|_| DEFAULT_USER.into()))?;

    tokio::runtime::Runtime::new()?.block_on(serve(cfg, api_key, git_token))
}

async fn serve(cfg: Arc<Config>, api_key: Option<String>, git_token: Option<String>) -> anyhow::Result<()> {
    let token = git_token.as_deref().map(|t| (cfg.git.token_username.as_str(), t));
    let repo = RepoTool::new(cfg.git.repos.clone(), cfg.sessions.mirrors_dir(), token);
    if token.is_some() {
        check_git_env_is_private(&repo, cfg.git.insecure_allow_exposed_token).await?;
    }
    let repo = Arc::new(repo);
    let sessions = SessionManager::new(cfg.clone(), repo)?;

    let maintenance = sessions.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MAINTENANCE_INTERVAL);
        loop {
            tick.tick().await;
            maintenance.maintain().await;
        }
    });

    let app = api::router(Arc::new(AppState { cfg: cfg.clone(), sessions: sessions.clone(), api_key }));
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.listen))?;
    tracing::info!("listening on {}", cfg.server.listen);
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    sessions.shutdown_all().await;
    Ok(())
}

/// The agent runs as our user; if it can read a git process's environment, it can read the token.
async fn check_git_env_is_private(repo: &RepoTool, allow_exposed: bool) -> anyhow::Result<()> {
    if repo.git_env_is_private().await.context("checking whether git processes expose the token")? {
        return Ok(());
    }
    const PROBLEM: &str = "git child processes are readable by the agent (their /proc environ is accessible), so the \
                           git token could leak.";
    if allow_exposed {
        tracing::warn!("{PROBLEM} Continuing because git.insecure_allow_exposed_token is set.");
        return Ok(());
    }
    anyhow::bail!(
        "{PROBLEM} Make the git binaries execute-only (see Dockerfile) or set git.insecure_allow_exposed_token = true \
         for local development."
    )
}

fn read_secret(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let value = std::fs::read_to_string(path).with_context(|| format!("reading secret {}", path.display()))?;
    let value = value.trim().to_string();
    anyhow::ensure!(!value.is_empty(), "secret {} is empty", path.display());
    Ok(Some(value))
}

/// Switches from root to `user` (when started as root) and marks the process non-dumpable,
/// so omp children running as the same user cannot read our memory or `/proc/<pid>/environ`.
fn drop_privileges(user: &str) -> anyhow::Result<()> {
    use nix::unistd::{Uid, User, setgid, setgroups, setuid};
    if Uid::effective().is_root() {
        let u = User::from_name(user)?.with_context(|| format!("user {user:?} does not exist"))?;
        setgroups(&[u.gid]).context("setgroups")?;
        setgid(u.gid).context("setgid")?;
        setuid(u.uid).context("setuid")?;
        anyhow::ensure!(setuid(Uid::from_raw(0)).is_err(), "privilege drop failed");
        tracing::info!("running as user {user}");
    }
    nix::sys::prctl::set_dumpable(false).context("prctl(PR_SET_DUMPABLE)")?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
