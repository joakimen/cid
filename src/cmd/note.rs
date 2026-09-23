//! `cid note` — list, select and open the notes in your vault.
//!
//! A registry like `repo` and `file`: the set is every Markdown file under
//! `[note] root`, and the verbs act on what is selected from it. The imperative
//! half lives here — the walk, the file reads, the clock and the editor;
//! [`crate::note`] decides everything else.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::note::{self, Note, Widths};
use crate::path::expand_home_dir;
use crate::select::{Preview, SelectItem};
use crate::{Ctx, cmd, prompt, select, stats, term};

/// How much of a note is read to find its front matter.
///
/// The block sits at the very top of the file, so this is all a listing needs
/// and a vault is spared having every byte of every note read to build one. A
/// block longer than this is not front matter anybody wrote by hand, and the
/// note is listed by its filename as though it had none.
const HEAD_BYTES: u64 = 8 * 1024;

/// The vault, expanded — the one directory `note` looks in.
fn vault(ctx: &Ctx) -> Result<PathBuf> {
    let root = ctx.config.note.root.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`[note] root` is not set in {} — `cid note` has nowhere to look",
            ctx.config_path.display()
        )
    })?;
    let path = expand_home_dir(root, ctx.home());
    if !path.is_dir() {
        bail!(
            "`[note] root` is {}, which is not a directory",
            path.display()
        );
    }
    Ok(path)
}

/// Every note in the vault, most recently modified first.
///
/// The walk and the head reads run on the walker's own threads, so a vault is
/// one pass rather than a walk followed by a read per note. Dotfiles are
/// skipped, which is what leaves Obsidian's `.obsidian` and `.trash` out
/// without naming either.
///
/// `[note] archives` is pruned rather than filtered afterwards, unless `all`
/// asks for it: an archive is the part of a vault that only grows, and not
/// descending into it is the difference between reading every note ever
/// written and reading the ones still in use.
fn load(ctx: &Ctx, all: bool) -> Result<Vec<Note>> {
    let root = vault(ctx)?;
    let offset = ctx.utc_offset();
    let found = Mutex::new(Vec::new());
    let archives = archives(ctx, all);

    ignore::WalkBuilder::new(&root)
        .hidden(true)
        .require_git(false)
        .add_custom_ignore_filename(".fdignore")
        .filter_entry({
            let root = root.clone();
            move |entry| !note::is_archived(&relative(entry.path(), &root), &archives)
        })
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                if let Ok(entry) = entry
                    && entry.file_type().is_some_and(|ft| ft.is_file())
                    && note::is_note(entry.path())
                    && let Some(found_note) = read_note(entry.path(), &root, offset)
                {
                    found
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(found_note);
                }
                ignore::WalkState::Continue
            })
        });

    let mut notes = found.into_inner().unwrap_or_else(|e| e.into_inner());
    note::newest_first(&mut notes);
    ctx.log
        .info(&format!("{} note(s) under {}", notes.len(), root.display()));
    if notes.is_empty() {
        bail!(
            "no notes under {}{} — `cid note` reads Markdown files, and \
             `cid note new` writes one",
            root.display(),
            if all {
                ""
            } else {
                " outside `[note] archives`"
            }
        );
    }
    Ok(notes)
}

/// The archive directories a listing hides, or none when `--all` was passed.
fn archives(ctx: &Ctx, all: bool) -> Vec<String> {
    match all {
        true => Vec::new(),
        false => ctx.config.note.archives.clone(),
    }
}

/// `path` as the vault knows it: below `root`, or as it stands when it is not
/// under one.
fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// One note, read: its times from the directory entry's metadata, its front
/// matter and its checkboxes from the first [`HEAD_BYTES`].
///
/// `None` for a file whose metadata cannot be read, which is one note missing
/// from a listing rather than the listing failing — the same way the walk
/// treats a directory it may not enter.
fn read_note(path: &Path, root: &Path, offset: time::UtcOffset) -> Option<Note> {
    let meta = path.metadata().ok()?;
    let modified = unix(meta.modified().ok()?)?;
    let birth = meta.created().ok().and_then(unix);

    let head = read_head(path, HEAD_BYTES).unwrap_or_default();
    let (block, _) = note::split_front_matter(&head);
    let front = block.map_or_else(note::Front::default, |block| {
        note::parse_front(block, offset)
    });

    let rel = relative(path, root);
    Some(Note {
        dir: note::top_dir(&rel).to_string(),
        rel,
        path: path.to_path_buf(),
        modified,
        created: note::created(front.created, birth, modified),
        front,
    })
}

