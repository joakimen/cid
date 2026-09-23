//! `cid config` — generate, print and check the configuration file, and list
//! the actions it can bind.
//!
//! `print` and `check` answer different questions and are kept apart on
//! purpose. `print` is the configuration: every setting there is, what is in
//! force for it, and whether that came from the file or from cid. `check` is
//! a checklist: whether what the settings point at is actually there, and
//! whether the programs cid shells out to are installed. A row in `check`
//! repeats a value only where repeating it is the way out of a problem.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Bindings, Config, Labels};
use crate::path::{display_path, expand_home_dir};
use crate::{Ctx, binding, cmd, files, gh, history, repo, stats, term};

/// A commented starter config. Settings are grouped by the command that reads
/// them; users edit it to taste.
const TEMPLATE: &str = r#"# cid configuration. Settings are grouped by the command that reads them,
# with `[selector]` — shared by every selector — at the end.

# `cid repo`: where your repositories are, and how they are labelled.
[repo]

# Every repository lives under one root, laid out as <owner>/<repo> — the same
# shape as GitHub itself. `repo clone` writes here, so a clone always lands
# somewhere `repo sel` will find it.
root = "~/dev/github.com"

# Repositories outside the root, listed one at a time. An escape hatch for
# checkouts that predate the layout; `clone` never writes here.
# extra = ["~/bin"]

# Directory names to skip while searching.
ignore = ["node_modules", "target"]

# How repository paths are rendered: relative | tilde | full
# display = "relative"

# Labels name owners, one label to many owners, so everything you touch for work
# colours as one group in the selector however many orgs it spans. An owner with
# no label still shows up — just uncoloured.
#
# Written inline, on one line, so it stays an ordinary `[repo]` key: a
# `[repo.labels]` header would swallow every `[repo]` key written after it.
# labels = { personal = ["your-github-user"], work = ["acme", "acme-labs"] }

# `cid worktree`: where `worktree add` creates a tree.
[worktree]

# One directory per branch, with `/` written as `-`. A relative path is inside
# the repository the tree belongs to, and is added to that clone's
# .git/info/exclude the first time, so nothing else offers the tree twice. An
# absolute path holds the trees of every repository, under the repository's own
# name.
# root = ".worktrees"

# `cid note`: where your notes are, and what opens one.
[note]

# The directory holding them — an Obsidian vault, or any tree of Markdown
# files. Notes below it are listed most recently modified first, and what each
# one is called, which tags it carries and when it was created come from its
# YAML front matter. Without this, `cid note` has nowhere to look.
# root = "~/notes"

# Labels for the directories directly below the root, one label to many
# directories — the same idea as `[repo] labels`, and the same colours, so
# `work` reads the same in both. A directory with no label still shows up in
# the label column, under its own name and uncoloured.
#
# Written inline, on one line, for the reason `[repo] labels` is.
# labels = { work = ["projects", "clients"], personal = ["journal"] }

# Directories holding notes you keep but no longer read, as paths below the
# root. They are left out of every listing, out of the selector and out of the
# text search; `--all` puts them back, and naming a note outright opens it
# either way. A list, because a vault that files notes by kind archives them
# the same way.
# archives = ["work/archive", "personal/archive"]

# The one permanent note `note scratch` opens — somewhere to put a thought
# without deciding first whether it is worth a note of its own. A path below
# the root; its directory is created the first time.
# scratch = "scratch/scratch.md"

# Where `note new` starts a note, as a directory below the root; subdirectories
# are fine. Created the first time.
# inbox = "inbox"

# Where `note daily` keeps one note per day, named `YYYY-MM-DD.md`, as a
# directory below the root. `note cleanup` does not hold a name made only of a
# date against a note in it.
# daily = "daily"

# What `note open` launches, split on whitespace like $EDITOR. Its own setting
# because a note is as often read as written — `glow` and `nvim` are both
# answers. Unset, it is $VISUAL then $EDITOR, as `cid edit` uses.
# editor = "nvim"

# `cid history`: which shell history to search.
[history]

# fish's history file. The default is $XDG_DATA_HOME/fish/fish_history, falling
# back to ~/.local/share/fish/fish_history. Set this only if you have named your
# session — `set -U fish_history work` reads `work_history` instead — since fish
# does not export that variable for cid to find.
# file = "~/.local/share/fish/work_history"

# `cid init`: what the shell integration defines.
#
# Neither table holds shell code — each names an action cid defines, so the
# same configuration serves fish and any shell cid later learns to write for.
# Between them the two tables below name every action there is. `cid config
# actions` lists them all with whatever is bound to each, `cid config print`
# lists the keys and names you have, and `cid config check` says whether they
# resolve.
#
# cid binds nothing on its own. The two tables below are suggestions, and
# stay suggestions until you uncomment one — a key is the scarcest thing a
# terminal has, and which of yours a tool may take is yours to say.
#
# What you write is the whole of it, so leaving a key out is how it stays
# unbound. Keys are spelled as fish spells them.
# [shell.bindings]
# ctrl-o = "repo-cd"          # cd to a repository
# ctrl-t = "worktree-cd"      # cd to a worktree of this repository
# f1     = "repo-open"        # open a repository on GitHub
# f2     = "pr-open"          # open this branch's pull request, or the list
# f3     = "file-edit"        # open a tracked file in $EDITOR
# ctrl-g = "branch-checkout"  # check out a branch
# f7     = "pr-checkout"      # check out a pull request
# ctrl-f7 = "pr-worktree-cd"  # the same, in a worktree of its own, and cd there
# f10    = "note-edit"        # open a note from the vault
# ctrl-f10 = "note-daily"     # open today's note, starting it if need be
# ctrl-r = "history-select"   # search history onto the command line
# up     = "history-up"       # the same, on the first line of a prompt

# Names defined as shell functions, each passing its arguments through.
# [shell.aliases]
# fe = "edit"           # cid edit
# kl = "proc-kill"      # cid ps kill --force
# i  = "project-deps"   # cid project deps
# b  = "project-build"  # cid project build

# The built-in fuzzy selector, shared by every command that opens one.
[selector]
height = "50%"        # finder height, e.g. "50%" or "20"
# recent = true        # offer repositories and files you have chosen before first
# preview = true       # show a preview pane for the highlighted row
# preview_window = "right:50%" # preview layout: [up|down|left|right][:SIZE][:hidden]

# The `bat` theme every file preview is drawn with. Passed as `--theme`, so it
# wins over BAT_THEME and your own bat config — a preview pane is cid's to
# make legible, and a theme chosen for reading whole files in a pager is not
# always one. A theme bat does not know is not an error: it draws in its own
# default instead. Empty hands bat nothing and lets its config decide.
# preview_theme = "Catppuccin Mocha"
"#;

