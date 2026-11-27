//! `config.toml`: optional, strict, never a place for secrets.
//!
//! Precedence is environment variable, then config file, then built-in default.
//! The file lives next to the database (`<data dir>/config.toml`). A missing file
//! means defaults; an unreadable, oversized, unknown or out-of-range setting is an
//! error rather than something silently ignored. Settings memb cannot honor yet
//! are rejected with a message instead of being accepted and dropped.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MRL_DIMS: [u32; 4] = [128, 256, 512, 768];

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub storage: Storage,
    pub scroll: Scroll,
    pub embeddings: Embeddings,
    pub curator: Curator,
    pub scheduler: Scheduler,
    pub retrieval: Retrieval,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub max_notes: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scroll {
    pub max_messages: usize,
    pub retain_tail: usize,
    pub min_prompt_chars: usize,
    pub dedup_reset_every_n_chunks: u32,
    pub ignore_dirs: Vec<String>,
}

/// `[embeddings.snowflake]`: Snowflake AI_EMBED as a cloud embedding backend. Having this
/// table is the opt-in: memb then sends note titles, descriptions and search queries to
/// Snowflake whenever no local model is in use.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snowflake {
    /// Account identifier, for example `ab14212.ap-southeast-1`.
    pub account: String,
    pub user: String,
    pub role: Option<String>,
    pub warehouse: String,
    /// The AI_EMBED model; default `snowflake-arctic-embed-m-v1.5`.
    pub model: Option<String>,
    /// Path to an unencrypted PKCS#8 private key (`BEGIN PRIVATE KEY`); `%VAR%` is expanded.
    /// The key stays in this file: it is never copied into config.toml or logged.
    pub key_file: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Embeddings {
    pub enabled: bool,
    /// Loopback llama-server port. Setting it sends note text and queries to that local service.
    pub port: Option<u16>,
    pub model_id: Option<String>,
    pub gemma_model_dir: Option<String>,
    /// Let the daily routine download the EmbeddingGemma 2 model (about 207 MB, from
    /// Hugging Face, checked against pinned SHA-256 values) when none is installed and no
    /// other backend is chosen. Off by default: `memb models install` does it on request.
    pub auto_download: bool,
    pub snowflake: Option<Snowflake>,
    pub mrl_dim: Option<u32>,
    // Accepted only to give a clear error: memb does not launch an embedding server.
    pub binary: Option<String>,
    pub model_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Curator {
    pub backend: String,
    pub cli_command: String,
    /// Tried when `cli_command` fails or its quota is used up. Unset: Codex (gpt-6-luna)
    /// when `cli_command` is the default agy command, otherwise none. Empty: none.
    pub fallback_command: Option<String>,
    pub chunk_to_chunk_threshold: f64,
    pub chunk_to_node_threshold: f64,
    pub staging_ttl_days: u32,
    /// Opt-in: let `memb daemon` run curation (sends conversation text to `cli_command`).
    pub daemon_enabled: bool,
    /// Apply valid proposals without a typed confirmation, with rollback as the safety net.
    /// Off by default; `memb proposals pause` is the kill switch.
    pub auto_apply: bool,
    /// Most proposals one run may apply automatically; the rest wait for review.
    pub max_auto_edits: u32,
}

/// `[retrieval]`: what a search returns and what a hook may inject into a prompt.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retrieval {
    /// Most tokens of note text one hook injects into a prompt, approximated as 4 bytes a token.
    pub max_inject_tokens: usize,
    /// Notes a search starts from, before the one-hop expansion adds parents and children.
    pub top_k: usize,
    /// A vector match with a cosine at or below this is dropped.
    pub score_floor: f64,
}