/// The first `limit` bytes of `path`, as text. A note is whatever the user's
/// editor wrote, so invalid UTF-8 — and a multi-byte character the limit cut in
/// half — is replaced rather than refused.
fn read_head(path: &Path, limit: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    std::io::BufReader::new(file)
        .take(limit)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn unix(time: SystemTime) -> Option<i64> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => Some(since.as_secs() as i64),
        // Before 1970. A file dated that way is a clock that was wrong, not a
        // note to leave out of the list.
        Err(before) => Some(-(before.duration().as_secs() as i64)),
    }
}

/// `cid note ls` — print the notes, most recently modified first.
///
/// Plain, one path below the vault per line, which is the name `note open`
/// takes. `--status` adds the tags and both dates; `--all` adds the notes
/// under `[note] archives`.
pub fn ls(ctx: &Ctx, status: bool, all: bool) -> Result<()> {
    let notes = load(ctx, all)?;
    let cfg = &ctx.config.note;
    let widths = Widths::of(&notes, cfg, ctx.home_str());
    let offset = ctx.utc_offset();

    let mut out = term::Listing::stdout();
    for note in &notes {
        let row = match status {
            // Absolute and uncollapsed: the plain listing is what a pipe
            // reads, and `~` is a thing only a shell expands.
            false => note.path.to_string_lossy().into_owned(),
            true => note::status_row(note, cfg, &widths, offset, ctx.home_str()),
        };
        if !out.line(&row)? {
            break;
        }
    }
    out.finish()?;
    Ok(())
}

/// `cid note sel` — fuzzy-select a note and print its absolute path.
pub fn sel(ctx: &Ctx, all: bool) -> Result<()> {
    let notes = load(ctx, all)?;
    let (cfg, offset) = (&ctx.config.note, ctx.utc_offset());
    let width = note::group_width(&notes, cfg);
    let rows = notes
        .iter()
        .map(|note| item(note, cfg, width, offset))
        .collect();
    let choice = select::select_one(rows, "Select a note", &ctx.config.selector)?;
    println!("{choice}");
    Ok(())
}

/// `cid note open [NAME]...` — open notes in `[note] editor`, selecting them
/// when none are named.
///
/// The selector is one list read three ways, each on a key: the names of the
/// notes, and — through ripgrep — every line of every note, fuzzily or exactly.
/// A note is as often looked for by something it says as by what it is called,
/// and which of the two it is only becomes clear once the looking has started.
pub fn open(ctx: &Ctx, names: &[String], all: bool) -> Result<()> {
    let editor = ctx.note_editor()?;
    let root = vault(ctx)?;

    if !names.is_empty() {
        let targets: Vec<String> = names
            .iter()
            .map(|name| resolve(&root, ctx.home(), name))
            .collect();
        // The editor would open a new, empty buffer at a mistyped path and say
        // nothing about it, which is how a typo becomes a second note.
        for (name, target) in names.iter().zip(&targets) {
            if !Path::new(target).exists() {
                eprintln!("warning: no note called {name} — the editor will open it as a new file");
            }
        }
        return cmd::edit::launch(ctx, &editor, &targets);
    }

    let notes = load(ctx, all)?;
    let chosen = match select::select_many_searching(
        searcher(
            root.clone(),
            notes,
            ctx.config.note.clone(),
            ctx.utc_offset(),
            archives(ctx, all),
        ),
        "Open a note",
        modes(which("rg").is_some()),
        &ctx.config.selector,
    ) {
        Ok(chosen) => chosen,
        Err(e) if e.is::<select::Cancelled>() => return Ok(()),
        Err(e) => return Err(e),
    };
    if chosen.is_empty() {
        return Ok(());
    }

    // A mode key empties the list, so what came back is all of one kind: paths
    // from the names, or places inside notes from a search.
    let matches: Vec<note::Match> = chosen
        .iter()
        .filter_map(|value| note::decode_match(value, &root))
        .collect();
    if matches.is_empty() {
        return cmd::edit::launch(ctx, &editor, &chosen);
    }
    open_matches(ctx, &editor, &matches)
}

/// A note named on the command line, as a path. A name is relative to the
/// vault, so `cid note open` takes back exactly what `cid note ls` printed;
/// an absolute path, or one that begins with `~`, is left where it points.
///
/// `[note] archives` does not apply: naming a note is asking for that note.
fn resolve(root: &Path, home: &Path, name: &str) -> String {
    let path = expand_home_dir(name, home);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    root.join(path).to_string_lossy().into_owned()
}