/// `cid config init` — write `config.toml` into the config directory,
/// refusing to clobber an existing config (either format) unless `force`.
pub fn init(ctx: &Ctx, force: bool) -> Result<()> {
    let dir = ctx
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let target = dir.join("config.toml");
    let legacy = dir.join("config.json");

    if !force {
        if target.exists() {
            bail!(
                "configuration already exists at {} (pass --force to overwrite)",
                target.display()
            );
        }
        if legacy.exists() {
            bail!(
                "a legacy JSON configuration already exists at {}; \
                 pass --force to write config.toml alongside it",
                legacy.display()
            );
        }
    }

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating config directory {}", dir.display()))?;
    std::fs::write(&target, TEMPLATE).with_context(|| format!("writing {}", target.display()))?;

    println!("Wrote starter configuration to {}", target.display());
    println!("Edit it, then run `cid config print` to verify.");
    Ok(())
}

// --- print ------------------------------------------------------------------

/// Cyan, for the `[table]` a group of settings is written under.
const HEADING: u8 = 6;

/// Red, for a value cid cannot act on.
const BROKEN: u8 = 1;

/// Shown where a setting has no value and nothing stands in for it.
const UNSET: &str = "(unset)";

/// Shown where a list is empty.
const EMPTY: &str = "(none)";

/// What the last column says about a value cid chose rather than the user.
const DEFAULT: &str = "default";

/// What is in force for one setting.
enum Value {
    /// A value, shown as it stands.
    Set(String),
    /// Nothing there, and the placeholder that says so.
    Missing(&'static str),
    /// Set to something cid cannot act on.
    Broken(String),
}

/// One line of `config print`.
enum Row {
    /// The gap between two groups.
    Blank,
    /// The `[table]` a group is written under, and what to know about the
    /// group as a whole.
    Heading { title: String, note: String },
    /// One setting: the key it is written under, what is in force, and where
    /// that came from.
    Setting {
        key: String,
        value: Value,
        note: String,
    },
}

impl Row {
    fn heading(title: impl Into<String>, note: impl Into<String>) -> Self {
        Self::Heading {
            title: title.into(),
            note: note.into(),
        }
    }

    fn setting(key: impl Into<String>, value: Value, note: impl Into<String>) -> Self {
        Self::Setting {
            key: key.into(),
            value,
            note: note.into(),
        }
    }
}

impl Value {
    /// The text as it is drawn, before it is painted.
    fn text(&self) -> &str {
        match self {
            Self::Set(text) | Self::Broken(text) => text.as_str(),
            Self::Missing(text) => text,
        }
    }

    /// The colour it is drawn in, or `None` for the terminal's own foreground —
    /// which is what a value the user set gets, so the settings are the only
    /// thing on the report at full contrast.
    fn color(&self) -> Option<u8> {
        match self {
            Self::Set(_) => None,
            Self::Missing(_) => Some(term::SECONDARY),
            Self::Broken(_) => Some(BROKEN),
        }
    }
}

/// A setting the file names, or what happens when it does not.
fn optional(key: &str, value: Option<&str>, without: &str) -> Row {
    match value {
        Some(value) => Row::setting(key, Value::Set(value.to_string()), ""),
        None => Row::setting(key, Value::Missing(UNSET), without),
    }
}

/// A setting that has a value whether the file gives it one or not, marked
/// where that value is the one cid ships.
fn defaulted(key: &str, value: Value, is_default: bool) -> Row {
    Row::setting(key, value, if is_default { DEFAULT } else { "" })
}

/// A list of names as one value.
fn list(items: &[String]) -> Value {
    if items.is_empty() {
        Value::Missing(EMPTY)
    } else {
        Value::Set(items.join(", "))
    }
}

/// One row per label, keyed as the file writes it — `labels.work` — so a set of
/// owners or directories stays one line each however many labels there are.
fn label_rows(labels: &Labels) -> Vec<Row> {
    if labels.is_empty() {
        return vec![Row::setting("labels", Value::Missing(EMPTY), "")];
    }
    labels
        .iter()
        .map(|(label, members)| Row::setting(format!("labels.{label}"), list(members), ""))
        .collect()
}

/// What a `[shell]` table's heading says. cid binds nothing on its own, so an
/// absent table is not a default in force — it is nothing bound, which the
/// report has to say outright or a user reads an empty list as a bug.
fn table_note(written: bool) -> &'static str {
    if written {
        ""
    } else {
        "not written — nothing is bound"
    }
}

/// One row per key or name, with the action it runs and what that action does.
///
/// An action nobody defines is shown and marked rather than left out: it is a
/// line the file really has, and a report that silently drops it is a user
/// hunting for a key that is written right there.
fn binding_rows(table: Option<&Bindings>) -> Vec<Row> {
    binding::entries(table)
        .into_iter()
        .map(|(trigger, id)| match binding::action(id) {
            Some(action) => Row::setting(trigger, Value::Set(id.to_string()), action.description),
            None => Row::setting(trigger, Value::Broken(id.to_string()), "no such action"),
        })
        .collect()
}

/// What `config print` reports that the config file does not hold: the paths
/// and the editor [`Ctx`] resolved from the environment. Passed in so building
/// the report stays pure.
struct Env<'a> {
    config_path: String,
    /// Without a file every value below is a default, which is worth saying
    /// once at the top rather than on every row.
    config_exists: bool,
    history_path: String,
    /// `$VISUAL`, then `$EDITOR`.
    editor: Option<&'a str>,
}