impl Default for Retrieval {
    fn default() -> Self {
        Self { max_inject_tokens: 750, top_k: memb_rag::DEFAULT_TOP_K, score_floor: 0.60 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scheduler {
    pub routine_wib_hour: u8,
    pub routine_wib_minute: u8,
    pub cooldown_hours: u64,
    pub idle_sweep_hours: u64,
    pub idle_sweep_min_messages: usize,
    pub obsidian_mirror_dir: Option<String>,
}

impl Default for Storage {
    fn default() -> Self {
        Self { max_notes: 50_000 }
    }
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            max_messages: 90,
            retain_tail: 10,
            min_prompt_chars: 100,
            dedup_reset_every_n_chunks: 7,
            ignore_dirs: Vec::new(),
        }
    }
}

impl Default for Embeddings {
    fn default() -> Self {
        Self {
            enabled: true,
            port: None,
            model_id: None,
            gemma_model_dir: None,
            auto_download: false,
            snowflake: None,
            mrl_dim: None,
            binary: None,
            model_path: None,
        }
    }
}

/// The default model: Antigravity `agy`, gemini-4-hydrogen-medium (stream protocol).
pub const AGY_COMMAND: &str = "agy --model gemini-4-hydrogen-medium --disable-slash-commands --input-format stream-json --output-format stream-json --print-timeout 150s";
/// The default fallback: Codex, gpt-6-luna at medium reasoning, isolated from the user's
/// Codex plugins, skills, hooks, memories and AGENTS.md.
pub const CODEX_COMMAND: &str = "codex exec -m gpt-6-luna -c model_reasoning_effort=medium -s read-only --skip-git-repo-check --ephemeral --ignore-user-config --ignore-rules -c project_doc_max_bytes=0 --disable apps --disable plugins --disable hooks --disable goals --disable multi_agent --disable in_app_browser --disable browser_use --disable computer_use --disable tool_suggest --disable skill_search --disable skill_mcp_dependency_install --disable view_image --disable sleep_tool --disable remote_plugin --disable plugin_sharing --disable memories";

impl Curator {
    /// The fallback model command, if any (see `fallback_command`).
    pub fn fallback(&self) -> Option<&str> {
        match self.fallback_command.as_deref() {
            Some(f) if f.trim().is_empty() => None,
            Some(f) => Some(f),
            None => (self.cli_command == AGY_COMMAND).then_some(CODEX_COMMAND),
        }
    }
}

impl Default for Curator {
    fn default() -> Self {
        Self {
            backend: "cli".into(),
            cli_command: AGY_COMMAND.into(),
            fallback_command: None,
            chunk_to_chunk_threshold: 0.72,
            chunk_to_node_threshold: 0.70,
            staging_ttl_days: 30,
            daemon_enabled: false,
            auto_apply: false,
            max_auto_edits: 5,
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            routine_wib_hour: 15,
            routine_wib_minute: 45,
            cooldown_hours: 20,
            idle_sweep_hours: 24,
            idle_sweep_min_messages: 45,
            obsidian_mirror_dir: None,
        }
    }
}

/// The parser quotes offending values in its messages (`"text"` or integer `42`); keep
/// the kind of error and the expected type and drop the value, so file contents are never
/// echoed. Key names (for example in "unknown field") are kept: they name the setting.
fn safe_message(message: &str) -> String {
    let first = message.lines().next().unwrap_or("syntax error");
    if first.starts_with("unknown field") || !(first.contains('"') || first.contains('`')) {
        return first.to_owned();
    }
    let kind = first.split(':').next().unwrap_or("invalid value");
    match first.find(", expected ") {
        Some(i) => format!("{kind}{}", &first[i..]),
        None => kind.to_owned(),
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// Expand `%NAME%` against the process environment. An unset variable is an error,
/// and the result must be an absolute path.
pub fn expand_path(raw: &str, key: &str) -> Result<PathBuf, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let end = tail.find('%').ok_or_else(|| format!("{key}: unmatched '%'"))?;
        let name = &tail[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(format!("{key}: invalid variable name between '%' signs"));
        }
        let value = std::env::var(name)
            .map_err(|_| format!("{key}: environment variable {name} is not set"))?;
        out.push_str(&value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    let path = PathBuf::from(out);
    if !path.is_absolute() {
        return Err(format!("{key}: must be an absolute path"));
    }
    Ok(path)
}

fn within<T: PartialOrd + std::fmt::Display>(key: &str, v: T, lo: T, hi: T) -> Result<(), String> {
    if v < lo || v > hi {
        // The offending value is deliberately not repeated.
        return Err(format!("{key} must be within {lo}..={hi}"));
    }
    Ok(())
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at line {}", line_of(text, s.start)))
                .unwrap_or_default();
            format!("config.toml is not valid{at}: {}", safe_message(e.message()))
        })?;
        config.validate()?;
        Ok(config)
    }