/// One selector row: where a note is filed and the day it was created, both
/// unsearchable, then what it calls itself — coloured by its label where its
/// directory carries one.
///
/// `width` is the group column's, over the whole vault rather than over the
/// rows being built: name mode rebuilds these on every keystroke.
///
/// The pane is built when the row is highlighted rather than now — see
/// [`Preview::File`]. A vault read up front is one file read per note for panes
/// the user scrolls past.
fn item(
    note: &Note,
    cfg: &crate::config::NoteConfig,
    width: usize,
    offset: time::UtcOffset,
) -> SelectItem {
    let (prefix, tints) = note::prefix(note, cfg, width, offset);
    let item = SelectItem::new(note::row(note), note.path.to_string_lossy().into_owned())
        .prefix(prefix, tints)
        .preview(Preview::File);
    match note::row_color(note, cfg) {
        Some(color) => item.color(color),
        None => item,
    }
}

/// `cid note new [TITLE]` — start a note called `TITLE` and open it.
///
/// With no title, one is asked for, with the filename it will produce drawn
/// beneath it as it is typed. The note is `YYYY-MM-DD-<title-in-kebab-case>.md`
/// at the top of the vault, and starts as the title for its H1 — which leaves
/// a note abandoned there a heading and nothing else, what `note cleanup`
/// offers as empty.
pub fn new(ctx: &Ctx, title: Option<&str>) -> Result<()> {
    let editor = ctx.note_editor()?;
    let root = vault(ctx)?;
    let (now, offset) = (crate::unix_now(), ctx.utc_offset());
    let file_for = |title: &str| {
        note::free_name(&note::titled_name(title, now, offset), |candidate| {
            root.join(candidate).exists()
        })
    };

    let title =
        match title.map(str::trim) {
            Some("") => bail!("a note's title cannot be blank"),
            Some(title) => title.to_string(),
            None => prompt::ask("Title", "File", file_for, ctx.color()).map_err(|e| {
                match e.is::<select::Cancelled>() {
                    true => e,
                    false => e.context("no TITLE given and no terminal to ask for one"),
                }
            })?,
        };
    let path = root.join(file_for(&title));

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, note::heading(&title).as_bytes()))
        .with_context(|| format!("creating {}", path.display()))?;

    let target = path.to_string_lossy().into_owned();
    ctx.log.info(&format!("new note at {target}"));
    cmd::edit::launch(ctx, &editor, std::slice::from_ref(&target))
}

/// `cid note scratch` — open the one note that is filed nowhere.
///
/// A single permanent file rather than a new one each time, which is the whole
/// point: somewhere to put a thought without deciding first whether it is worth
/// a note, and somewhere to find it again afterwards. `[note] scratch` says
/// where it lives, `scratch/scratch.md` by default.
pub fn scratch(ctx: &Ctx) -> Result<()> {
    let editor = ctx.note_editor()?;
    let root = vault(ctx)?;
    let path = scratch_path(ctx, &root);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    ctx.log.info(&format!("scratch note at {}", path.display()));
    cmd::edit::launch(ctx, &editor, &[path.to_string_lossy().into_owned()])
}

/// Where the scratch note lives. One place, because two commands need to agree
/// about it: the one that opens it and the one that must never offer to delete
/// it.
fn scratch_path(ctx: &Ctx, root: &Path) -> PathBuf {
    root.join(
        ctx.config
            .note
            .scratch
            .as_deref()
            .unwrap_or(note::DEFAULT_SCRATCH),
    )
}

// --- cleanup ----------------------------------------------------------------