/// The whole report, in the order it is printed: every setting there is, under
/// the table it is written in, whether the file sets it or not.
fn report(cfg: &Config, env: &Env) -> Vec<Row> {
    let default = Config::default();
    let mut rows = vec![Row::heading(
        env.config_path.clone(),
        if env.config_exists {
            ""
        } else {
            "not found — every value below is a default"
        },
    )];

    rows.push(Row::Blank);
    rows.push(Row::heading("[repo]", ""));
    rows.push(optional(
        "root",
        cfg.repo.root.as_deref(),
        "`repo` and `repo clone` have nowhere to look",
    ));
    rows.push(Row::setting("extra", list(&cfg.repo.extra), ""));
    rows.push(defaulted(
        "ignore",
        list(&cfg.repo.ignore),
        cfg.repo.ignore == default.repo.ignore,
    ));
    rows.push(defaulted(
        "display",
        Value::Set(cfg.repo.display.as_str().to_string()),
        cfg.repo.display == default.repo.display,
    ));
    rows.extend(label_rows(&cfg.repo.labels));

    rows.push(Row::Blank);
    rows.push(Row::heading("[worktree]", ""));
    rows.push(defaulted(
        "root",
        Value::Set(cfg.worktree.root.clone()),
        cfg.worktree.root == default.worktree.root,
    ));

    rows.push(Row::Blank);
    rows.push(Row::heading("[note]", ""));
    rows.push(optional(
        "root",
        cfg.note.root.as_deref(),
        "`cid note` has nowhere to look",
    ));
    rows.extend(label_rows(&cfg.note.labels));
    rows.push(Row::setting("archives", list(&cfg.note.archives), ""));
    rows.push(match cfg.note.scratch.as_deref() {
        Some(scratch) => Row::setting("scratch", Value::Set(scratch.to_string()), ""),
        None => Row::setting(
            "scratch",
            Value::Set(crate::note::DEFAULT_SCRATCH.to_string()),
            DEFAULT,
        ),
    });
    for (key, set, default) in [
        ("inbox", &cfg.note.inbox, crate::note::DEFAULT_INBOX),
        ("daily", &cfg.note.daily, crate::note::DEFAULT_DAILY),
    ] {
        rows.push(match set.as_deref() {
            Some(dir) => Row::setting(key, Value::Set(dir.to_string()), ""),
            None => Row::setting(key, Value::Set(default.to_string()), DEFAULT),
        });
    }
    rows.push(match (cfg.note.editor.as_deref(), env.editor) {
        (Some(editor), _) => Row::setting("editor", Value::Set(editor.to_string()), ""),
        (None, Some(editor)) => Row::setting(
            "editor",
            Value::Set(editor.to_string()),
            "from $VISUAL / $EDITOR",
        ),
        (None, None) => Row::setting(
            "editor",
            Value::Missing(UNSET),
            "set `[note] editor`, $VISUAL or $EDITOR",
        ),
    });

    rows.push(Row::Blank);
    rows.push(Row::heading("[history]", ""));
    rows.push(defaulted(
        "file",
        Value::Set(env.history_path.clone()),
        cfg.history.file.is_none(),
    ));

    rows.push(Row::Blank);
    rows.push(Row::heading("[selector]", ""));
    let (sel, base) = (&cfg.selector, &default.selector);
    rows.push(defaulted(
        "height",
        Value::Set(sel.height.clone()),
        sel.height == base.height,
    ));
    rows.push(defaulted(
        "preview",
        Value::Set(sel.preview.to_string()),
        sel.preview == base.preview,
    ));
    rows.push(defaulted(
        "preview_window",
        Value::Set(sel.preview_window.clone()),
        sel.preview_window == base.preview_window,
    ));
    rows.push(defaulted(
        "preview_theme",
        if sel.preview_theme.is_empty() {
            Value::Missing("(bat's own)")
        } else {
            Value::Set(sel.preview_theme.clone())
        },
        sel.preview_theme == base.preview_theme,
    ));
    rows.push(defaulted(
        "recent",
        Value::Set(sel.recent.to_string()),
        sel.recent == base.recent,
    ));

    rows.push(Row::Blank);
    rows.push(Row::heading(
        "[shell.bindings]",
        table_note(cfg.shell.bindings.is_some()),
    ));
    rows.extend(binding_rows(cfg.shell.bindings.as_ref()));

    rows.push(Row::Blank);
    rows.push(Row::heading(
        "[shell.aliases]",
        table_note(cfg.shell.aliases.is_some()),
    ));
    rows.extend(binding_rows(cfg.shell.aliases.as_ref()));

    rows.push(Row::Blank);
    rows.push(Row::heading(
        "environment",
        "read by cid, not written in the file",
    ));
    rows.push(match env.editor {
        Some(editor) => Row::setting(
            "editor",
            Value::Set(editor.to_string()),
            "$VISUAL, then $EDITOR",
        ),
        None => Row::setting(
            "editor",
            Value::Missing(UNSET),
            "`cid edit` has nothing to open files with",
        ),
    });

    rows
}

/// Draw the report.
///
/// Keys share one column across the whole report, so every setting starts in
/// the same place; the notes beside them line up only within their own group,
/// since a path in one group would otherwise push the word `default` half a
/// terminal away from the value it describes.
fn render_report(rows: &[Row], color: bool) -> Vec<String> {
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Setting { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let note_columns = note_columns(rows);

    rows.iter()
        .zip(note_columns)
        .map(|(row, value_width)| match row {
            Row::Blank => String::new(),
            Row::Heading { title, note } => {
                let mut line = term::paint(title, HEADING, color);
                if !note.is_empty() {
                    line.push_str("  ");
                    line.push_str(&term::paint(note, term::SECONDARY, color));
                }
                line
            }
            Row::Setting { key, value, note } => {
                let mut line = format!("  {}  ", term::bold(&pad(key, key_width), color));
                let text = if note.is_empty() {
                    value.text().to_string()
                } else {
                    pad(value.text(), value_width)
                };
                line.push_str(&match value.color() {
                    Some(color_index) => term::paint(&text, color_index, color),
                    None => text,
                });
                if !note.is_empty() {
                    line.push_str("  ");
                    line.push_str(&term::paint(note, term::SECONDARY, color));
                }
                line
            }
        })
        .collect()
}

/// The width each row pads its value to, so the notes in one group line up
/// with each other and with nothing else. Rows without a note are not measured:
/// nothing lines up behind them.
fn note_columns(rows: &[Row]) -> Vec<usize> {
    let mut widths = vec![0; rows.len()];
    let mut start = 0;
    for end in 0..=rows.len() {
        if end != rows.len() && !matches!(rows[end], Row::Heading { .. }) {
            continue;
        }
        let group = rows[start..end]
            .iter()
            .filter_map(|row| match row {
                Row::Setting { value, note, .. } if !note.is_empty() => {
                    Some(value.text().chars().count())
                }
                _ => None,
            })
            .max()
            .unwrap_or(0);
        widths[start..end].fill(group);
        start = end;
    }
    widths
}

/// `text` in a field `width` characters wide, counted in characters rather
/// than bytes so a label with an accent in it does not shift the column.
fn pad(text: &str, width: usize) -> String {
    let mut padded = text.to_string();
    for _ in text.chars().count()..width {
        padded.push(' ');
    }
    padded
}

/// `cid config print` — every setting there is, and what is in force for it.
///
/// The settings only: whether the paths they name exist, and whether the tools
/// cid shells out to are installed, is `cid config check`.
pub fn print(ctx: &Ctx) -> Result<()> {
    ctx.log.info(&format!(
        "printing configuration from {}",
        ctx.config_path.display()
    ));

    let env = Env {
        config_path: display_path(&ctx.config_path.to_string_lossy(), ctx.home_str(), false),
        config_exists: ctx.config_path.exists(),
        history_path: display_path(&ctx.history_path.to_string_lossy(), ctx.home_str(), false),
        editor: ctx.editor_setting(),
    };

    let mut out = term::Listing::stdout();
    for line in render_report(&report(&ctx.config, &env), ctx.color()) {
        if !out.line(&line)? {
            return Ok(());
        }
    }
    out.finish()?;
    Ok(())
}

/// `cid config actions` — every action a key or a name can run, and what the
/// configuration binds to each.
pub fn actions(ctx: &Ctx) -> Result<()> {
    let mut out = term::Listing::stdout();
    let assignments = binding::assignments(&ctx.config.shell);
    for line in binding::render_assignments(&assignments, ctx.color()) {
        if !out.line(&line)? {
            return Ok(());
        }
    }
    out.finish()?;
    Ok(())
}

/// `cid config path` — print the resolved config file path.
pub fn path(ctx: &Ctx) -> Result<()> {
    println!("{}", ctx.config_path.display());
    Ok(())
}

// --- check ------------------------------------------------------------------

/// How one check came out. Only [`Status::Fail`] means something is actually
/// broken, which is what makes the command's exit status worth anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    /// A shape, not a colour alone, so a piped report says exactly as much.
    fn glyph(self) -> &'static str {
        match self {
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
        }
    }

    /// ANSI 256-colour index: green, yellow, red.
    fn color(self) -> u8 {
        match self {
            Self::Ok => 2,
            Self::Warn => 3,
            Self::Fail => 1,
        }
    }

    /// The colour the row's name takes, or `None` for the terminal's own
    /// foreground. Only a failure is coloured: a report where every name is
    /// green is one nobody reads for the one that is not.
    fn name_color(self) -> Option<u8> {
        match self {
            Self::Fail => Some(self.color()),
            _ => None,
        }
    }

    /// The colour the row's detail takes. What a working setup found is
    /// secondary text; what to do about a broken one carries the status colour.
    fn detail_color(self) -> u8 {
        match self {
            Self::Ok => term::SECONDARY,
            other => other.color(),
        }
    }
}