    /// `<root>/config.toml`; absent means defaults.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("config.toml");
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err("config.toml cannot be read".into()),
        };
        if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
            return Err(format!(
                "config.toml must be a regular file of at most {MAX_CONFIG_BYTES} bytes"
            ));
        }
        let bytes = std::fs::read(&path).map_err(|_| "config.toml cannot be read")?;
        Self::parse(std::str::from_utf8(&bytes).map_err(|_| "config.toml is not UTF-8")?)
    }

    pub fn validate(&self) -> Result<(), String> {
        within("storage.max_notes", self.storage.max_notes, 1, 50_000)?;
        let s = &self.scroll;
        within("scroll.max_messages", s.max_messages, 10, 1000)?;
        within("scroll.retain_tail", s.retain_tail, 1, s.max_messages / 2)?;
        within("scroll.min_prompt_chars", s.min_prompt_chars, 1, 10_000)?;
        within("scroll.dedup_reset_every_n_chunks", s.dedup_reset_every_n_chunks, 1, 1000)?;
        if s.ignore_dirs.len() > 64
            || s.ignore_dirs.iter().any(|d| d.trim().is_empty() || d.len() > 512)
        {
            return Err("scroll.ignore_dirs: at most 64 non-empty patterns of at most 512 bytes".into());
        }
        let e = &self.embeddings;
        if e.binary.is_some() || e.model_path.is_some() {
            return Err("embeddings.binary and embeddings.model_path are not supported: memb does not start an embedding server. Run llama-server yourself and set embeddings.port, or set embeddings.gemma_model_dir for the in-process EmbeddingGemma 2 model".into());
        }
        if e.port == Some(0) {
            return Err("embeddings.port must be 1..=65535".into());
        }
        if let Some(id) = &e.model_id {
            if id.is_empty()
                || id.len() > 128
                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-@".contains(&b))
            {
                return Err("embeddings.model_id: 1 to 128 characters of letters, digits and . _ : - @".into());
            }
        }
        if let Some(dir) = &e.gemma_model_dir {
            expand_path(dir, "embeddings.gemma_model_dir")?;
        }
        if let Some(sf) = &e.snowflake {
            let key = expand_path(&sf.key_file, "embeddings.snowflake.key_file")?;
            memb_embed::snowflake::SnowflakeConfig {
                account: sf.account.clone(),
                user: sf.user.clone(),
                role: sf.role.clone(),
                warehouse: sf.warehouse.clone(),
                model: sf.model.clone().unwrap_or_else(|| memb_embed::snowflake::DEFAULT_MODEL.to_string()),
                key_path: key,
                base_url: None,
            }
            .validate()
            .map_err(|e| format!("embeddings.snowflake: {e}"))?;
        }
        if let Some(d) = e.mrl_dim {
            if !MRL_DIMS.contains(&d) {
                return Err(format!("embeddings.mrl_dim must be one of {MRL_DIMS:?}"));
            }
        }
        let c = &self.curator;
        if c.backend != "cli" {
            return Err("curator.backend must be \"cli\", the only supported backend".into());
        }
        memb_curator::backend::Backend::from_command(&c.cli_command)
            .map_err(|e| format!("curator.cli_command: {e}"))?;
        if let Some(f) = c.fallback() {
            memb_curator::backend::Backend::from_command(f)
                .map_err(|e| format!("curator.fallback_command: {e}"))?;
        }
        let fixed = Curator::default();
        if c.chunk_to_chunk_threshold != fixed.chunk_to_chunk_threshold
            || c.chunk_to_node_threshold != fixed.chunk_to_node_threshold
        {
            return Err("curator thresholds are fixed at 0.72 and 0.70; remove those settings".into());
        }
        within("curator.staging_ttl_days", c.staging_ttl_days, 1, 365)?;
        within("curator.max_auto_edits", c.max_auto_edits, 1, 50)?;
        let r = &self.retrieval;
        within("retrieval.max_inject_tokens", r.max_inject_tokens, 50, 8000)?;
        within("retrieval.top_k", r.top_k, 1, 20)?;
        within("retrieval.score_floor", r.score_floor, 0.0, 0.95)?;
        let d = &self.scheduler;
        within("scheduler.routine_wib_hour", d.routine_wib_hour, 0, 23)?;
        within("scheduler.routine_wib_minute", d.routine_wib_minute, 0, 59)?;
        within("scheduler.cooldown_hours", d.cooldown_hours, 1, 168)?;
        within("scheduler.idle_sweep_hours", d.idle_sweep_hours, 1, 720)?;
        within("scheduler.idle_sweep_min_messages", d.idle_sweep_min_messages, 1, 10_000)?;
        if d.idle_sweep_min_messages <= self.scroll.retain_tail {
            return Err("scheduler.idle_sweep_min_messages must exceed scroll.retain_tail".into());
        }
        if let Some(dir) = &d.obsidian_mirror_dir {
            expand_path(dir, "scheduler.obsidian_mirror_dir")?;
        }
        Ok(())
    }

    /// Fill embedding settings into the process environment where the user has not
    /// already set them (environment wins). Call once at startup, before other threads.
    pub fn apply_embedding_env(&self) -> Result<(), String> {
        let set = |name: &str, value: Option<String>| {
            if let (Some(v), None) = (value, std::env::var_os(name)) {
                std::env::set_var(name, v);
            }
        };
        let e = &self.embeddings;
        if !e.enabled {
            set("MEMB_EMBED_DISABLE", Some("1".into()));
            return Ok(());
        }
        set("MEMB_EMBED_PORT", e.port.map(|p| p.to_string()));
        set("MEMB_EMBED_MODEL_ID", e.model_id.clone());
        set("MEMB_EMBED_MRL_DIM", e.mrl_dim.map(|d| d.to_string()));
        if let Some(dir) = &e.gemma_model_dir {
            let dir = expand_path(dir, "embeddings.gemma_model_dir")?;
            set("MEMB_GEMMA_MODEL_DIR", Some(dir.to_string_lossy().into_owned()));
        }
        if let Some(sf) = &e.snowflake {
            let key = expand_path(&sf.key_file, "embeddings.snowflake.key_file")?;
            set("MEMB_SNOWFLAKE_ACCOUNT", Some(sf.account.clone()));
            set("MEMB_SNOWFLAKE_USER", Some(sf.user.clone()));
            set("MEMB_SNOWFLAKE_ROLE", sf.role.clone());
            set("MEMB_SNOWFLAKE_WAREHOUSE", Some(sf.warehouse.clone()));
            set("MEMB_SNOWFLAKE_MODEL", sf.model.clone());
            set("MEMB_SNOWFLAKE_KEY", Some(key.to_string_lossy().into_owned()));
        }
        Ok(())
    }

    /// Hand `[retrieval]` to the search code (explicit environment variables win).
    pub fn apply_retrieval_env(&self) {
        let r = &self.retrieval;
        for (name, value) in [("MEMB_RETRIEVAL_TOP_K", r.top_k.to_string()), ("MEMB_RETRIEVAL_SCORE_FLOOR", r.score_floor.to_string())] {
            if std::env::var_os(name).is_none() {
                std::env::set_var(name, value);
            }
        }
    }

    /// The most bytes of context a hook may inject (4 bytes approximate one token).
    pub fn inject_budget_bytes(&self) -> usize {
        self.retrieval.max_inject_tokens * 4
    }

    /// The directory the daemon mirrors into: a command-line directory wins over the config.
    pub fn mirror_root(&self, cli: Option<&Path>) -> Result<Option<PathBuf>, String> {
        match cli {
            Some(dir) => Ok(Some(dir.to_path_buf())),
            None => self.mirror_dir(),
        }
    }

    pub fn mirror_dir(&self) -> Result<Option<PathBuf>, String> {
        self.scheduler
            .obsidian_mirror_dir
            .as_deref()
            .map(|d| expand_path(d, "scheduler.obsidian_mirror_dir"))
            .transpose()
    }
}

