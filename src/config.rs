//! `proxy.toml` loading and validation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Deserialize;

/// The only omp tools the agent may use: none of them can change files or run commands. `omp.tools` entries
/// are normalized with [`normalize_tool`] and must be in this list.
pub const ALLOWED_TOOLS: &[&str] = &["read", "grep", "glob", "todo"];

/// omp flags the proxy sets itself (or that would widen what the agent can do). Rejected in `omp.extra_args`,
/// both as `<flag>` and as `<flag>=<value>`.
const RESERVED_OMP_FLAGS: &[&str] = &[
    "--tools",
    "--no-tools",
    "--mode",
    "--cwd",
    "--session-dir",
    "--resume",
    "-r",
    "--session",
    "--continue",
    "-c",
    "--fork",
    "--extension",
    "-e",
    "--hook",
    "--trusted-extension",
    "--plugin-dir",
    "--approval-mode",
    "--auto-approve",
    "--yolo",
    "--add-dir",
    "--allow-home",
    "--config",
];

/// Canonical tool name as omp's `--tools` parser sees it: lowercase, with the `search`/`find` aliases resolved.
pub fn normalize_tool(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "search" => "grep".into(),
        "find" => "glob".into(),
        other => other.into(),
    }
}

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
    /// Start even though running git processes expose the token to the agent (local development only).
    pub insecure_allow_exposed_token: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            token_file: None,
            token_username: "x-access-token".into(),
            repos: Vec::new(),
            insecure_allow_exposed_token: false,
        }
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
        let mut config: Config = toml::from_str(text).context("invalid proxy config")?;
        config.validate()?;
        config.omp.tools = config.omp.tools.iter().map(|t| normalize_tool(t)).collect();
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
            for (field, value) in [("name", &p.name), ("model", &p.model)] {
                if value.is_empty() || value.starts_with('-') {
                    bail!("profile {field} {value:?} must be non-empty and must not start with '-'");
                }
            }
            if p.name.contains(':') {
                bail!("profile name {:?} must not contain ':'", p.name);
            }
            if !names.insert(&p.name) {
                bail!("duplicate profile name {:?}", p.name);
            }
        }
        if self.sessions.max_live == 0 {
            bail!("sessions.max_live must be at least 1");
        }
        if self.sessions.turn_timeout.is_zero() {
            bail!("sessions.turn_timeout must not be zero");
        }
        for tool in &self.omp.tools {
            let malformed = tool.contains(',') || tool.contains(char::is_whitespace);
            if malformed || !ALLOWED_TOOLS.contains(&normalize_tool(tool).as_str()) {
                bail!("tool {tool:?} is not allowed: the agent must stay read-only (allowed: {ALLOWED_TOOLS:?})");
            }
        }
        for arg in &self.omp.extra_args {
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            if RESERVED_OMP_FLAGS.contains(&flag) {
                bail!("omp.extra_args must not contain {flag}: the proxy controls it");
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
            if !is_valid_repo_url(&r.url) {
                bail!(
                    "invalid url for repo {:?}: use an https://, http:// or file:// url without embedded credentials",
                    r.name
                );
            }
            if self.git.token_file.is_some() && r.url.starts_with("http://") {
                bail!(
                    "repo {:?} uses http:// while git.token_file is set: the token would be sent in cleartext; use \
                     https://",
                    r.name
                );
            }
        }
        Ok(())
    }
}