/// One thing that was looked at, and what was found.
#[derive(Debug, Clone)]
struct Check {
    /// What was looked at, e.g. `repo root`.
    name: &'static str,
    status: Status,
    /// What was found, and — when something is wrong — what to do about it.
    /// Empty where a working setup has nothing to add: the row is a tick, not
    /// a place to repeat what `config print` already says.
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }

    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, detail)
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail)
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail)
    }
}

/// `count` with the noun it counts, in the number the count calls for.
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A group of checks, named for what they are about.
struct Section {
    title: &'static str,
    checks: Vec<Check>,
}

/// Render one check as its row: glyph, name, and what was found. `width` is the
/// widest name in the report, counted in characters — a row with nothing to add
/// is not padded to it, since nothing lines up behind it.
fn render_check(check: &Check, width: usize, color: bool) -> String {
    let name = if check.detail.is_empty() {
        check.name.to_string()
    } else {
        pad(check.name, width)
    };
    let mut row = format!(
        "  {} {}",
        term::paint(check.status.glyph(), check.status.color(), color),
        term::style(&name, check.status.name_color(), true, color),
    );
    if !check.detail.is_empty() {
        row.push_str("  ");
        row.push_str(&term::paint(
            &check.detail,
            check.status.detail_color(),
            color,
        ));
    }
    row
}

/// What the report came to, and the colour it is painted in.
fn summary(checks: &[&Check]) -> (String, u8) {
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let (warned, failed) = (count(Status::Warn), count(Status::Fail));

    let mut parts = vec![format!("{} checks", checks.len())];
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if warned > 0 {
        parts.push(format!("{warned} to look at"));
    }
    if failed == 0 && warned == 0 {
        parts.push("all clear".to_string());
    }

    let status = if failed > 0 {
        Status::Fail
    } else if warned > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    (parts.join(" · "), status.color())
}

/// How many checks failed. Warnings are not failures: the command exits
/// non-zero only when something is genuinely broken, so it is worth putting in
/// a setup script.
fn failures(checks: &[Check]) -> usize {
    checks.iter().filter(|c| c.status == Status::Fail).count()
}

/// Resolve `program` against `path_env` the way a shell would: a name
/// containing a separator is a path already, anything else is looked for in
/// each `PATH` entry in order. Executability is deliberately not checked — the
/// spawn reports that better.
fn resolve_on_path(
    program: &str,
    path_env: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let direct = PathBuf::from(program);
        return exists(&direct).then_some(direct);
    }
    path_env?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| exists(candidate))
}

/// [`resolve_on_path`] against this process's `PATH` and the filesystem.
pub(crate) fn on_path(program: &str) -> Option<PathBuf> {
    resolve_on_path(program, std::env::var("PATH").ok().as_deref(), |p| {
        p.exists()
    })
}

/// The first line of `program --version`, for reporting which one is installed.
fn version_of(program: &str) -> Option<String> {
    let _child = stats::in_child();
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(str::trim).map(str::to_string)
}

/// The version out of a `--version` line — `git version 2.51.0` is `2.51.0`.
///
/// The first word that starts with a digit, since every tool here writes its
/// name before the version and its build details after. A `v` prefix is part of
/// the spelling, not of the version.
fn version_number(line: &str) -> Option<&str> {
    line.split_whitespace()
        .map(|word| word.strip_prefix('v').unwrap_or(word))
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
}

/// What a row says about an installed tool: its version, or that it is there
/// at all when it will not say.
fn installed(program: &str) -> String {
    version_of(program)
        .as_deref()
        .and_then(version_number)
        .unwrap_or("installed")
        .to_string()
}

/// Whether a tool cid shells out to is installed, and which version.
fn tool_check(name: &'static str, program: &str, required: bool, note: &str) -> Check {
    match on_path(program) {
        Some(_) => Check::ok(name, installed(program)),
        None if required => Check::fail(name, format!("not on PATH — {note}")),
        None => Check::warn(name, format!("not on PATH — {note}")),
    }
}

/// What the `gh` row says when `gh` is not installed at all.
const GH_NOTE: &str = "only `pr` and `repo clone`/`open` need it (https://cli.github.com)";

/// What the `gh` row says once it is installed: which version, and whether it
/// can act as anyone. An expired token fails `pr` exactly as completely as a
/// missing binary does, which is why one row covers both — but neither stops
/// the rest of cid, so it is a warning.
fn gh_state(version: &str, authenticated: bool) -> (Status, String) {
    if authenticated {
        (Status::Ok, format!("{version}, authenticated"))
    } else {
        (
            Status::Warn,
            format!("{version}, not authenticated — run `gh auth login`"),
        )
    }
}

/// `gh`: installed, and logged in. The login is asked of GitHub, which is the
/// slowest thing in the report and the reason this is not a [`tool_check`].
fn gh_check() -> Check {
    if on_path("gh").is_none() {
        return Check::warn("gh", format!("not on PATH — {GH_NOTE}"));
    }
    let (status, detail) = gh_state(&installed("gh"), gh::authenticated());
    Check::new("gh", status, detail)
}