/// Tests that change the process environment take this lock (config and models tests share it).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    use super::ENV_LOCK as ENV;
    const EMBED_VARS: [&str; 12] = [
        "MEMB_SNOWFLAKE_ACCOUNT",
        "MEMB_SNOWFLAKE_USER",
        "MEMB_SNOWFLAKE_ROLE",
        "MEMB_SNOWFLAKE_WAREHOUSE",
        "MEMB_SNOWFLAKE_MODEL",
        "MEMB_SNOWFLAKE_KEY",
        "MEMB_EMBED_PORT",
        "MEMB_EMBED_MODEL_ID",
        "MEMB_EMBED_MRL_DIM",
        "MEMB_GEMMA_MODEL_DIR",
        "MEMB_EMBED_DISABLE",
        "MEMB_CFG_TEST_ROOT",
    ];

    /// Tests that touch the process environment run one at a time and restore it.
    fn with_clean_env<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<_> = EMBED_VARS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for k in EMBED_VARS {
            std::env::remove_var(k);
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    fn clear_embedding_vars() {
        for k in EMBED_VARS.iter().filter(|k| **k != "MEMB_CFG_TEST_ROOT") {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn the_snowflake_table_is_validated_exported_and_never_overrides_the_environment() {
        with_clean_env(|| {
            let toml = "[embeddings.snowflake]
account = \"ab14212.ap-southeast-1\"
user = \"SOMEONE\"
warehouse = \"WH\"
key_file = \"%MEMB_CFG_TEST_ROOT%/secrets/key.p8\"
";
            std::env::set_var("MEMB_CFG_TEST_ROOT", if cfg!(windows) { r"C:\data" } else { "/data" });
            let cfg = Config::parse(toml).unwrap();
            let sf = cfg.embeddings.snowflake.clone().unwrap();
            assert_eq!((sf.role.as_deref(), sf.model.as_deref()), (None, None), "role and model are optional");
            cfg.apply_embedding_env().unwrap();
            assert_eq!(std::env::var("MEMB_SNOWFLAKE_ACCOUNT").unwrap(), "ab14212.ap-southeast-1");
            assert_eq!(std::env::var("MEMB_SNOWFLAKE_USER").unwrap(), "SOMEONE");
            assert_eq!(std::env::var("MEMB_SNOWFLAKE_WAREHOUSE").unwrap(), "WH");
            let key = std::env::var("MEMB_SNOWFLAKE_KEY").unwrap();
            assert!(key.ends_with("key.p8") && key.contains("secrets") && !key.contains('%'), "{key}");
            assert!(std::env::var_os("MEMB_SNOWFLAKE_ROLE").is_none() && std::env::var_os("MEMB_SNOWFLAKE_MODEL").is_none());
            // The environment wins over the file.
            clear_embedding_vars();
            std::env::set_var("MEMB_SNOWFLAKE_USER", "OTHER");
            cfg.apply_embedding_env().unwrap();
            assert_eq!(std::env::var("MEMB_SNOWFLAKE_USER").unwrap(), "OTHER");
            // Disabled embeddings export nothing about Snowflake.
            clear_embedding_vars();
            Config::parse(&format!("[embeddings]
enabled = false
{toml}")).unwrap().apply_embedding_env().unwrap();
            assert!(std::env::var_os("MEMB_SNOWFLAKE_ACCOUNT").is_none());
            // Bad values and missing required keys are refused, and the key text is not a config value at all.
            for bad in [
                "[embeddings.snowflake]
account = \"a b\"
user = \"U\"
warehouse = \"W\"
key_file = \"k\"
",
                "[embeddings.snowflake]
account = \"a\"
user = \"U\"
warehouse = \"W\"
",
                "[embeddings.snowflake]
account = \"a\"
user = \"U\"
warehouse = \"W\"
key_file = \"k\"
private_key = \"x\"
",
                "[embeddings.snowflake]
account = \"a\"
user = \"U\"
warehouse = \"W\"
key_file = \"k\"
model = \"m;DROP\"
",
            ] {
                assert!(Config::parse(bad).is_err(), "{bad}");
            }
        });
    }

    #[test]
    fn empty_and_missing_config_mean_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert!(Config::default().embeddings.enabled);
        let dir = std::env::temp_dir().join(format!("memb-cfg-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Config::load(&dir).unwrap(), Config::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn defaults_match_the_kit_example() {
        let kit = Config::parse(
            "[storage]\nmax_notes = 50000\n[scroll]\nmax_messages = 90\nretain_tail = 10\nmin_prompt_chars = 100\ndedup_reset_every_n_chunks = 7\nignore_dirs = [\"*/tmp/*\", \"*/playground/*\"]\n[curator]\nbackend = \"cli\"\ncli_command = \"agy --model gemini-4-hydrogen-medium --disable-slash-commands --input-format stream-json --output-format stream-json --print-timeout 150s\"\nchunk_to_chunk_threshold = 0.72\nchunk_to_node_threshold = 0.70\nstaging_ttl_days = 30\n[scheduler]\nroutine_wib_hour = 15\nroutine_wib_minute = 45\ncooldown_hours = 20\nidle_sweep_hours = 24\nidle_sweep_min_messages = 45\n",
        )
        .unwrap();
        assert_eq!(kit.scroll.ignore_dirs.len(), 2);
        let without_patterns = Config {
            scroll: Scroll { ignore_dirs: vec![], ..kit.scroll.clone() },
            ..kit
        };
        assert_eq!(without_patterns, Config::default());
    }

    #[test]
    fn shipped_example_file_parses_to_the_defaults() {
        let example = include_str!("../../../config.example.toml");
        assert_eq!(Config::parse(example).unwrap(), Config::default());
        // Every commented-out optional line must also be valid once uncommented.
        const OPTIONAL: [&str; 5] =
            ["port =", "model_id =", "gemma_model_dir =", "mrl_dim =", "obsidian_mirror_dir ="];
        let uncommented: String = example
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if OPTIONAL.iter().any(|k| rest.starts_with(k)) => format!("{rest}\n"),
                _ => format!("{l}\n"),
            })
            .collect();
        with_clean_env(|| {
            std::env::set_var(
                "LOCALAPPDATA",
                if cfg!(windows) { r"C:\Users\x\AppData\Local" } else { "/x" },
            );
            let all = Config::parse(&uncommented).unwrap();
            assert_eq!(all.embeddings.port, Some(18432));
            assert_eq!(all.embeddings.mrl_dim, Some(256));
            assert!(all.scheduler.obsidian_mirror_dir.is_some());
            assert!(all.embeddings.gemma_model_dir.is_some());
        });
    }

    #[test]
    fn unknown_keys_and_wrong_types_fail_closed() {
        for bad in [
            "[storage]\nmax_noets = 1\n",
            "[nope]\nx = 1\n",
            "[storage]\nmax_notes = \"many\"\n",
            "storage = 3\n",
            "[[scroll]]\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ranges_are_enforced() {
        for bad in [
            "[storage]\nmax_notes = 0\n",
            "[storage]\nmax_notes = 50001\n",
            "[scroll]\nmax_messages = 9\n",
            "[scroll]\nmax_messages = 20\nretain_tail = 11\n",
            "[scroll]\nretain_tail = 0\n",
            "[scroll]\nmin_prompt_chars = 0\n",
            "[scroll]\ndedup_reset_every_n_chunks = 0\n",
            "[scroll]\nignore_dirs = [\"\"]\n",
            "[embeddings]\nport = 0\n",
            "[embeddings]\nmrl_dim = 100\n",
            "[embeddings]\nmodel_id = \"bad id\"\n",
            "[scheduler]\nroutine_wib_hour = 24\n",
            "[scheduler]\nroutine_wib_minute = 60\n",
            "[scheduler]\ncooldown_hours = 0\n",
            "[scheduler]\ncooldown_hours = 169\n",
            "[scheduler]\nidle_sweep_hours = 0\n",
            "[scheduler]\nidle_sweep_min_messages = 0\n",
            "[scheduler]\nidle_sweep_min_messages = 10\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        assert!(Config::parse("[scroll]\nmax_messages = 20\nretain_tail = 10\n[scheduler]\nidle_sweep_min_messages = 11\n").is_ok());
        assert!(Config::parse("[scheduler]\nroutine_wib_hour = 0\nroutine_wib_minute = 0\n").is_ok());
    }

    #[test]
    fn retrieval_settings_default_export_to_the_environment_and_never_override_it() {
        let d = Config::default();
        assert_eq!((d.retrieval.max_inject_tokens, d.retrieval.top_k), (750, 5));
        assert_eq!(d.inject_budget_bytes(), 3000);
        assert_eq!(d.retrieval.score_floor as f32, memb_rag::DEFAULT_SCORE_FLOOR, "config and search agree on the default floor");
        let c = Config::parse("[retrieval]
max_inject_tokens = 100
top_k = 3
score_floor = 0.5
").unwrap();
        assert_eq!((c.inject_budget_bytes(), c.retrieval.top_k, c.retrieval.score_floor), (400, 3, 0.5));
        with_clean_env(|| {
            let saved = (std::env::var_os("MEMB_RETRIEVAL_TOP_K"), std::env::var_os("MEMB_RETRIEVAL_SCORE_FLOOR"));
            std::env::remove_var("MEMB_RETRIEVAL_TOP_K");
            std::env::remove_var("MEMB_RETRIEVAL_SCORE_FLOOR");
            c.apply_retrieval_env();
            assert_eq!(std::env::var("MEMB_RETRIEVAL_TOP_K").unwrap(), "3");
            assert_eq!(std::env::var("MEMB_RETRIEVAL_SCORE_FLOOR").unwrap(), "0.5");
            std::env::set_var("MEMB_RETRIEVAL_TOP_K", "9");
            Config::default().apply_retrieval_env();
            assert_eq!(std::env::var("MEMB_RETRIEVAL_TOP_K").unwrap(), "9", "the environment wins");
            for (name, v) in [("MEMB_RETRIEVAL_TOP_K", saved.0), ("MEMB_RETRIEVAL_SCORE_FLOOR", saved.1)] {
                match v {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        });
    }

    #[test]
    fn unsupported_kit_keys_get_a_clear_error_not_silence() {
        let e = Config::parse("[embeddings]\nbinary = \"llama-server.exe\"\n").unwrap_err();
        assert!(e.contains("does not start an embedding server"), "{e}");
        assert!(Config::parse("[embeddings]\nmodel_path = \"x\"\n").is_err());
        let e = Config::parse("[curator]\nchunk_to_chunk_threshold = 0.5\n").unwrap_err();
        assert!(e.contains("thresholds are fixed"), "{e}");
        assert!(Config::parse("[curator]\nchunk_to_node_threshold = 0.71\n").is_err());
        let e = Config::parse("[curator]\nbackend = \"api\"\n").unwrap_err();
        assert!(e.contains("curator.backend"), "{e}");
    }

    #[test]
    fn curator_command_and_staging_limit_are_configurable_and_checked() {
        let c = Config::parse("[curator]\ncli_command = \"codex exec\"\nstaging_ttl_days = 7\n").unwrap();
        assert_eq!((c.curator.cli_command.as_str(), c.curator.staging_ttl_days), ("codex exec", 7));
        assert!(Config::parse("[curator]\nstaging_ttl_days = 365\n").is_ok());
        let auto = Config::parse("[curator]\nauto_apply = true\nmax_auto_edits = 50\n").unwrap();
        assert!(auto.curator.auto_apply && auto.curator.max_auto_edits == 50);
        assert!(!Config::default().curator.auto_apply, "auto-apply is off by default");
        let default = Config::default().curator.cli_command;
        assert!(default.starts_with("agy --model gemini-4-hydrogen-medium "), "{default}");
        let c = Config::default().curator;
        assert_eq!(c.fallback(), Some(CODEX_COMMAND));
        assert!(CODEX_COMMAND.starts_with("codex exec -m gpt-6-luna "));
        // A command of your own gets no surprise fallback; an empty setting turns it off.
        let own = Config::parse("[curator]\ncli_command = \"mymodel -p\"\n").unwrap().curator;
        assert_eq!(own.fallback(), None);
        let off = Config::parse("[curator]\nfallback_command = \"\"\n").unwrap().curator;
        assert_eq!(off.fallback(), None);
        let set = Config::parse("[curator]\nfallback_command = \"mymodel -p\"\n").unwrap().curator;
        assert_eq!(set.fallback(), Some("mymodel -p"));
        assert!(Config::parse("[curator]\nfallback_command = \"x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x x\"\n").is_err());
        assert!(Config::parse(&format!("[curator]
cli_command = \"{default}\"
")).is_ok(), "the default command must itself be valid");
        for bad in [0, 51] {
            let e = Config::parse(&format!("[curator]\nmax_auto_edits = {bad}\n")).unwrap_err();
            assert!(e.contains("curator.max_auto_edits"), "{e}");
        }
        for bad in ["0", "366", "99999"] {
            let e = Config::parse(&format!("[curator]\nstaging_ttl_days = {bad}\n")).unwrap_err();
            assert!(e.contains("curator.staging_ttl_days"), "{e}");
            assert!(!e.contains("99999"), "{e}");
        }
        let e = Config::parse("[curator]\ncli_command = \"   \"\n").unwrap_err();
        assert!(e.contains("curator.cli_command"), "{e}");
        let long = format!("[curator]\ncli_command = \"{}\"\n", "SECRETWORD".repeat(40));
        let e = Config::parse(&long).unwrap_err();
        assert!(!e.contains("SECRETWORD"), "{e}");
    }

    #[test]
    fn errors_name_the_setting_but_never_echo_contents() {
        let e = Config::parse("[storage]\nmax_notes = \"SECRET-VALUE-123\"\n").unwrap_err();
        assert!(!e.contains("SECRET-VALUE-123"), "{e}");
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn range_and_type_errors_never_repeat_the_value() {
        for (text, value) in [
            ("[storage]\nmax_notes = 98765\n", "98765"),
            ("[embeddings]\nport = 70000\n", "70000"),
            ("[scroll]\nignore_dirs = [42424]\n", "42424"),
            ("[scroll]\nmax_messages = 5555\n", "5555"),
            ("[scheduler]\ncooldown_hours = 31337\n", "31337"),
            ("[embeddings]\nmrl_dim = 31415\n", "31415"),
            ("[storage]\nmax_notes = 99999999999999999999999999\n", "99999999999999999999999999"),
            ("[scroll]\nignore_dirs = [-4242]\n", "4242"),
            ("[storage]\nmax_notes = 12.75\n", "12.75"),
            ("[storage]\nmax_notes = \"sekrit\"\n", "sekrit"),
            ("[embeddings]\nmodel_id = \"sekrit id\"\n", "sekrit"),
        ] {
            let e = Config::parse(text).unwrap_err();
            assert!(!e.contains(value), "{text} -> {e}");
        }
    }

    #[test]
    fn path_expansion_requires_set_variables_and_absolute_results() {
        with_clean_env(|| {
            std::env::set_var("MEMB_CFG_TEST_ROOT", if cfg!(windows) { r"C:\data" } else { "/data" });
            let p = expand_path("%MEMB_CFG_TEST_ROOT%/vault", "k").unwrap();
            assert!(p.to_string_lossy().contains("vault") && p.is_absolute());
            assert!(expand_path("%MEMB_CFG_TEST_UNSET_XYZ%/v", "k").is_err());
            assert!(expand_path("relative/vault", "k").is_err());
            assert!(expand_path("%unterminated", "k").is_err());
            assert!(expand_path("%bad name%/v", "k").is_err());
        });
    }

    #[test]
    fn embedding_settings_reach_the_environment_and_the_environment_wins() {
        with_clean_env(|| {
            std::env::set_var("MEMB_CFG_TEST_ROOT", if cfg!(windows) { r"C:\models" } else { "/models" });
            let cfg = Config::parse(
                "[embeddings]\nport = 18432\nmodel_id = \"m-1\"\nmrl_dim = 256\ngemma_model_dir = \"%MEMB_CFG_TEST_ROOT%/gemma\"\n",
            )
            .unwrap();
            cfg.apply_embedding_env().unwrap();
            assert_eq!(std::env::var("MEMB_EMBED_PORT").unwrap(), "18432");
            assert_eq!(std::env::var("MEMB_EMBED_MODEL_ID").unwrap(), "m-1");
            assert_eq!(std::env::var("MEMB_EMBED_MRL_DIM").unwrap(), "256");
            let dir = std::env::var("MEMB_GEMMA_MODEL_DIR").unwrap();
            assert!(dir.contains("models") && dir.ends_with("gemma") && !dir.contains('%'), "{dir}");
            assert!(std::env::var_os("MEMB_EMBED_DISABLE").is_none());

            // A variable that is already set is never overwritten by the file.
            clear_embedding_vars();
            std::env::set_var("MEMB_EMBED_MRL_DIM", "128");
            cfg.apply_embedding_env().unwrap();
            assert_eq!(std::env::var("MEMB_EMBED_MRL_DIM").unwrap(), "128");
            assert_eq!(std::env::var("MEMB_EMBED_PORT").unwrap(), "18432");

            // enabled = false sets only the disable switch.
            clear_embedding_vars();
            Config::parse("[embeddings]\nenabled = false\nport = 18432\n")
                .unwrap()
                .apply_embedding_env()
                .unwrap();
            assert_eq!(std::env::var("MEMB_EMBED_DISABLE").unwrap(), "1");
            assert!(std::env::var_os("MEMB_EMBED_PORT").is_none());

            // An unset variable inside a path is an error, not a silent skip.
            assert!(Config::parse("[embeddings]\ngemma_model_dir = \"%MEMB_CFG_NOT_SET_ANYWHERE%/x\"\n").is_err());
        });
    }

    #[test]
    fn mirror_directory_comes_from_config_unless_the_command_line_gives_one() {
        with_clean_env(|| {
            std::env::set_var("MEMB_CFG_TEST_ROOT", if cfg!(windows) { r"C:\vault" } else { "/vault" });
            let cfg = Config::parse("[scheduler]\nobsidian_mirror_dir = \"%MEMB_CFG_TEST_ROOT%/out\"\n").unwrap();
            let configured = cfg.mirror_root(None).unwrap().unwrap();
            assert!(configured.is_absolute() && configured.ends_with("out") && configured.to_string_lossy().contains("vault"));
            let cli = if cfg!(windows) { PathBuf::from(r"D:\cli") } else { PathBuf::from("/cli") };
            assert_eq!(cfg.mirror_root(Some(&cli)).unwrap().unwrap(), cli);
            assert_eq!(Config::default().mirror_root(None).unwrap(), None);
        });
    }

    #[test]
    fn oversized_or_non_regular_config_is_refused() {
        let dir = std::env::temp_dir().join(format!("memb-cfg-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "#".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        assert!(Config::load(&dir).is_err());
        std::fs::remove_file(dir.join("config.toml")).unwrap();
        std::fs::create_dir(dir.join("config.toml")).unwrap();
        assert!(Config::load(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
