//! `proxy.toml` loading and validation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Deserialize;

/// Tools that would let the agent change files or run commands. Rejected in `omp.tools`.
const FORBIDDEN_TOOLS: &[&str] = &["bash", "eval", "edit", "write", "ast_edit", "task", "debug"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub omp: OmpConfig,
    #[serde(rename = "profile")]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: String,
    pub api_key_file: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { listen: "0.0.0.0:8080".into(), api_key_file: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionsConfig {
    pub data_dir: PathBuf,
    #[serde(with = "humantime_serde")]
    pub idle_timeout: Duration,
    pub max_live: usize,
    #[serde(with = "humantime_serde")]
    pub turn_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/data"),
            idle_timeout: Duration::from_secs(15 * 60),
            max_live: 8,
            turn_timeout: Duration::from_secs(10 * 60),
            retention: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

impl SessionsConfig {
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
    pub fn mirrors_dir(&self) -> PathBuf {
        self.data_dir.join("mirrors")
    }
    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.json")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    /// `delta.reasoning_content`
    #[default]
    Field,
    /// `<think>…</think>` inside `delta.content`
    ThinkTags,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OmpConfig {
    pub binary: PathBuf,
    pub tools: Vec<String>,
    pub extra_args: Vec<String>,
    pub env_passthrough: Vec<String>,
    pub reasoning: ReasoningMode,
}

impl Default for OmpConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("omp"),
            tools: ["read", "grep", "glob", "todo"].map(String::from).to_vec(),
            extra_args: Vec::new(),
            env_passthrough: Vec::new(),
            reasoning: ReasoningMode::Field,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub model: String,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitConfig {
    pub token_file: Option<PathBuf>,
    /// Basic-auth user name sent with the token (GitHub: `x-access-token`, GitLab: `oauth2`).
    pub token_username: String,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self { token_file: None, token_username: "x-access-token".into(), repos: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub description: String,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text).context("invalid proxy config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading config {}", path.display()))?;
        Self::from_toml(&text)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.git.repos.iter().find(|r| r.name == name)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.profiles.is_empty() {
            bail!("at least one [[profile]] is required");
        }
        let mut names = HashSet::new();
        for p in &self.profiles {
            if !names.insert(&p.name) {
                bail!("duplicate profile name {:?}", p.name);
            }
        }
        for tool in &self.omp.tools {
            if FORBIDDEN_TOOLS.contains(&tool.as_str()) {
                bail!("tool {tool:?} is not allowed: the agent must stay read-only");
            }
        }
        let mut repos = HashSet::new();
        for r in &self.git.repos {
            if !is_valid_repo_name(&r.name) {
                bail!("invalid repo name {:?} (use a-z, 0-9, '.', '_', '-')", r.name);
            }
            if !repos.insert(&r.name) {
                bail!("duplicate repo name {:?}", r.name);
            }
        }
        Ok(())
    }
}

fn is_valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[profile]]
        name = "omp-codex"
        model = "openai-codex/gpt-5.5"
    "#;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:8080");
        assert_eq!(c.sessions.idle_timeout, Duration::from_secs(900));
        assert_eq!(c.sessions.max_live, 8);
        assert_eq!(c.omp.tools, ["read", "grep", "glob", "todo"]);
        assert_eq!(c.omp.reasoning, ReasoningMode::Field);
        assert_eq!(c.git.token_username, "x-access-token");
        assert_eq!(c.profile("omp-codex").unwrap().model, "openai-codex/gpt-5.5");
        assert!(c.profile("nope").is_none());
    }

    #[test]
    fn full_config_parses() {
        let c = Config::from_toml(
            r#"
            [server]
            listen = "127.0.0.1:9000"
            api_key_file = "/run/secrets/proxy_api_key"

            [sessions]
            data_dir = "/tmp/x"
            idle_timeout = "0s"
            max_live = 2
            turn_timeout = "30s"
            retention = "7d"

            [omp]
            binary = "/usr/local/bin/omp"
            tools = ["read", "grep"]
            extra_args = ["--no-title"]
            env_passthrough = ["OPENAI_API_KEY"]
            reasoning = "think_tags"

            [[profile]]
            name = "a"
            model = "m"
            thinking = "high"

            [git]
            token_file = "/run/secrets/git_token"
            [[git.repo]]
            name = "backend"
            url = "https://github.com/acme/backend.git"
            description = "Backend"
            "#,
        )
        .unwrap();
        assert_eq!(c.sessions.idle_timeout, Duration::ZERO);
        assert_eq!(c.sessions.retention, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(c.omp.reasoning, ReasoningMode::ThinkTags);
        assert_eq!(c.repo("backend").unwrap().description, "Backend");
        assert_eq!(c.sessions.mirrors_dir(), PathBuf::from("/tmp/x/mirrors"));
    }

    #[test]
    fn rejects_write_tools() {
        let err = Config::from_toml(&format!("[omp]\ntools = [\"read\", \"bash\"]\n{MINIMAL}")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    #[test]
    fn rejects_missing_profiles_and_bad_repo_names() {
        assert!(Config::from_toml("").is_err());
        let bad = format!("{MINIMAL}\n[[git.repo]]\nname = \"../etc\"\nurl = \"https://x\"\n");
        assert!(Config::from_toml(&bad).is_err());
    }
}