/// The config file itself: whether there is one. Reaching this point means it
/// parsed, so the only question left is whether one exists — and absent is a
/// warning, since the known-files commands work without one. Where it is, is
/// the first line `config print` draws.
fn config_check(ctx: &Ctx) -> Check {
    if ctx.config_path.exists() {
        Check::ok("config file", "")
    } else {
        Check::warn(
            "config file",
            format!(
                "{} not found — run `cid config init`",
                ctx.config_path.display()
            ),
        )
    }
}

/// The search root, and the `extra` paths beside it.
fn root_checks(ctx: &Ctx) -> Vec<Check> {
    let mut checks = Vec::new();

    match &ctx.config.repo.root {
        None if ctx.config.repo.extra.is_empty() => {
            checks.push(Check::fail(
                "repo root",
                "not set — `repo`, `edit --tracked` and `repo clone` have nowhere to look",
            ));
        }
        None => checks.push(Check::warn(
            "repo root",
            "not set; only `extra` is searched",
        )),
        Some(root) => {
            let expanded = expand_home_dir(root, ctx.home());
            checks.push(if expanded.is_dir() {
                Check::ok("repo root", "")
            } else {
                Check::fail(
                    "repo root",
                    format!("{} is not a directory", expanded.display()),
                )
            });
        }
    }

    if !ctx.config.repo.extra.is_empty() {
        let total = ctx.config.repo.extra.len();
        let missing: Vec<&str> = ctx
            .config
            .repo
            .extra
            .iter()
            .filter(|p| !expand_home_dir(p, ctx.home()).is_dir())
            .map(String::as_str)
            .collect();
        checks.push(if missing.is_empty() {
            Check::ok("repo extra", plural(total, "path", "paths"))
        } else {
            // Discovery treats a missing search path as a hard error.
            Check::fail("repo extra", format!("missing: {}", missing.join(", ")))
        });
    }

    checks
}

/// How many repositories discovery actually finds. Skipped when a search path
/// has already been reported missing, so the report stays one line per thing
/// wrong.
fn discovery_check(ctx: &Ctx, paths_ok: bool) -> Option<Check> {
    if !paths_ok || (ctx.config.repo.root.is_none() && ctx.config.repo.extra.is_empty()) {
        return None;
    }
    Some(
        match repo::find_all_repos(&ctx.config, ctx.home(), &ctx.log) {
            Ok(repos) if repos.is_empty() => Check::warn(
                "repositories",
                "none found — check `root` and `ignore` in the config",
            ),
            Ok(repos) => Check::ok("repositories", format!("{} found", repos.len())),
            Err(err) => Check::fail("repositories", format!("{err:#}")),
        },
    )
}

/// Whether an editor command is a program this machine has. Shared by the
/// editor `cid edit` launches and the one `[note] editor` names.
fn editor_on_path(name: &'static str, setting: &str) -> Check {
    // The setting may carry arguments (`code -w`); only the program is looked
    // for, the same split the launch does.
    let program = setting.split_whitespace().next().unwrap_or(setting);
    match on_path(program) {
        Some(found) => Check::ok(name, found.display().to_string()),
        None => Check::fail(name, format!("`{program}` is not on PATH")),
    }
}

/// The editor `cid edit` will launch, and whether it is actually there.
fn editor_check(ctx: &Ctx) -> Check {
    match ctx.editor_setting() {
        Some(setting) => editor_on_path("editor", setting),
        None => Check::warn(
            "editor",
            "no $VISUAL or $EDITOR — `cid edit` has nothing to open files with",
        ),
    }
}

/// The editor `note open` will launch, when `[note] editor` names one of its
/// own. Unset, it is the editor the row above already reported, and a second
/// row saying so is a second row saying so.
fn note_editor_check(ctx: &Ctx) -> Option<Check> {
    ctx.config
        .note
        .editor
        .as_deref()
        .map(|setting| editor_on_path("note editor", setting))
}

/// The notes vault: whether it resolves, and how many notes are under it.
///
/// A warning rather than a failure when it is unset, since every other group
/// works without one.
fn note_vault_check(ctx: &Ctx) -> Check {
    if ctx.config.note.root.is_none() {
        return Check::warn(
            "note vault",
            "`[note] root` not set — `cid note` has nowhere to look",
        );
    }
    match cmd::note::vault_summary(ctx) {
        Err(err) => Check::fail("note vault", format!("{err:#}")),
        Ok(vault) if vault.listed + vault.archived == 0 => Check::warn(
            "note vault",
            format!("{} holds no Markdown files", vault.root.display()),
        ),
        Ok(vault) => Check::ok(
            "note vault",
            match vault.archived {
                0 => plural(vault.listed, "note", "notes"),
                archived => format!(
                    "{}, {archived} archived",
                    plural(vault.listed, "note", "notes")
                ),
            },
        ),
    }
}

/// The archive directories, when there are any: whether each names a directory
/// that is actually there.
///
/// A warning rather than a failure, since an entry that names nothing hides
/// nothing — which is the point of reporting it. A path is relative to the
/// vault, and one written as though it were absolute, or misspelled, is a
/// setting that silently does nothing at all.
fn note_archives_check(ctx: &Ctx) -> Option<Check> {
    if ctx.config.note.archives.is_empty() {
        return None;
    }
    Some(match cmd::note::missing_archives(ctx) {
        Err(err) => Check::fail("note archives", format!("{err:#}")),
        Ok(missing) if missing.is_empty() => Check::ok(
            "note archives",
            plural(ctx.config.note.archives.len(), "directory", "directories"),
        ),
        Ok(missing) => Check::warn(
            "note archives",
            format!("not below the vault: {}", missing.join(", ")),
        ),
    })
}

/// `[shell]`: whether every key and name resolves to an action that exists.
///
/// A failure here is `cid init fish` refusing to emit anything, which takes
/// the whole shell integration with it — so it is a failure rather than a
/// warning, and this is where it should be found rather than at the next new
/// shell. Which key runs what is `config print`; this counts them and says
/// whether they resolve.
fn shell_check(cfg: &Config) -> Check {
    let bindings = binding::entries(cfg.shell.bindings.as_ref());
    let aliases = binding::entries(cfg.shell.aliases.as_ref());

    let unknown: Vec<String> = bindings
        .iter()
        .chain(&aliases)
        .filter(|(_, id)| binding::action(id).is_none())
        .map(|(trigger, id)| format!("`{trigger}` names `{id}`"))
        .collect();

    if unknown.is_empty() {
        Check::ok(
            "shell integration",
            format!(
                "{}, {}",
                plural(bindings.len(), "key binding", "key bindings"),
                plural(aliases.len(), "alias", "aliases"),
            ),
        )
    } else {
        Check::fail(
            "shell integration",
            format!(
                "{} — no such action, and `cid init` refuses until there is",
                unknown.join(", ")
            ),
        )
    }
}