/// `cid note cleanup` — look through the notes that were never really
/// written, and delete the ones you agree about.
///
/// Nothing is deleted without being listed and then agreed to, and the listing
/// says why each note is on it: three rules, applied without judgement, over a
/// vault only its owner can actually read.
pub fn cleanup(ctx: &Ctx, yes: bool, all: bool) -> Result<()> {
    let notes = load(ctx, all)?;
    let candidates = junk(ctx, &notes)?;

    if candidates.is_empty() {
        println!(
            "Nothing to clean up — all {} notes have a name and something in them",
            notes.len()
        );
        return Ok(());
    }
    ctx.log.info(&format!(
        "{} of {} notes are candidates",
        candidates.len(),
        notes.len()
    ));

    let chosen = match select::select_many(
        junk_items(ctx, &candidates),
        "Notes to delete (tab to select several)",
        &ctx.config.selector,
    ) {
        Ok(chosen) => chosen,
        Err(e) if e.is::<select::Cancelled>() => return Ok(()),
        Err(e) => return Err(e),
    };
    if chosen.is_empty() {
        println!("Nothing selected");
        return Ok(());
    }

    // Printed before the question is put: "delete 4 notes?" is answerable only
    // by someone who has already seen the four.
    let total: u64 = candidates
        .iter()
        .filter(|(note, _, _)| chosen.contains(&note.path.to_string_lossy().into_owned()))
        .map(|(_, _, bytes)| bytes)
        .sum();
    let mut out = term::Listing::stdout();
    for path in &chosen {
        if !out.line(&term::paint(path, note::Junk::Empty.color(), ctx.color()))? {
            return Ok(());
        }
    }
    out.finish()?;

    match term::Confirm::resolve(yes) {
        term::Confirm::Assumed => {}
        term::Confirm::Ask => {
            let question = format!(
                "Delete {} {} ({})? This cannot be undone",
                chosen.len(),
                if chosen.len() == 1 { "note" } else { "notes" },
                note::size(total),
            );
            if !term::confirm(&question)? {
                println!("Nothing deleted");
                return Ok(());
            }
        }
        term::Confirm::Impossible => bail!(
            "no terminal to ask for confirmation on — pass `--yes` to delete without being asked"
        ),
    }

    let mut failed = 0;
    for path in &chosen {
        match std::fs::remove_file(path) {
            Ok(()) => println!("Deleted {path}"),
            Err(err) => {
                failed += 1;
                eprintln!("error: {path}: {err}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} could not be deleted", chosen.len());
    }
    Ok(())
}

/// Every note worth offering for deletion, with the reason and its size.
///
/// The whole file is read rather than the head bound the listing uses: "empty"
/// is a claim about all of it, and a note that only looks empty for the first
/// eight kilobytes is not one.
fn junk(ctx: &Ctx, notes: &[Note]) -> Result<Vec<(Note, note::Junk, u64)>> {
    let scratch = scratch_path(ctx, &vault(ctx)?);
    let mut candidates: Vec<(Note, note::Junk, u64)> = notes
        .iter()
        .filter_map(|note| {
            let bytes = note.path.metadata().map(|m| m.len()).unwrap_or(0);
            let text = read_head(&note.path, CLEANUP_BYTES).unwrap_or_default();
            let (_, body) = note::split_front_matter(&text);
            note::junk(note, body, &scratch).map(|reason| (note.clone(), reason, bytes))
        })
        .collect();
    note::cleanup_order(&mut candidates);
    Ok(candidates)
}

/// How much of a note is read to decide whether there is anything in it. Well
/// past [`note::MIN_BODY`], and past any note this could call empty.
const CLEANUP_BYTES: u64 = 64 * 1024;

fn junk_items(ctx: &Ctx, candidates: &[(Note, note::Junk, u64)]) -> Vec<SelectItem> {
    let widths = Widths::of(
        &candidates
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>(),
        &ctx.config.note,
        ctx.home_str(),
    )
    .with_sizes(candidates.iter().map(|(_, _, bytes)| *bytes));

    candidates
        .iter()
        .map(|(note, reason, bytes)| {
            let (label, tints) = note::junk_row(note, *reason, *bytes, &widths);
            SelectItem::new(label, note.path.to_string_lossy().into_owned())
                .tints(tints)
                .preview(Preview::File)
        })
        .collect()
}

// --- searching --------------------------------------------------------------

/// The most matches one search hands back.
///
/// A two-character query against a vault matches tens of thousands of lines,
/// and a list nobody can reach the bottom of is not more useful than one they
/// can. Reaching it stops ripgrep rather than reading the rest and dropping it.
const MATCH_LIMIT: usize = 2000;

/// The width a search row's `note:line` column is padded to. A fixed number
/// rather than the widest in the batch, because rows arrive while the search is
/// still running and there is no widest yet.
const MATCH_COLUMN: usize = 40;

/// The mode that needs nothing installed, named once because two lists hold it.
const BY_NAME: select::Mode = select::Mode::new("ctrl-e", "name");

/// The keys `note open` reads the query on, names first.
///
/// Names are the default because that is what a note is usually looked for by,
/// and because the list is worth something before a key is pressed: an empty
/// query is the whole vault, where an empty search is nothing.
///
/// Of the two that search the text, fuzzy comes first for the reason a finder
/// exists at all — typing `errhand` should find "error handling", and a search
/// that only matches what was typed exactly makes the user do the remembering.
/// Exact is the key for a phrase, a path or a snippet of code, where
/// subsequence matching finds a hundred lines that merely contain the letters.
const MODES: &[select::Mode] = &[
    BY_NAME,
    select::Mode::new("ctrl-r", "text"),
    select::Mode::new("ctrl-f", "exact"),
];

/// The one mode left when ripgrep is not installed.
///
/// Offering the other two would be offering keys that find nothing and say
/// nothing about why: the selector owns the screen and has nowhere to report a
/// missing program to. Names need no ripgrep, so that much still works.
const NAMES_ONLY: &[select::Mode] = &[BY_NAME];

/// Which modes to offer, given whether ripgrep is on `PATH`.
const fn modes(has_ripgrep: bool) -> &'static [select::Mode] {
    match has_ripgrep {
        true => MODES,
        false => NAMES_ONLY,
    }
}

/// What the query is matched against, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Looking {
    /// The names of the notes, already in hand and scored the way the selector
    /// scores every other list.
    Names,
    /// Every line of every note, through ripgrep.
    Lines(Matching),
}

