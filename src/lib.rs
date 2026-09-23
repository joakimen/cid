//! `cid` — select repositories, files, notes, git branches and worktrees,
//! GitHub pull requests and running processes from one fuzzy finder, and build
//! and install the project you are standing in.
//!
//! The crate is split into an I/O-free core and an imperative shell: the
//! [`cmd`] modules read the environment, touch the filesystem, shell out to
//! `git`/`gh`, and drive interactive selection; everything else is pure.
//! [`Ctx`] resolves the environment once and hands it to every command.

pub mod binding;
pub mod cmd;
pub mod config;
pub mod files;
pub mod gh;
pub mod git;
pub mod history;
pub mod logger;
pub mod note;
pub mod path;
pub mod proc;
pub mod project;
pub mod prompt;
pub mod recent;
pub mod repo;
pub mod select;
pub mod shell;
pub mod stats;
pub mod term;
pub mod walk;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What `cid --version` reports.
///
/// The crate version when this commit is a release — sitting exactly on a tag
/// with nothing modified — and `<version>-dev.<sha>[.dirty]` otherwise.
/// Computed at compile time by `build.rs`.
pub const VERSION: &str = env!("CID_VERSION");

use config::Config;
use logger::Logger;

/// The wall clock, in Unix seconds, as [`recent`] counts it. Before 1970 is not
/// a time anything here has to represent.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Write the store, creating its directory — the config directory exists on
/// every machine that has a config, and not on one that has never had one.
fn write_recent(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// A subprocess that already explained its own failure on stderr. Propagated
/// instead of a message so the command exits with the child's status without
/// cid restating what the user just read.
#[derive(Debug)]
pub struct Reported(pub i32);

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "subprocess exited with status {}", self.0)
    }
}

impl std::error::Error for Reported {}

/// Resolved runtime environment shared by every command.
///
/// Built once from the process environment and CLI flags, then passed by
/// reference so the command implementations stay free of environment lookups.
pub struct Ctx {
    home: PathBuf,
    home_s: String,
    pwd_s: String,
    /// The resolved config file (`config.toml`, or a legacy `config.json`).
    pub config_path: PathBuf,
    /// The known-files list, beside the config file.
    pub files_path: PathBuf,
    /// What has been selected before, beside the config file.
    pub recent_path: PathBuf,
    /// The standalone `kf` tool's config, read once to migrate its list.
    pub legacy_kf_path: PathBuf,
    /// fish's history file, which `cid history` reads.
    pub history_path: PathBuf,
    /// The log every run appends itself to, which `cid stats` reads.
    pub stats_path: PathBuf,
    /// This machine's offset from UTC, for dating history entries.
    utc_offset: time::UtcOffset,
    /// The editor `cid edit` launches, from the environment.
    editor: Option<String>,
    /// The command `cid note open` launches: `[note] editor`, else the one
    /// above. Resolved here so no command looks the environment up itself.
    note_editor: Option<String>,
    /// `GH_REPO`, which names the repository `gh` acts on when the working
    /// directory is not one.
    gh_repo: Option<String>,
    /// Whether printed output carries colour, resolved once from `--color`,
    /// `CID_NO_COLOR` and whether stdout is a terminal.
    color: bool,
    pub config: Config,
    pub log: Logger,
}

impl Ctx {
    /// Resolve the environment and load configuration.
    pub fn load(
        config_flag: Option<&str>,
        verbose: bool,
        color: term::ColorChoice,
    ) -> Result<Self> {
        // `$HOME`, not a passwd lookup: the tests and the demo fixture both
        // apply their sandbox by setting it, and a home read from the passwd
        // file would silently disagree with the one the shell is using.
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .context("determining home directory: $HOME is unset")?;
        let cwd = std::env::current_dir().context("determining working directory")?;
        let pwd = path::resolve_pwd(
            std::env::var("PWD").ok().as_deref(),
            cwd,
            |claimed, actual| match (claimed.canonicalize(), actual.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            },
        );

        let cid_env = std::env::var(config::CONFIG_ENV_VAR).ok();
        let xdg_env = std::env::var(config::XDG_ENV_VAR).ok();

        let config_path = config::resolve_config_path(
            config_flag,
            cid_env.as_deref(),
            xdg_env.as_deref(),
            &home,
            |p| p.exists(),
        );
        let files_path = config::files_path(&config_path);
        let recent_path = recent::path(&config_path);
        let legacy_kf_path = config::legacy_kf_path(xdg_env.as_deref(), &home);
        let config = config::load_config(&config_path)?;

        // fish's data directory, a different XDG variable from the one above.
        let data_home = std::env::var(history::XDG_DATA_ENV_VAR).ok();
        let history_path =
            history::history_path(config.history.file.as_deref(), data_home.as_deref(), &home);
        let stats_path = stats::path(data_home.as_deref(), &home);

        // Read at the top of the process because it cannot be read later:
        // `time` refuses to determine the local offset once the process is
        // multi-threaded, and skim has threads running by the time a history
        // row wants dating.
        let utc_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

        let editor = config::resolve_editor(
            std::env::var("VISUAL").ok().as_deref(),
            std::env::var("EDITOR").ok().as_deref(),
        );

        // `[note] editor` first, since it exists to be something other than
        // the editor: `glow` reads a note where `nvim` writes one.
        let note_editor = config
            .note
            .editor
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .or_else(|| editor.clone());

        let gh_repo = std::env::var("GH_REPO")
            .ok()
            .filter(|repo| !repo.trim().is_empty());

        Ok(Self {
            home_s: home.to_string_lossy().into_owned(),
            pwd_s: pwd.to_string_lossy().into_owned(),
            home,
            config_path,
            files_path,
            recent_path,
            legacy_kf_path,
            history_path,
            stats_path,
            utc_offset,
            editor,
            note_editor,
            gh_repo,
            color: color.for_stdout(),
            config,
            log: Logger::new(verbose),
        })
    }

