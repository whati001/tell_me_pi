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
const MAX_REFS: usize = 200;

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

    async fn refs(
        &self,
        name: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let mirror = self.sync_mirror(name, progress, cancel).await?;
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
        let mut lines: Vec<String> =
            tags.lines().map(|t| format!("tag {t}")).chain(branches.lines().map(|b| format!("branch {b}"))).collect();
        if lines.len() > MAX_REFS {
            let more = lines.len() - MAX_REFS;
            lines.truncate(MAX_REFS);
            lines.push(format!("… {more} more"));
        }
        Ok(lines.join("\n"))
    }

    async fn checkout(
        &self,
        workdir: &Path,
        name: &str,
        git_ref: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        validate_ref(git_ref)?;
        let dir_name = format!("{name}@{}", git_ref.replace('/', "_"));
        let target = workdir.join(&dir_name);
        if target.join(".git").exists() {
            return Ok(format!("{name} {git_ref} is already checked out in ./{dir_name}/"));
        }
        let mirror = self.sync_mirror(name, progress, cancel).await?;
        let lock = self.lock_for(name).await;
        let _guard = lock.lock().await;
        let commit = self
            .git(Some(&mirror), &["rev-parse", "--verify", "--quiet", &format!("{git_ref}^{{commit}}")], cancel)
            .await
            .map_err(|_| anyhow::anyhow!("unknown ref {git_ref:?} in {name}; use action=refs to list refs"))?;
        let commit = commit.trim();
        progress(&format!("checking out {name} {git_ref}"));
        let target_str = target.to_str().context("working directory is not valid UTF-8")?;
        self.git(Some(&mirror), &["worktree", "add", "--detach", target_str, commit], cancel).await?;
        Ok(format!(
            "Checked out {name} {git_ref} ({}) into ./{dir_name}/. Use read, grep and glob on paths under ./{dir_name}/.",
            &commit[..commit.len().min(12)]
        ))
    }

    async fn lock_for(&self, name: &str) -> Arc<Mutex<Option<Instant>>> {
        self.locks.lock().await.entry(name.to_string()).or_default().clone()
    }

    /// Clones or fetches the shared mirror. Fetches at most once per `FETCH_INTERVAL`.
    async fn sync_mirror(
        &self,
        name: &str,
        progress: &(dyn Fn(&str) + Send + Sync),
        cancel: &CancellationToken,
    ) -> anyhow::Result<PathBuf> {
        let Some(repo) = self.repos.iter().find(|r| r.name == name) else {
            bail!("unknown repository {name:?}; use action=list");
        };
        let mirror = self.mirrors_dir.join(format!("{name}.git"));
        let lock = self.lock_for(name).await;
        let mut last_fetch = lock.lock().await;
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
            self.git(Some(&mirror), &["fetch", "--prune", "--tags", "origin"], cancel).await?;
            self.git(Some(&mirror), &["worktree", "prune"], cancel).await?;
            *last_fetch = Some(Instant::now());
        }
        Ok(mirror)
    }

    async fn git(&self, git_dir: Option<&Path>, args: &[&str], cancel: &CancellationToken) -> anyhow::Result<String> {
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
        if let Some(header) = &self.auth_header {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.extraHeader")
                .env("GIT_CONFIG_VALUE_0", header);
        }
        let child = cmd.spawn().context("spawning git")?;
        let output = tokio::select! {
            out = child.wait_with_output() => out.context("running git")?,
            _ = cancel.cancelled() => bail!("cancelled"),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args[0], stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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
    fn validates_refs() {
        for good in ["v1.4.2", "main", "release/1.4", "a1b2c3d", "feature_x-y"] {
            validate_ref(good).unwrap();
        }
        for bad in ["", "-x", "--upload-pack=evil", "a..b", "/etc", "x/", "x.lock", "a b", "a;b", ".hidden", "x//y"] {
            assert!(validate_ref(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