impl Looking {
    fn of(mode: usize) -> Self {
        match mode {
            0 => Self::Names,
            1 => Self::Lines(Matching::Fuzzy),
            _ => Self::Lines(Matching::Exact),
        }
    }
}

/// The closure the selector calls on every keystroke: the vault filtered by
/// name, or one ripgrep run per query with its rows streamed as ripgrep prints
/// them.
///
/// The notes are carried into it rather than re-read, so the name list costs
/// nothing per keystroke and the vault is walked once for the whole selector.
fn searcher(
    root: PathBuf,
    notes: Vec<Note>,
    cfg: crate::config::NoteConfig,
    offset: time::UtcOffset,
    archives: Vec<String>,
) -> select::Search {
    // Measured once, over the whole vault: a width recomputed from the rows
    // that survived the query would shift the titles as the list narrows.
    let width = note::group_width(&notes, &cfg);
    Box::new(move |query: &str, mode: usize| match Looking::of(mode) {
        Looking::Names => {
            let rows: Vec<SelectItem> = note::by_name(&notes, query)
                .into_iter()
                .map(|note| item(note, &cfg, width, offset))
                .collect();
            select::Searching {
                rows: Box::new(rows.into_iter()),
                stop: Box::new(|| {}),
            }
        }
        // An empty pattern matches every line of every note, which is neither
        // an answer nor a cheap question.
        Looking::Lines(_) if query.trim().is_empty() => nothing(),
        Looking::Lines(matching) => match Search::start(&root, query, matching, archives.clone()) {
            Ok(search) => search.into_searching(),
            // A failed spawn is an empty list: the selector is open and has
            // nowhere to report an error to, and the next keystroke tries
            // again.
            Err(_) => nothing(),
        },
    })
}

/// A search with no rows in it and nothing to call off.
fn nothing() -> select::Searching {
    select::Searching {
        rows: Box::new(std::iter::empty()),
        stop: Box::new(|| {}),
    }
}

/// How a query becomes something ripgrep can look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matching {
    /// The characters of the query, in order, with anything between them.
    Fuzzy,
    /// The query, as typed, meaning itself.
    Exact,
}

/// The pattern ripgrep is given, and whether it is a regular expression.
///
/// Fuzzy is a subsequence: every character of the query, in order, with
/// anything but a line break allowed between them — which is what a fuzzy
/// finder means and what ripgrep, having no fuzzy mode of its own, can be asked
/// for. Whitespace is dropped, so `err hand` and `errhand` look for the same
/// thing.
///
/// Every character is escaped either way. A query is being typed live, so it
/// spends most of its life as an unfinished regular expression — a lone `(` or
/// `[` would otherwise make the search fail rather than find nothing.
fn pattern(query: &str, matching: Matching) -> (String, bool) {
    match matching {
        Matching::Exact => (query.to_string(), false),
        Matching::Fuzzy => {
            let pattern = query
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(regex_escape)
                .collect::<Vec<_>>()
                .join("[^\n]*");
            (pattern, true)
        }
    }
}

/// One character as a regular expression matching only itself.
fn regex_escape(c: char) -> String {
    match c {
        '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
            format!("\\{c}")
        }
        _ => c.to_string(),
    }
}

/// One ripgrep run, read line by line./// One ripgrep run, read line by line.
///
/// The child is held behind a mutex the reader never takes, so
/// [`select::Searching::stop`] can kill it from another thread while this one
/// is blocked waiting for output — which is the whole point, since the thing
/// being waited on is exactly what has to stop.
struct Search {
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    child: Arc<Mutex<Option<std::process::Child>>>,
    root: PathBuf,
    /// Directories whose hits are dropped as they arrive. ripgrep matches
    /// `--glob` against paths relative to the working directory rather than to
    /// the directory it was told to search, so an archive cannot be excluded by
    /// one; it is read and its lines are thrown away here.
    archives: Vec<String>,
    found: usize,
}