/// fish's history file: whether it is where cid looked, and how much is in
/// it.
fn history_check(ctx: &Ctx) -> Check {
    match std::fs::read(&ctx.history_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check::warn(
            "fish history",
            format!(
                "{} not found — set `[history] file` if yours is elsewhere",
                ctx.history_path.display()
            ),
        ),
        Err(e) => Check::fail(
            "fish history",
            format!("{}: {e}", ctx.history_path.display()),
        ),
        Ok(data) => {
            // The same list `history sel` offers, so the count is the one the
            // selector will show rather than one that counts key presses.
            let entries = history::recent_first(history::typed_only(history::parse(
                &String::from_utf8_lossy(&data),
            )));
            Check::ok(
                "fish history",
                plural(entries.len(), "unique command", "unique commands"),
            )
        }
    }
}

/// The known-files list, and how much of it still exists on disk.
fn files_check(ctx: &Ctx) -> Check {
    let lines = match files::read_lines(&ctx.files_path) {
        Ok(lines) => lines,
        Err(err) => return Check::fail("tracked files", format!("{err:#}")),
    };
    if lines.is_empty() {
        return Check::ok("tracked files", "none yet — add one with `cid file add`");
    }
    let missing = lines
        .iter()
        .filter(|line| !Path::new(&crate::path::expand_tilde(line, ctx.home_str())).exists())
        .count();
    if missing == 0 {
        Check::ok("tracked files", format!("{} tracked", lines.len()))
    } else {
        // Not a failure: `file ls --missing` is the thing to run next.
        Check::warn(
            "tracked files",
            format!(
                "{} tracked, {missing} no longer on disk — see `cid file ls --missing`",
                lines.len()
            ),
        )
    }
}

/// Everything `config check` looks at, in the order it is reported: what the
/// configuration points at, then the programs cid shells out to, then the
/// files it keeps.
fn collect(ctx: &Ctx) -> Vec<Section> {
    let mut settings = vec![config_check(ctx)];
    let paths = root_checks(ctx);
    let paths_ok = failures(&paths) == 0;
    settings.extend(paths);
    settings.extend(discovery_check(ctx, paths_ok));
    settings.push(shell_check(&ctx.config));

    let mut tools = vec![editor_check(ctx)];
    tools.extend(note_editor_check(ctx));
    tools.push(tool_check(
        "git",
        "git",
        true,
        "`branch` and `repo` cannot work without it",
    ));
    tools.push(gh_check());
    tools.push(tool_check(
        "bat",
        "bat",
        false,
        "previews fall back to `head`, without highlighting or a theme",
    ));
    tools.push(tool_check(
        "rg",
        "rg",
        false,
        "without it `note open` looks at names only \
         (https://github.com/BurntSushi/ripgrep)",
    ));
    // `kill` and `lsof` get no row: both ship in the same base system `ps`
    // does, and a report of three lines saying the same thing is one line.
    tools.push(tool_check(
        "ps",
        "ps",
        true,
        "`cid ps` reads the process table through it",
    ));
    // The one row `project` earns: everything else it runs is whatever the
    // project in front of it asks for, and a missing one of those is already
    // reported where it is skipped.
    tools.push(tool_check(
        "mise",
        "mise",
        false,
        "`project` runs a project's own tools through it (https://mise.jdx.dev)",
    ));
    tools.push(tool_check(
        "claude",
        "claude",
        false,
        "only `stats improve` needs it (https://claude.com/product/claude-code)",
    ));

    let mut content = vec![history_check(ctx), files_check(ctx), note_vault_check(ctx)];
    content.extend(note_archives_check(ctx));

    vec![
        Section {
            title: "configuration",
            checks: settings,
        },
        Section {
            title: "tools",
            checks: tools,
        },
        Section {
            title: "content",
            checks: content,
        },
    ]
}

