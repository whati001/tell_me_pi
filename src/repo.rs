//! The `repo` host tool: read-only access to configured git repositories.
//!
//! Shared blobless mirrors live in `<data>/mirrors/<name>.git`; checkouts are detached
//! worktrees inside the chat's working directory. There is deliberately no push action.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::config::RepoConfig;

pub const TOOL_NAME: &str = "repo";
const FETCH_INTERVAL: Duration = Duration::from_secs(60);
const MAX_TAGS: usize = 150;
const MAX_BRANCHES: usize = 50;
/// Proxy environment variables handed to git when set (network access in restricted setups).
const GIT_ENV_PASSTHROUGH: &[&str] = &[
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RepoArgs {
    List,
    Refs {
        repo: String,
    },
    Checkout {
        repo: String,
        #[serde(rename = "ref")]
        git_ref: String,
    },
}

pub struct RepoTool {
    repos: Vec<RepoConfig>,
    mirrors_dir: PathBuf,
    auth_header: Option<String>,
    git: PathBuf,
    locks: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
}

impl RepoTool {
    pub fn new(repos: Vec<RepoConfig>, mirrors_dir: PathBuf, token: Option<(&str, &str)>) -> Self {
        let auth_header =
            token.map(|(user, token)| format!("Authorization: Basic {}", STANDARD.encode(format!("{user}:{token}"))));
        Self { repos, mirrors_dir, auth_header, git: PathBuf::from("git"), locks: Mutex::default() }
    }

    /// Uses `git` instead of the `git` found on `PATH`.
    pub fn with_git_binary(mut self, git: PathBuf) -> Self {
        self.git = git;
        self
    }

    /// Whether the environment of git child processes (which holds the token) is hidden from other processes of
    /// the same user, i.e. whether `/proc/<pid>/environ` of a running git is unreadable. That is the case when
    /// the git binary is execute-only: the kernel then marks the process non-dumpable.
    pub async fn git_env_is_private(&self) -> anyhow::Result<bool> {
        // `hash-object --stdin` waits for stdin even outside a repository, so the probe reads the environment of a
        // live git rather than of a zombie (whose environ may be readable regardless). The cwd is set explicitly so
        // the result does not depend on where the proxy runs.
        let mut child = Command::new(&self.git)
            .args(["hash-object", "--stdin"])
            .current_dir(std::env::temp_dir())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OMP_PROXY_PROBE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", self.git.display()))?;
        // `spawn` returns after the exec succeeded, so this reads the environment of git itself.
        let read = match child.id() {
            Some(pid) => tokio::fs::read(format!("/proc/{pid}/environ")).await,
            None => Err(std::io::Error::other("no pid")),
        };
        // Only trust the result if git was still running while it was read.
        let result = match child.try_wait() {
            Ok(None) => match read {
                Ok(_) => Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(true),
                Err(e) => Err(anyhow::Error::new(e).context("reading the environment of a git process")),
            },
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = pipe.read_to_string(&mut stderr).await;
                }
                Err(anyhow::anyhow!("git probe exited early ({status}): {}", stderr.trim()))
            }
            Err(e) => Err(anyhow::Error::new(e).context("checking the git probe")),
        };
        drop(child.stdin.take());
        drop(child.stderr.take());
        if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        result
    }

    /// The `set_host_tools` definition.
    pub fn definition(&self) -> Value {
        let names: Vec<&str> = self.repos.iter().map(|r| r.name.as_str()).collect();
        json!({
            "name": TOOL_NAME,
            "label": "Repository",
            "description": "Read-only access to the configured git repositories. \
                action=list: show available repositories. \
                action=refs: list tags (newest first) and branches of a repository. \
                action=checkout: check out a tag, branch or commit into ./<repo>@<ref> in the \
                working directory; then use read/grep/glob on that directory. \
                Only check out code when the question needs it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "refs", "checkout"] },
                    "repo": { "type": "string", "enum": names },
                    "ref": { "type": "string", "description": "Tag, branch or commit for checkout" }
                },
                "required": ["action"],
                "additionalProperties": false
            },
            "loadMode": "essential"
        })
    }

    /// Runs one tool call. `Ok` text goes to the model; `Err` is reported as a tool error.
    pub async fn execute(
        &self,
        workdir: &Path,
        args: Value,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let args: RepoArgs = serde_json::from_value(args).context("invalid arguments")?;
        match args {
            RepoArgs::List => Ok(self.list()),
            RepoArgs::Refs { repo } => self.refs(&repo, progress, cancel).await,
            RepoArgs::Checkout { repo, git_ref } => self.checkout(workdir, &repo, &git_ref, progress, cancel).await,
        }
    }

    fn list(&self) -> String {
        if self.repos.is_empty() {
            return "No repositories are configured.".into();
        }
        self.repos.iter().map(|r| format!("- {}: {}", r.name, r.description)).collect::<Vec<_>>().join("\n")
    }

    fn repo(&self, name: &str) -> anyhow::Result<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| r.name == name)
            .with_context(|| format!("unknown repository {name:?}; use action=list"))
    }

    async fn refs(
        &self,
        name: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let repo = self.repo(name)?;
        let (mirror, warning) = self.sync_mirror(repo, progress, cancel).await?;
        let tags = self
            .git(
                Some(&mirror),
                &["for-each-ref", "--sort=-version:refname", "--format=%(refname:short)", "refs/tags"],
                cancel,
            )
            .await?;
        let branches = self
            .git(
                Some(&mirror),
                &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads"],
                cancel,
            )
            .await?;
        let mut lines = capped(&tags, "tag", "tags", MAX_TAGS);
        lines.extend(capped(&branches, "branch", "branches", MAX_BRANCHES));
        Ok(format!("{}{}", warning.unwrap_or_default(), lines.join("\n")))
    }

    async fn checkout(
        &self,
        workdir: &Path,
        name: &str,
        git_ref: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        // `name` becomes part of a path: only configured names get that far.
        let repo = self.repo(name)?;
        validate_ref(git_ref)?;
        let dir_name = format!("{name}@{}", encode_ref(git_ref));
        let target = workdir.join(&dir_name);
        let marker = workdir.join(format!(".{dir_name}.done"));
        let (mirror, warning) = self.sync_mirror(repo, progress, cancel).await?;
        let warning = warning.unwrap_or_default();

        let lock = self.lock_for(name).await;
        let _guard = lock.lock().await;
        if target.is_dir()
            && let Ok(done) = tokio::fs::read_to_string(&marker).await
        {
            return Ok(format!(
                "{warning}{name} {git_ref} is already checked out in ./{dir_name}/ (commit {}; the directory reflects \
                 that commit even if the ref has moved since).",
                short(done.trim())
            ));
        }
        let spec = format!("{git_ref}^{{commit}}");
        let output = self.git_output(Some(&mirror), &["rev-parse", "--verify", "--quiet", &spec], cancel).await?;
        if !output.status.success() {
            if output.status.code() == Some(1) && output.stderr.trim_ascii().is_empty() {
                bail!("unknown ref {git_ref:?} in {name}; use action=refs to list refs");
            }
            bail!("git rev-parse failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

        progress(&format!("checking out {name} {git_ref}"));
        // Leftovers of an interrupted checkout: the marker is only written once the worktree is complete.
        remove_if_exists(tokio::fs::remove_file(&marker).await)?;
        if tokio::fs::symlink_metadata(&target).await.is_ok() {
            tokio::fs::remove_dir_all(&target).await.with_context(|| format!("removing incomplete ./{dir_name}/"))?;
        }
        let target_str = target.to_str().context("working directory is not valid UTF-8")?;
        self.git(Some(&mirror), &["worktree", "prune"], cancel).await?;
        // `-f -f`: a killed `worktree add` leaves its entry locked ("initializing"), which `prune` skips and a
        // plain `add` refuses forever ("missing but locked worktree"). Forcing is safe: the target was just removed.
        self.git(Some(&mirror), &["worktree", "add", "--detach", "-f", "-f", target_str, &commit], cancel).await?;
        tokio::fs::write(&marker, &commit).await.context("writing checkout marker")?;
        Ok(format!(
            "{warning}Checked out {name} {git_ref} ({}) into ./{dir_name}/. Use read, grep and glob on paths under \
             ./{dir_name}/.",
            short(&commit)
        ))
    }

    async fn lock_for(&self, name: &str) -> Arc<Mutex<Option<Instant>>> {
        self.locks.lock().await.entry(name.to_string()).or_default().clone()
    }

    /// Clones or fetches the shared mirror. Fetches at most once per `FETCH_INTERVAL`. A failed fetch of an
    /// existing mirror is not fatal: the second value is then a warning to put in front of the tool result.
    async fn sync_mirror(
        &self,
        repo: &RepoConfig,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<(PathBuf, Option<String>)> {
        let name = &repo.name;
        let mirror = self.mirrors_dir.join(format!("{name}.git"));
        let lock = self.lock_for(name).await;
        let mut last_fetch = lock.lock().await;
        let mut warning = None;
        if !mirror.join("HEAD").exists() {
            progress(&format!("cloning {name} (first use, this can take a while)"));
            tokio::fs::create_dir_all(&self.mirrors_dir).await?;
            let tmp = self.mirrors_dir.join(format!(".{name}.git.tmp"));
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            let tmp_str = tmp.to_str().context("mirror path is not valid UTF-8")?;
            self.git(None, &["clone", "--mirror", "--filter=blob:none", "--", &repo.url, tmp_str], cancel).await?;
            tokio::fs::rename(&tmp, &mirror).await?;
            *last_fetch = Some(Instant::now());
        } else if last_fetch.is_none_or(|t| t.elapsed() > FETCH_INTERVAL) {
            progress(&format!("fetching {name}"));
            match self.git(Some(&mirror), &["fetch", "--prune", "--tags", "origin"], cancel).await {
                Ok(_) => *last_fetch = Some(Instant::now()),
                Err(e) if cancel.is_cancelled() => return Err(e),
                Err(e) => {
                    let short_error: String =
                        format!("{e:#}").lines().next().unwrap_or_default().chars().take(200).collect();
                    tracing::warn!(repo = %name, "fetch failed: {e:#}");
                    progress(&format!("fetch failed, using cached mirror: {short_error}"));
                    warning =
                        Some(format!("Warning: could not update {name} ({short_error}); results may be stale.\n"));
                }
            }
        }
        Ok((mirror, warning))
    }

    async fn git(&self, git_dir: Option<&Path>, args: &[&str], cancel: &CancellationToken) -> anyhow::Result<String> {
        let output = self.git_output(git_dir, args, cancel).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args[0], stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs git and returns its output regardless of the exit status.
    async fn git_output(
        &self,
        git_dir: Option<&Path>,
        args: &[&str],
        cancel: &CancellationToken,
    ) -> anyhow::Result<std::process::Output> {
        let mut cmd = Command::new(&self.git);
        if let Some(dir) = git_dir {
            cmd.arg("--git-dir").arg(dir);
        }
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.mirrors_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in GIT_ENV_PASSTHROUGH {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        if let Some(header) = &self.auth_header {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        let child = cmd.spawn().context("spawning git")?;
        tokio::select! {
            out = child.wait_with_output() => out.context("running git"),
            _ = cancel.cancelled() => bail!("cancelled"),
        }
    }
}

/// `prefix line` for each line of `text`, at most `max` of them plus a "… N more" line.
fn capped(text: &str, prefix: &str, plural: &str, max: usize) -> Vec<String> {
    let all: Vec<&str> = text.lines().collect();
    let mut lines: Vec<String> = all.iter().take(max).map(|l| format!("{prefix} {l}")).collect();
    if all.len() > max {
        lines.push(format!("… {} more {plural}", all.len() - max));
    }
    lines
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

fn remove_if_exists(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Reversible directory-name encoding of a (validated) ref: `_` → `_5f`, `/` → `_2f`.
fn encode_ref(r: &str) -> String {
    r.replace('_', "_5f").replace('/', "_2f")
}

/// Accepts tag, branch and commit names; rejects option injection and path tricks.
pub fn validate_ref(r: &str) -> anyhow::Result<()> {
    let ok = !r.is_empty()
        && r.len() <= 200
        && !r.starts_with(['-', '/', '.'])
        && !r.ends_with(['/', '.'])
        && !r.ends_with(".lock")
        && !r.contains("..")
        && !r.contains("//")
        && r.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
    if !ok {
        bail!("invalid ref {r:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_refs_reversibly() {
        assert_eq!(encode_ref("release/1.x"), "release_2f1.x");
        assert_eq!(encode_ref("release_1.x"), "release_5f1.x");
        assert_eq!(encode_ref("a_2f"), "a_5f2f");
    }

    #[test]
    fn caps_refs() {
        let text = "a\nb\nc\n";
        assert_eq!(capped(text, "tag", "tags", 2), ["tag a", "tag b", "… 1 more tags"]);
        assert_eq!(capped(text, "branch", "branches", 2), ["branch a", "branch b", "… 1 more branches"]);
        assert_eq!(capped(text, "tag", "tags", 3).len(), 3);
    }

    #[test]
    fn validates_refs() {
        for good in ["v1.4.2", "main", "release/1.4", "a1b2c3d", "feature_x-y"] {
            validate_ref(good).unwrap();
        }
        for bad in ["", "-x", "--upload-pack=evil", "a..b", "/etc", "x/", "x.lock", "a b", "a;b", ".hidden", "x//y"] {
            assert!(validate_ref(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