impl Search {
    fn start(
        root: &Path,
        query: &str,
        matching: Matching,
        archives: Vec<String>,
    ) -> std::io::Result<Self> {
        let (pattern, is_regex) = pattern(query, matching);
        let mut child = std::process::Command::new("rg")
            .args([
                "--line-number",
                "--column",
                "--no-heading",
                "--color=never",
                "--smart-case",
                // A minified file or a base64 attachment in a vault is one
                // line several megabytes long, and drawing it is the only
                // slow thing a row can do.
                "--max-columns=400",
                "--glob=*.md",
                "--glob=*.markdown",
            ])
            // A fixed string in exact mode, so a query holding `.` or `(`
            // looks for those characters rather than meaning something by them.
            .args(if is_regex {
                &[][..]
            } else {
                &["--fixed-strings"][..]
            })
            // `-e` rather than a bare positional: a query beginning with `-`
            // is a pattern, not a flag, and the user is typing it live.
            .arg("-e")
            .arg(&pattern)
            .arg("--")
            .arg(root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            // ripgrep's complaints about an unfinished regex arrive on every
            // keystroke and belong nowhere: the selector owns the screen.
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .expect("stdout was piped, so it is there to take");
        Ok(Self {
            lines: std::io::BufRead::lines(std::io::BufReader::new(stdout)),
            child: Arc::new(Mutex::new(Some(child))),
            root: root.to_path_buf(),
            archives,
            found: 0,
        })
    }

    fn into_searching(self) -> select::Searching {
        let child = Arc::clone(&self.child);
        select::Searching {
            rows: Box::new(self),
            stop: Box::new(move || reap(&child)),
        }
    }
}

impl Iterator for Search {
    type Item = SelectItem;

    fn next(&mut self) -> Option<SelectItem> {
        loop {
            if self.found >= MATCH_LIMIT {
                reap(&self.child);
                return None;
            }
            // A line that is not valid UTF-8 ends the read: ripgrep is
            // searching Markdown, and the alternative is a row nobody can see
            // the text of anyway.
            let line = self.lines.next()?.ok()?;
            let Some(found) = note::parse_match(&line, &self.root) else {
                continue;
            };
            if note::is_archived(&found.rel, &self.archives) {
                continue;
            }
            self.found += 1;
            let (label, tints) = note::match_row(&found, MATCH_COLUMN);
            // The pane opens the note at the line that matched, marked, which
            // is what a row saying `note.md:41` is promising.
            let preview = select::line_preview(&found.path.to_string_lossy(), found.line);
            return Some(
                SelectItem::new(label, note::encode_match(&found))
                    .tints(tints)
                    .preview(preview),
            );
        }
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        reap(&self.child);
    }
}

/// Kill the search and collect it, so a vault-wide grep abandoned mid-word
/// leaves neither a running ripgrep nor a zombie behind. Idempotent: `stop` and
/// the drop both call it, and the selector may do so in either order.
fn reap(child: &Arc<Mutex<Option<std::process::Child>>>) {
    let mut held = child.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = held.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Open what was selected: the first match in the editor at its line, and every
/// other one in the quickfix list behind it.
///
/// Quickfix is a vim idea, so this is the shape of a vim command line. An
/// editor that is not one of that family is handed the files and nothing else —
/// there is no portable way to say "line 41" to an arbitrary program, and
/// inventing one per editor is a list that is never finished.
fn open_matches(ctx: &Ctx, editor: &[String], matches: &[note::Match]) -> Result<()> {
    let (program, args) = editor
        .split_first()
        .expect("an editor is resolved non-empty");
    if !vim_family(program) {
        let mut files: Vec<String> = Vec::new();
        for found in matches {
            let path = found.path.to_string_lossy().into_owned();
            if !files.contains(&path) {
                files.push(path);
            }
        }
        ctx.log.info(&format!(
            "{program} takes no line number; opening the files"
        ));
        return cmd::edit::launch(ctx, editor, &files);
    }

    let list = QuickfixFile::write(matches)?;
    let mut command = args.to_vec();
    command.push("-c".into());
    command.push("set errorformat=%f:%l:%c:%m".into());
    command.push("-c".into());
    command.push(format!(
        "cfile {}",
        vim_escape(&list.path.to_string_lossy())
    ));
    command.push("-c".into());
    command.push("cfirst".into());
    // One match is a jump, not a list to walk: the window would be a pane
    // showing the line already on screen.
    if matches.len() > 1 {
        command.push("-c".into());
        command.push("copen".into());
    }

    // The editor owns the terminal until the user leaves it, as in
    // [`crate::cmd::edit::launch`].
    let _waiting = stats::interacting();
    let status = std::process::Command::new(program)
        .args(&command)
        .status()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!("`{program}` was not found on PATH"),
            _ => anyhow::Error::new(e).context(format!("running {program}")),
        })?;
    if !status.success() {
        return Err(crate::Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

/// The quickfix list on disk, removed when it goes out of scope — vim reads it
/// at startup and never looks again, so it is cid's to clean up.
struct QuickfixFile {
    path: PathBuf,
}

impl QuickfixFile {
    fn write(matches: &[note::Match]) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("cid-quickfix-{}.txt", std::process::id()));
        let body: String = matches
            .iter()
            .map(|found| format!("{}\n", note::quickfix_line(found)))
            .collect();
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for QuickfixFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether `program` is a vim, and so understands a quickfix list.
fn vim_family(program: &str) -> bool {
    let name = Path::new(program)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    matches!(
        name.as_str(),
        "vim" | "nvim" | "vi" | "view" | "gvim" | "mvim" | "nvim-qt"
    )
}

/// Escape a path for a vim command line, where a space separates arguments and
/// `%` and `#` name the current and alternate file.
fn vim_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        if matches!(ch, ' ' | '\t' | '%' | '#' | '|' | '"' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Whether `program` resolves on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.exists())
    })
}