/// `cid config check` — look at everything cid depends on in one go and say
/// what is wrong with it. The exit status is non-zero only when something is
/// genuinely broken, so it is worth putting in a setup script.
///
/// A checklist, not a report of the configuration: what each setting is set to
/// is `cid config print`, and a row here repeats it only where repeating it
/// is the way out of a problem.
pub fn check(ctx: &Ctx) -> Result<()> {
    let sections = collect(ctx);
    let color = ctx.color();
    let all: Vec<&Check> = sections.iter().flat_map(|s| &s.checks).collect();
    let width = all
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = term::Listing::stdout();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 && !out.line("")? {
            return Ok(());
        }
        if !out.line(&term::paint(section.title, HEADING, color))? {
            return Ok(());
        }
        for check in &section.checks {
            if !out.line(&render_check(check, width, color))? {
                return Ok(());
            }
        }
    }
    let (summary, tint) = summary(&all);
    if !out.line("")? || !out.line(&term::paint(&summary, tint, color))? {
        return Ok(());
    }
    out.finish()?;

    let failed = all.iter().filter(|c| c.status == Status::Fail).count();
    if failed > 0 {
        bail!("{failed} of {} checks failed", all.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RepoDisplay, load_config};

    #[test]
    fn the_starter_template_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, TEMPLATE).unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.repo.root.as_deref(), Some("~/dev/github.com"));
        assert_eq!(
            cfg.repo.ignore,
            vec!["node_modules".to_string(), "target".to_string()]
        );
        assert_eq!(cfg.repo.display, RepoDisplay::Relative);
        assert_eq!(cfg.selector.height, "50%");
    }

    /// A commented key is `# <name> = ...`; prose comments are left alone.
    /// The starter config is the only place the actions are written out for a
    /// reader, so one that never appears in it is one nobody can discover.
    #[test]
    fn the_template_names_every_action_there_is() {
        for action in binding::ACTIONS {
            assert!(
                TEMPLATE.contains(&format!("\"{}\"", action.id)),
                "`{}` is in no table a user can read",
                action.id
            );
        }
    }

    /// The template writes the default bindings and aliases out so they can be
    /// uncommented and edited. Nothing but this notices when the two drift
    /// apart, and a stale copy is a user who uncomments it and silently loses
    /// whatever cid added since.
    #[test]
    fn the_commented_shell_tables_are_the_defaults_written_out() {
        let start = TEMPLATE
            .find("# [shell.bindings]")
            .expect("no [shell] tables");
        let end = TEMPLATE[start..]
            .find("# The built-in fuzzy selector")
            .expect("no [selector] section after them")
            + start;
        let block: String = TEMPLATE[start..end]
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| line.starts_with('[') || line.contains(" = "))
            .collect::<Vec<_>>()
            .join("\n");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &block).unwrap();
        let cfg = load_config(&path).unwrap_or_else(|e| panic!("{e:#}\n\n{block}"));

        let written = |table: &Option<crate::config::Bindings>| -> Vec<(String, String)> {
            table
                .as_ref()
                .expect("table missing from the template")
                .iter()
                .map(|(key, id)| (key.clone(), id.clone()))
                .collect()
        };
        let defaults = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(key, id)| ((*key).to_string(), (*id).to_string()))
                .collect()
        };

        assert_eq!(
            written(&cfg.shell.bindings),
            defaults(binding::EXAMPLE_BINDINGS)
        );
        assert_eq!(
            written(&cfg.shell.aliases),
            defaults(binding::EXAMPLE_ALIASES)
        );
    }

    #[test]
    fn the_templates_commented_keys_parse_when_uncommented() {
        let uncommented: String = TEMPLATE
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest) if template_key(rest).is_some() => rest,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(uncommented.contains("labels = {"), "{uncommented}");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, &uncommented).unwrap();

        let cfg = load_config(&path)
            .unwrap_or_else(|e| panic!("uncommented template rejected: {e:#}\n\n{uncommented}"));
        assert_eq!(cfg.repo.extra, vec!["~/bin".to_string()]);
        assert_eq!(cfg.repo.display, RepoDisplay::Relative);
        assert_eq!(cfg.repo.label_of("acme"), Some("work"));
        assert!(!cfg.selector.preview_window.is_empty());
    }

    // --- print ---

    /// The key `line` sets, if it sets one: `root = "~/dev"` is `root`.
    fn template_key(line: &str) -> Option<&str> {
        line.split_once(" = ").map(|(name, _)| name).filter(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        })
    }

    fn env() -> Env<'static> {
        Env {
            config_path: "~/.config/cid/config.toml".to_string(),
            config_exists: true,
            history_path: "~/.local/share/fish/fish_history".to_string(),
            editor: Some("nvim"),
        }
    }

    fn keys(rows: &[Row]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Setting { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The starter config describes every setting there is, so it is the list
    /// `config print` has to cover. A setting added to one and not the other
    /// is a configuration file that cannot be read back.
    #[test]
    fn every_setting_the_starter_config_names_is_printed() {
        // The `[shell]` tables are written out, since the report lists what is
        // bound rather than what could be — and nothing is bound by default.
        let cfg = Config {
            shell: crate::config::ShellConfig {
                bindings: Some(pairs(binding::EXAMPLE_BINDINGS)),
                aliases: Some(pairs(binding::EXAMPLE_ALIASES)),
            },
            ..Config::default()
        };
        let rows = report(&cfg, &env());
        let printed = keys(&rows);

        for line in TEMPLATE.lines() {
            let Some(key) = template_key(line.strip_prefix("# ").unwrap_or(line)) else {
                continue;
            };
            assert!(
                printed
                    .iter()
                    .any(|shown| *shown == key || shown.starts_with(&format!("{key}."))),
                "`{key}` is in the starter config but not in `config print`: {printed:?}"
            );
        }
    }

    /// The keys a shell binds are configuration like any other setting, and
    /// the report is where you look to find out which ones you have.
    #[test]
    fn the_key_bindings_and_aliases_that_are_written_are_printed() {
        let cfg = Config {
            shell: crate::config::ShellConfig {
                bindings: Some(pairs(binding::EXAMPLE_BINDINGS)),
                aliases: Some(pairs(binding::EXAMPLE_ALIASES)),
            },
            ..Config::default()
        };
        let rows = report(&cfg, &env());
        let printed = keys(&rows);

        for (trigger, _) in binding::EXAMPLE_BINDINGS
            .iter()
            .chain(binding::EXAMPLE_ALIASES)
        {
            assert!(
                printed.contains(trigger),
                "`{trigger}` is not in {printed:?}"
            );
        }
        let lines = render_report(&rows, false).join("\n");
        assert!(
            lines.contains("Select a repository and cd into it"),
            "a binding was printed without saying what it does:\n{lines}"
        );
    }

    fn pairs(entries: &[(&str, &str)]) -> crate::config::Bindings {
        entries
            .iter()
            .map(|(trigger, id)| ((*trigger).to_string(), (*id).to_string()))
            .collect()
    }

    /// An unwritten table is nothing bound, not a set of defaults in force —
    /// and the report says so rather than printing an empty list.
    #[test]
    fn an_unwritten_binding_table_says_that_nothing_is_bound() {
        let rows = report(&Config::default(), &env());
        let lines = render_report(&rows, false).join("\n");

        assert!(lines.contains("nothing is bound"), "{lines}");
        assert!(
            !keys(&rows).contains(&"ctrl-o"),
            "a binding was in force that nobody wrote"
        );
    }

    /// A binding naming an action cid does not define stops `cid init`
    /// outright. The report still shows the line, since it is one the file
    /// really has, and says what is wrong with it.
    #[test]
    fn a_binding_that_names_nothing_is_shown_and_marked() {
        let rows = binding_rows(Some(
            &[("f6".to_string(), "repo-jump".to_string())]
                .into_iter()
                .collect(),
        ));

        let line = &render_report(&rows, false)[0];
        assert!(line.contains("f6"), "{line}");
        assert!(line.contains("repo-jump"), "{line}");
        assert!(line.contains("no such action"), "{line}");
    }

    /// Which values are cid's own rather than the user's is most of what the
    /// report is for: a setting nobody chose reads differently from one
    /// somebody did.
    #[test]
    fn a_value_cid_chose_is_marked_and_one_the_user_chose_is_not() {
        let mut cfg = Config::default();
        cfg.selector.height = "20".to_string();
        let lines = render_report(&report(&cfg, &env()), false);

        let row = |key: &str| {
            lines
                .iter()
                .find(|line| line.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("no `{key}` row in {lines:#?}"))
                .clone()
        };
        assert!(!row("height").contains(DEFAULT), "{}", row("height"));
        assert!(row("recent").contains(DEFAULT), "{}", row("recent"));
    }

    #[test]
    fn a_missing_config_file_says_so_once_rather_than_on_every_row() {
        let lines = render_report(
            &report(
                &Config::default(),
                &Env {
                    config_exists: false,
                    ..env()
                },
            ),
            false,
        );

        assert!(lines[0].contains("not found"), "{:?}", lines[0]);
        assert_eq!(
            lines.iter().filter(|l| l.contains("not found")).count(),
            1,
            "{lines:#?}"
        );
    }

    #[test]
    fn keys_line_up_in_one_column_and_carry_no_colour_when_it_is_off() {
        let lines = render_report(&report(&Config::default(), &env()), false);
        let settings: Vec<&String> = lines.iter().filter(|line| line.starts_with("  ")).collect();

        // The value starts after the first gap wide enough to be a column break.
        let value_column = |line: &str| {
            let rest = &line[2..];
            let gap = rest.find("  ")?;
            let padded = &rest[gap..];
            Some(2 + gap + padded.len() - padded.trim_start().len())
        };
        let first = value_column(settings[0]);
        assert!(first.is_some(), "{:?}", settings[0]);
        for line in &settings {
            assert_eq!(value_column(line), first, "{line}");
            assert!(!line.contains('\x1b'), "colour leaked into a plain report");
        }
    }

    // --- check ---

    #[test]
    fn warnings_do_not_fail_the_run() {
        let checks = vec![
            Check::ok("git", "git version 2.51.0"),
            Check::warn("gh", "not on PATH"),
        ];
        assert_eq!(failures(&checks), 0);

        let broken = vec![Check::fail("repo root", "not set"), checks[1].clone()];
        assert_eq!(failures(&broken), 1);
    }

    #[test]
    fn every_status_is_a_distinct_shape_not_a_colour() {
        let glyphs: Vec<&str> = [Status::Ok, Status::Warn, Status::Fail]
            .iter()
            .map(|s| s.glyph())
            .collect();
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), glyphs.len(), "two statuses look alike");

        let plain = render_check(&Check::fail("repo root", "not set"), 9, false);
        assert_eq!(plain, "  ✗ repo root  not set");
        assert!(!plain.contains('\x1b'), "colour leaked into a plain report");
    }

    #[test]
    fn names_are_padded_into_one_column() {
        let rows = [
            render_check(&Check::ok("gh", "found"), 9, false),
            render_check(&Check::ok("repo root", "/tmp"), 9, false),
        ];
        let column = |row: &str| row.rfind("  ").map(|i| i + 2);
        assert_eq!(column(&rows[0]), column(&rows[1]), "{rows:?}");
    }

    /// A row with nothing to add is the tick alone: `config print` is where
    /// the value it would have repeated is written.
    #[test]
    fn a_check_with_nothing_to_add_is_the_tick_and_the_name() {
        let row = render_check(&Check::ok("config file", ""), 9, false);
        assert_eq!(row, "  ✓ config file");
    }

    #[test]
    fn the_summary_counts_what_is_left_to_do() {
        let clear = [Check::ok("git", ""), Check::ok("gh", "")];
        let (line, tint) = summary(&clear.iter().collect::<Vec<_>>());
        assert_eq!(line, "2 checks · all clear");
        assert_eq!(tint, Status::Ok.color());

        let mixed = [
            Check::ok("git", ""),
            Check::warn("gh", "not on PATH"),
            Check::fail("repo root", "not set"),
        ];
        let (line, tint) = summary(&mixed.iter().collect::<Vec<_>>());
        assert_eq!(line, "3 checks · 1 failed · 1 to look at");
        assert_eq!(tint, Status::Fail.color());
    }

    /// The row names the key that is wrong, since the report is where the
    /// user finds out before `cid init` does.
    #[test]
    fn a_binding_that_names_nothing_fails_the_shell_row() {
        let mut broken_config = Config::default();
        broken_config.shell.bindings = Some(
            [("f6".to_string(), "repo-jump".to_string())]
                .into_iter()
                .collect(),
        );

        let mut sound_config = Config::default();
        sound_config.shell.bindings = Some(pairs(binding::EXAMPLE_BINDINGS));

        let sound = shell_check(&sound_config);
        assert_eq!(sound.status, Status::Ok);
        let bound = format!("{} key bindings", binding::EXAMPLE_BINDINGS.len());
        assert!(sound.detail.contains(&bound), "{}", sound.detail);

        // Nothing written is nothing bound, and that is a sound state.
        let empty = shell_check(&Config::default());
        assert_eq!(empty.status, Status::Ok);
        assert!(empty.detail.contains("0 key bindings"), "{}", empty.detail);

        let broken = shell_check(&broken_config);
        assert_eq!(broken.status, Status::Fail);
        assert!(broken.detail.contains("f6"), "{}", broken.detail);
        assert!(broken.detail.contains("repo-jump"), "{}", broken.detail);
    }

    #[test]
    fn an_unauthenticated_gh_is_a_warning_that_names_the_way_out() {
        let (status, detail) = gh_state("2.97.0", false);
        assert_eq!(status, Status::Warn, "a login nobody has is not fatal");
        assert!(detail.contains("gh auth login"), "{detail}");
        assert!(
            detail.contains("2.97.0"),
            "the version went missing: {detail}"
        );

        let (status, detail) = gh_state("2.97.0", true);
        assert_eq!(status, Status::Ok);
        assert!(!detail.contains("gh auth login"), "{detail}");
    }

    /// Every tool here writes its version differently, and a checklist that
    /// repeats each one's whole banner is a checklist nobody scans.
    #[test]
    fn a_version_line_is_reported_as_the_version() {
        assert_eq!(version_number("git version 2.51.0"), Some("2.51.0"));
        assert_eq!(version_number("bat 0.26.1"), Some("0.26.1"));
        assert_eq!(
            version_number("gh version 2.97.0 (2026-07-31)"),
            Some("2.97.0")
        );
        assert_eq!(
            version_number("2026.8.3 macos-arm64 (2026-08-07)"),
            Some("2026.8.3")
        );
        assert_eq!(version_number("ripgrep v15.2.0"), Some("15.2.0"));
        assert_eq!(version_number("usage: ps [-AaCcEefhjlMmrSTvwXx]"), None);
    }

    #[test]
    fn path_lookup_takes_the_first_match_in_order() {
        let found = resolve_on_path("gh", Some("/a:/b:/c"), |p| {
            p == Path::new("/b/gh") || p == Path::new("/c/gh")
        });
        assert_eq!(found, Some(PathBuf::from("/b/gh")));
    }

    #[test]
    fn path_lookup_reports_nothing_when_it_is_not_there() {
        assert_eq!(resolve_on_path("gh", Some("/a:/b"), |_| false), None);
        // No PATH at all is not a panic; it is simply nothing found.
        assert_eq!(resolve_on_path("gh", None, |_| true), None);
        // Empty entries are skipped rather than turned into a bare filename.
        assert_eq!(
            resolve_on_path("gh", Some("::"), |p| p == Path::new("gh")),
            None
        );
    }

    #[test]
    fn a_program_with_a_separator_is_used_as_it_stands() {
        assert_eq!(
            resolve_on_path("/opt/bin/hx", Some("/usr/bin"), |p| p
                == Path::new("/opt/bin/hx")),
            Some(PathBuf::from("/opt/bin/hx"))
        );
        assert_eq!(
            resolve_on_path("/opt/bin/hx", Some("/usr/bin"), |_| false),
            None
        );
    }
}