fn is_valid_repo_url(url: &str) -> bool {
    let Some(rest) = ["https://", "http://", "file://"].iter().find_map(|scheme| url.strip_prefix(scheme)) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.contains('@')
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
        assert!(!c.git.insecure_allow_exposed_token);
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
            insecure_allow_exposed_token = true
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
        assert!(c.git.insecure_allow_exposed_token);
        assert_eq!(c.sessions.mirrors_dir(), PathBuf::from("/tmp/x/mirrors"));
    }

    #[test]
    fn rejects_write_tools() {
        let err = Config::from_toml(&format!("[omp]\ntools = [\"read\", \"bash\"]\n{MINIMAL}")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    fn with_omp(omp: &str) -> anyhow::Result<Config> {
        Config::from_toml(&format!("[omp]\n{omp}\n{MINIMAL}"))
    }

    #[test]
    fn rejects_tools_outside_the_allowlist() {
        for tool in ["Bash", "read,bash", " edit", "lsp", "memory_edit", "", "read bash"] {
            let err = with_omp(&format!("tools = [{tool:?}]")).unwrap_err();
            assert!(err.to_string().contains("read-only"), "{tool:?}: {err}");
        }
    }

    #[test]
    fn normalizes_tool_aliases() {
        let c = with_omp(r#"tools = ["Search", "FIND", "Read"]"#).unwrap();
        assert_eq!(c.omp.tools, ["grep", "glob", "read"]);
    }

    #[test]
    fn rejects_proxy_controlled_extra_args() {
        for args in [r#"["--tools=bash"]"#, r#"["--extension", "x"]"#, r#"["-e", "x"]"#, r#"["--approval-mode=ask"]"#] {
            let err = with_omp(&format!("extra_args = {args}")).unwrap_err();
            assert!(err.to_string().contains("the proxy controls it"), "{args}: {err}");
        }
        with_omp(r#"extra_args = ["--no-title"]"#).unwrap();
    }

    #[test]
    fn rejects_bad_repo_urls() {
        for url in ["https://user:pw@github.com/x.git", "ssh://git@x/y", "git@github.com:x/y.git", "/srv/x"] {
            let bad = format!("{MINIMAL}\n[[git.repo]]\nname = \"x\"\nurl = {url:?}\n");
            assert!(Config::from_toml(&bad).is_err(), "{url}");
        }
        for url in ["https://github.com/x.git", "http://host/p@x.git", "file:///srv/x"] {
            let ok = format!("{MINIMAL}\n[[git.repo]]\nname = \"x\"\nurl = {url:?}\n");
            Config::from_toml(&ok).unwrap();
        }
    }

    #[test]
    fn rejects_bad_session_limits_and_profiles() {
        assert!(Config::from_toml(&format!("[sessions]\nmax_live = 0\n{MINIMAL}")).is_err());
        assert!(Config::from_toml(&format!("[sessions]\nturn_timeout = \"0s\"\n{MINIMAL}")).is_err());
        assert!(Config::from_toml("[[profile]]\nname = \"\"\nmodel = \"m\"\n").is_err());
        assert!(Config::from_toml("[[profile]]\nname = \"a\"\nmodel = \"\"\n").is_err());
        assert!(Config::from_toml("[[profile]]\nname = \"a\"\nmodel = \"--tools=bash\"\n").is_err());
        assert!(Config::from_toml("[[profile]]\nname = \"-a\"\nmodel = \"m\"\n").is_err());
    }

    #[test]
    fn rejects_http_repo_urls_with_a_token() {
        let repo = "[[git.repo]]\nname = \"x\"\nurl = \"http://host/x.git\"\n";
        let err = Config::from_toml(&format!("{MINIMAL}\n[git]\ntoken_file = \"/t\"\n{repo}")).unwrap_err();
        assert!(err.to_string().contains("cleartext"), "{err}");
        let https = repo.replace("http://", "https://");
        Config::from_toml(&format!("{MINIMAL}\n[git]\ntoken_file = \"/t\"\n{https}")).unwrap();
        Config::from_toml(&format!("{MINIMAL}\n{repo}")).unwrap();
    }

    #[test]
    fn rejects_colons_in_profile_names() {
        let err = Config::from_toml("[[profile]]\nname = \"a:b\"\nmodel = \"m\"\n").unwrap_err();
        assert!(err.to_string().contains("':'"), "{err}");
    }

    #[test]
    fn rejects_missing_profiles_and_bad_repo_names() {
        assert!(Config::from_toml("").is_err());
        let bad = format!("{MINIMAL}\n[[git.repo]]\nname = \"../etc\"\nurl = \"https://x\"\n");
        assert!(Config::from_toml(&bad).is_err());
    }
}