    /// Whether printed output should carry ANSI colour. Does not govern the
    /// selector, which only ever draws on a terminal.
    pub fn color(&self) -> bool {
        self.color
    }

    /// The resolved editor command, or `None` when nothing is set. For
    /// reporting; [`Ctx::editor`] is what launching goes through.
    pub fn editor_setting(&self) -> Option<&str> {
        self.editor.as_deref()
    }

    /// The editor command to launch, split into program and arguments.
    pub fn editor(&self) -> Result<Vec<String>> {
        let command = self
            .editor
            .as_deref()
            .context("no editor set — set $VISUAL or $EDITOR")?;
        let parts = config::split_editor(command);
        if parts.is_empty() {
            anyhow::bail!("the configured editor is empty");
        }
        Ok(parts)
    }

    /// The command `cid note open` launches, or `None` when neither
    /// `[note] editor` nor the environment names one. For reporting;
    /// [`Ctx::note_editor`] is what launching goes through.
    pub fn note_editor_setting(&self) -> Option<&str> {
        self.note_editor.as_deref()
    }

    /// The command `cid note open` launches, split into program and
    /// arguments.
    pub fn note_editor(&self) -> Result<Vec<String>> {
        let command = self
            .note_editor
            .as_deref()
            .context("no editor set — set `[note] editor`, $VISUAL or $EDITOR")?;
        let parts = config::split_editor(command);
        if parts.is_empty() {
            anyhow::bail!("the configured note editor is empty");
        }
        Ok(parts)
    }

    /// The repository `GH_REPO` names, if it names one. `gh` reads the variable
    /// itself; cid reads it only to know whether a command that needs a
    /// repository already has one.
    pub fn gh_repo(&self) -> Option<&str> {
        self.gh_repo.as_deref()
    }

    /// This machine's offset from UTC, resolved once at startup.
    pub fn utc_offset(&self) -> time::UtcOffset {
        self.utc_offset
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn home_str(&self) -> &str {
        &self.home_s
    }

    pub fn pwd_str(&self) -> &str {
        &self.pwd_s
    }

    /// Reorder `items` so what has been selected before comes first, and hand
    /// back the clock reading it was ordered against — [`Ctx::remember`] dates
    /// the selection with the same one, so a row cannot be scored against a
    /// moment before it was chosen.
    ///
    /// A store that cannot be read leaves the order alone: an unreadable file
    /// of past selections is not a reason to fail the selection in hand.
    pub fn by_recency<T>(&self, items: Vec<T>, key: impl Fn(&T) -> &str) -> (Vec<T>, i64) {
        let now = unix_now();
        if !self.config.selector.recent {
            return (items, now);
        }
        let uses = recent::parse(&std::fs::read_to_string(&self.recent_path).unwrap_or_default());
        (recent::order(items, key, &uses, now), now)
    }

    /// Record that `key` was selected at `now`.
    ///
    /// Best effort: the selection has already happened and been acted on, and
    /// failing it afterwards over a file that only decides an ordering would be
    /// reporting the wrong thing.
    pub fn remember(&self, key: &str, now: i64) {
        if !self.config.selector.recent {
            return;
        }
        let uses = recent::parse(&std::fs::read_to_string(&self.recent_path).unwrap_or_default());
        let uses = recent::bump(uses, key, now);
        if let Err(err) = write_recent(&self.recent_path, &recent::render(&uses)) {
            self.log
                .warn(&format!("could not record the selection: {err:#}"));
        }
    }

    /// Copy the standalone `kf` tool's list into place on first use.
    ///
    /// Runs only when the known-files list does not yet exist and a legacy `kf`
    /// config does; idempotent thereafter.
    pub fn ensure_files_migrated(&self) -> Result<()> {
        if self.files_path.exists() || !self.legacy_kf_path.exists() {
            return Ok(());
        }
        let lines = files::read_lines(&self.legacy_kf_path)?;
        let normalized = files::normalize_entries(&lines);
        if normalized.is_empty() {
            return Ok(());
        }
        files::write_lines(&self.files_path, &normalized)?;
        eprintln!(
            "migrated {} entries from {}",
            normalized.len(),
            self.legacy_kf_path.display()
        );
        Ok(())
    }
}