/// The `config check` row for the vault: where it is, how much is in it, and
/// how much of that a listing leaves out. One row rather than two, since a root
/// that resolves and holds nothing has already been reported by the count.
pub(crate) fn vault_summary(ctx: &Ctx) -> Result<Vault> {
    let root = vault(ctx)?;
    let archives = &ctx.config.note.archives;
    let (mut listed, mut archived) = (0, 0);
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(true)
        .require_git(false)
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .filter(|entry| note::is_note(entry.path()))
    {
        match note::is_archived(&relative(entry.path(), &root), archives) {
            true => archived += 1,
            false => listed += 1,
        }
    }
    Ok(Vault {
        root,
        listed,
        archived,
    })
}

/// What `config check` found in the vault.
pub(crate) struct Vault {
    pub root: PathBuf,
    /// Notes a listing shows.
    pub listed: usize,
    /// Notes under `[note] archives`, which only `--all` shows.
    pub archived: usize,
}

/// The `[note] archives` entries naming nothing that is there — a typo, or a
/// directory since renamed, which hides no notes and says nothing about it.
pub(crate) fn missing_archives(ctx: &Ctx) -> Result<Vec<String>> {
    let root = vault(ctx)?;
    Ok(ctx
        .config
        .note
        .archives
        .iter()
        .filter(|archive| !root.join(archive).is_dir())
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn utc() -> time::UtcOffset {
        time::UtcOffset::UTC
    }

    #[test]
    fn a_note_takes_its_name_from_where_it_sits_below_the_vault() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("work/meetings")).unwrap();
        let path = root.join("work/meetings/standup.md");
        std::fs::write(&path, "# Standup\n").unwrap();

        let note = read_note(&path, root, utc()).unwrap();

        assert_eq!(note.rel, "work/meetings/standup.md");
        assert_eq!(note.title(), "standup");
        assert_eq!(note.dir, "work");
    }

    #[test]
    fn front_matter_is_read_from_the_head_of_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(
            &path,
            "---\ntitle: Error handling\ntags: [rust, til]\n---\n\nbody\n",
        )
        .unwrap();

        let note = read_note(&path, dir.path(), utc()).unwrap();

        assert_eq!(note.title(), "Error handling");
        assert_eq!(note.tag_column(), "#rust #til");
    }

    /// The bound is what keeps a vault one pass rather than a full read per
    /// note; a note longer than it must still list, by its filename.
    #[test]
    fn a_note_larger_than_the_head_bound_still_lists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long.md");
        let body = "x".repeat(HEAD_BYTES as usize * 2);
        std::fs::write(&path, format!("---\ntitle: Long\n---\n{body}")).unwrap();

        let note = read_note(&path, dir.path(), utc()).unwrap();

        assert_eq!(note.title(), "Long");
    }

    /// A note nothing named a creation date for falls back to the filesystem,
    /// and never reads as newer than its own last edit.
    #[test]
    fn an_undated_note_is_created_no_later_than_it_was_modified() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "body\n").unwrap();

        let note = read_note(&path, dir.path(), utc()).unwrap();

        assert!(note.created <= note.modified, "{note:?}");
    }

    fn vault() -> Vec<Note> {
        ["work/meetings/standup.md", "scratch/idea.md", "inbox.md"]
            .iter()
            .map(|rel| Note {
                path: PathBuf::from("/vault").join(rel),
                rel: (*rel).to_string(),
                dir: note::top_dir(rel).to_string(),
                modified: 0,
                created: 0,
                front: note::Front::default(),
            })
            .collect()
    }

    /// Name mode rebuilds its rows on every keystroke, and a group column
    /// measured over the survivors would shift the titles left as the list
    /// narrows.
    #[test]
    fn the_group_column_holds_its_width_as_a_query_narrows_the_list() {
        let notes = vault();
        let cfg = crate::config::NoteConfig::default();
        let mut search = searcher(PathBuf::from("/vault"), notes, cfg, utc(), Vec::new());
        let mut widths = |query: &str| -> Vec<usize> {
            (search)(query, 0)
                .rows
                .map(|row| row.prefix.unwrap().chars().count())
                .collect()
        };

        let whole = widths("");
        let narrowed = widths("inbox");
        assert_eq!(whole.len(), 3);
        assert_eq!(narrowed.len(), 1);
        assert!(whole.windows(2).all(|w| w[0] == w[1]), "{whole:?}");
        assert_eq!(narrowed[0], whole[0]);
    }

    /// Only a vim has a quickfix list. Anything else is handed the files.
    #[test]
    fn only_a_vim_is_offered_a_quickfix_list() {
        for editor in ["nvim", "vim", "/usr/bin/vi", "/opt/homebrew/bin/nvim"] {
            assert!(vim_family(editor), "{editor}");
        }
        for editor in ["glow", "code", "hx", "emacs", "bat"] {
            assert!(!vim_family(editor), "{editor}");
        }
    }

    /// A space separates arguments on a vim command line, and `%` and `#` name
    /// the current and alternate file — so a temp path holding one would open
    /// something else entirely.
    #[test]
    fn a_path_handed_to_vim_is_escaped() {
        assert_eq!(vim_escape("/tmp/a b.txt"), r"/tmp/a\ b.txt");
        assert_eq!(vim_escape("/tmp/100%.txt"), r"/tmp/100\%.txt");
        assert_eq!(vim_escape("/tmp/plain.txt"), "/tmp/plain.txt");
    }

    #[test]
    fn a_named_note_resolves_against_the_vault_but_an_absolute_path_does_not() {
        let (root, home) = (Path::new("/vault"), Path::new("/home/me"));
        assert_eq!(
            resolve(root, home, "work/standup.md"),
            "/vault/work/standup.md"
        );
        assert_eq!(resolve(root, home, "/elsewhere/a.md"), "/elsewhere/a.md");
        assert_eq!(resolve(root, home, "~/inbox.md"), "/home/me/inbox.md");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// Typing `errhand` should find "error handling": the letters, in order,
    /// with anything but a line break between them.
    #[test]
    fn a_fuzzy_query_becomes_a_subsequence() {
        let (pattern, is_regex) = pattern("abc", Matching::Fuzzy);
        assert_eq!(pattern, "a[^\n]*b[^\n]*c");
        assert!(is_regex);
    }

    /// Whitespace is dropped, so `err hand` and `errhand` look for one thing.
    #[test]
    fn a_fuzzy_query_ignores_the_spaces_in_it() {
        assert_eq!(
            pattern("a b", Matching::Fuzzy).0,
            pattern("ab", Matching::Fuzzy).0
        );
    }

    /// A query is typed live, so it spends most of its life as an unfinished
    /// regular expression. A lone `(` must find nothing, not fail the search.
    #[test]
    fn a_half_typed_query_is_not_a_broken_regex() {
        for query in ["(", "[a", "a|", "*", "\\"] {
            let (pattern, _) = pattern(query, Matching::Fuzzy);
            assert!(
                regex_is_sane(&pattern),
                "{query:?} became {pattern:?}, which ripgrep would refuse"
            );
        }
    }

    /// Every metacharacter escaped, and every escape balanced — which is what
    /// makes the pattern above one ripgrep accepts.
    fn regex_is_sane(pattern: &str) -> bool {
        let mut chars = pattern.chars().peekable();
        let mut depth = 0i32;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if chars.next().is_none() {
                        return false;
                    }
                }
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        }
        depth == 0
    }

    /// Exact means exact: a query holding `.` or `(` looks for those
    /// characters rather than meaning something by them.
    #[test]
    fn an_exact_query_is_handed_over_untouched() {
        let (pattern, is_regex) = pattern("a.c(", Matching::Exact);
        assert_eq!(pattern, "a.c(");
        assert!(!is_regex, "an exact query would be read as a regex");
    }

    /// The mode the selector opens in is the one that needs no ripgrep and has
    /// something to show before a key is pressed.
    #[test]
    fn the_first_mode_is_the_one_that_reads_names() {
        assert_eq!(Looking::of(0), Looking::Names);
        assert_eq!(Looking::of(1), Looking::Lines(Matching::Fuzzy));
        assert_eq!(Looking::of(2), Looking::Lines(Matching::Exact));
        assert_eq!(MODES[0].label, "name");
    }

    /// Without ripgrep the two keys that would need it are not offered: they
    /// would find nothing, and a selector has nowhere to say why.
    #[test]
    fn the_modes_that_search_the_text_are_offered_only_with_ripgrep() {
        assert_eq!(modes(true).len(), 3);
        assert_eq!(modes(false).len(), 1);
        assert_eq!(modes(false)[0].label, "name");
        // Whichever list is in force, mode 0 is the same one.
        assert_eq!(Looking::of(0), Looking::Names);
    }
}
