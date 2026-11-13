//! Interactive fuzzy selection, built in via [`skim`].
//!
//! The fuzzy finder is compiled into the binary — there is no `fzf` subprocess.
//! Every place that asks the user to choose goes through here.
//!
//! Items carry a separate [`SelectItem::label`] (shown and fuzzy-matched) and
//! value (returned on selection), so the selector can show a `~`-collapsed path
//! while still returning an absolute one.
//!
//! Rows arrive either all at once ([`select_one`], [`select_many`]) or as they
//! are discovered ([`select_one_streamed`], [`select_many_streamed`]).

use std::ops::Range;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use skim::fuzzy_matcher::FuzzyMatcher;
use skim::prelude::*;

use crate::config::SelectorConfig;
use crate::term;

/// A user-cancelled selection (Esc / Ctrl-C). Distinct from "nothing matched"
/// so the caller can exit 130 without printing anything.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("selection cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What the preview pane shows for the highlighted row.
///
/// Prefer [`Preview::Text`] whenever the data is already in hand. A
/// [`Preview::Command`] is only appropriate for work that is local and fast
/// (tens of milliseconds), because skim spawns it again on every move through
/// the list and does not kill the previous child.
pub enum Preview {
    /// Ready-made text; ANSI escapes are honoured.
    Text(String),
    /// A shell command, run only when the row is highlighted. skim exports
    /// `COLUMNS` and `ROWS` to it.
    Command(String),
    /// The row's own value, previewed as a file. Built when the row is
    /// highlighted rather than when it is created: a streamed walk can produce
    /// a million rows, and a command string on each is megabytes of panes
    /// nobody looks at.
    File,
    /// The row's own value, previewed as a directory. [`Preview::File`]'s
    /// counterpart, and deferred for the same reason — the directory walk is
    /// streamed too.
    Dir,
    /// Text cid builds itself, the moment the row is highlighted.
    ///
    /// [`Preview::Text`] for a pane too expensive to hold one of per row: a
    /// vault of two thousand notes read and rendered up front is two thousand
    /// panes nobody looks at. Runs on skim's preview thread, so it is held to
    /// the same bar as a [`Preview::Command`] — local, bounded, tens of
    /// milliseconds — and ANSI escapes in what it returns are honoured.
    Deferred(Box<dyn Fn() -> String + Send + Sync>),
}

/// Hand-written because [`Preview::Deferred`] holds a closure, which derives
/// nothing. A pane's whole body would drown the assertion that printed it, so
/// each variant reports its name and `Text` and `Command` the string they were
/// built from.
impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(text).finish(),
            Self::Command(command) => f.debug_tuple("Command").field(command).finish(),
            Self::File => f.write_str("File"),
            Self::Dir => f.write_str("Dir"),
            Self::Deferred(_) => f.write_str("Deferred(..)"),
        }
    }
}

/// Score `choice` against `query` the way the selector scores what it matches
/// itself, or `None` where the two do not match at all.
///
/// For a list a searching selector cannot rank on its own: the query has gone
/// to the search, so a mode that filters something already in memory does its
/// own matching, and going through skim's matcher is what keeps that list
/// ordered like every other one.
pub fn score(choice: &str, query: &str) -> Option<i64> {
    static MATCHER: std::sync::LazyLock<SkimMatcherV2> =
        std::sync::LazyLock::new(SkimMatcherV2::default);
    MATCHER.fuzzy_match(choice, query)
}

/// Quote `arg` for the shell that runs a [`Preview::Command`], so a branch name
/// or path containing spaces or quotes cannot alter the command.
pub fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// What a preview pane says on its last row when its content ran past it.
///
/// Nothing else tells the reader: skim's own `line/total` indicator appears
/// only once a pane has been scrolled, and a pane at rest looks the same
/// whether it ends on screen or a thousand lines below.
const OVERFLOW_HINT: &str = "⋯ more";

/// [`OVERFLOW_HINT`] as the row that ends the pane.
fn overflow_marker() -> String {
    term::paint(OVERFLOW_HINT, term::SECONDARY, true)
}

/// How many rows of a pane `height` tall a preview may draw on.
///
/// One short of what skim reports, which is the pane's outer height: a `down`
/// or `up` preview window spends a row of that on a border where a `right` or
/// `left` one does not. Overshooting would clip the bottom row, and the bottom
/// row is the one that has to be on screen, so the row is given up in every
/// layout.
fn preview_rows(height: usize) -> usize {
    height.saturating_sub(1)
}

/// `text` cut to a pane `height` rows tall, ending in an [`overflow_marker`]
/// when there was more of it than fitted.
///
/// The content is cut rather than left long and scrollable: the marker is only
/// truthful on the row the reader's eye stops at, and that is the bottom one.
fn with_overflow_marker(text: String, height: usize) -> String {
    let rows = preview_rows(height);
    if rows == 0 || text.lines().count() <= rows {
        return text;
    }
    let mut out: String = text
        .lines()
        .take(rows - 1)
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&overflow_marker());
    out
}

/// `cmd` cut and marked the same way — for a pane whose content only the shell
/// has seen. `ROWS` is the pane height skim exports to a preview command.
///
/// The filter holds a pane's worth of lines and stops reading at the first line
/// past it, so a command that prints a great deal is cut short rather than read
/// out. stderr joins stdout because skim sees the filter's exit status rather
/// than the command's, and a preview that failed would otherwise draw nothing
/// at all.
///
/// `keep` is [`preview_rows`] worked out in awk, the pane height being
/// something only the run knows.
///
/// Every brace pair in the awk program is closed before the next one opens:
/// skim expands `{...}` in a preview command as one of its own field
/// placeholders, and a `{` whose `}` arrives only after another `{` is dropped
/// along with everything between them.
fn with_overflow_filter(cmd: &str) -> String {
    let marker = overflow_marker();
    format!(
        "{{ {cmd}; }} 2>&1 | awk -v rows=\"${{ROWS:-0}}\" '\
         BEGIN {{ keep = rows - 1 }} \
         keep < 1 {{ print; next }} \
         NR <= keep {{ held[NR] = $0; next }} \
         {{ over = 1; exit }} \
         END {{ if (keep < 1) exit; if (over) keep = keep - 1; \
         for (i = 1; i <= keep && i <= NR; i++) print held[i]; \
         if (over) print \"{marker}\" }}'"
    )
}

/// The preview for a file: its contents, via `bat` when installed and `head`
/// otherwise, bounded to 200 lines.
pub fn file_preview(path: &str) -> Preview {
    Preview::Command(file_preview_cmd(path))
}

/// The command behind [`Preview::Dir`]: what is directly inside the directory,
/// capped at 200 rows.
fn dir_preview_cmd(path: &str) -> String {
    let path = quote(path);
    format!("ls -Ap -- {path} 2>&1 | head -n 200")
}

/// The preview for a git checkout — a repository or one of its worktrees: the
/// current branch and working-tree state, then recent commits. Both commands
/// are local and take tens of milliseconds.
///
/// `--no-optional-locks` matters here: a plain `git status` rewrites the index,
/// so scrolling past a checkout would contend for its index lock.
pub fn checkout_preview(path: &str) -> Preview {
    let checkout = quote(path);
    Preview::Command(format!(
        "git --no-optional-locks -C {checkout} -c color.status=always status --short --branch \
         | head -n 10; \
         git --no-optional-locks -C {checkout} log --color=always --max-count=20 --date=relative \
         --format='%C(auto)%h%C(reset)  %C(blue)%an%C(reset)  %C(green)%ad%C(reset)  %s'"
    ))
}

/// The command behind [`Preview::File`] and [`file_preview`].
fn file_preview_cmd(path: &str) -> String {
    let path = quote(path);
    format!(
        "bat --color=always --style=plain {theme} {SYNTAX_MAP} --line-range=:200 -- {path} \
         2>/dev/null || head -n 200 -- {path}",
        theme = theme_flag(),
    )
}

/// Syntaxes for extensions bat ships no grammar for, as flags for its command line.
///
/// JSON5 goes to JavaScript rather than JSON: bat's JSON grammar marks unquoted
/// keys, single-quoted strings and hex numbers as invalid, and every JSON5
/// document is an ES5 object literal.
const SYNTAX_MAP: &str = "--map-syntax='*.json5:JavaScript'";

/// How much of a file the preview shows either side of a line it is opened at.
const LINE_CONTEXT: u32 = 12;

/// The preview for one *line* of a file: the lines around it, with that one
/// marked — for a search result, where the answer is a place rather than a file.
///
/// Line numbers are drawn here where an ordinary file preview leaves them out:
/// the row said `note.md:41`, and a pane that cannot show 41 is asking to be
/// counted down by hand.
pub fn line_preview(path: &str, line: u32) -> Preview {
    let quoted = quote(path);
    let first = line.saturating_sub(LINE_CONTEXT).max(1);
    let last = line.saturating_add(LINE_CONTEXT);
    Preview::Command(format!(
        "bat --color=always --style=numbers {theme} {SYNTAX_MAP} --highlight-line {line} \
         --line-range={first}:{last} -- {quoted} 2>/dev/null \
         || sed -n '{first},{last}p' -- {quoted}",
        theme = theme_flag(),
    ))
}

/// The `--theme` bat is given, as a flag ready to drop into a command line, or
/// nothing when the configuration asked for nothing.
///
/// A theme bat does not know is not an error: it warns on stderr — which the
/// preview command already discards — and falls back to its own default. So an
/// older bat, or one whose theme set does not include the configured one, keeps
/// working and simply looks like the user's own `bat` does.
fn theme_flag() -> String {
    match preview_theme() {
        "" => String::new(),
        theme => format!("--theme={}", quote(theme)),
    }
}

/// The preview theme, resolved once from the configuration the first selector
/// ran with.
///
/// Set rather than passed because a [`Preview::File`] builds its command inside
/// [`SkimItem::preview`], which skim calls on a row long after the row was made
/// and with no configuration in reach. One process reads one configuration —
/// [`crate::Ctx`] resolves it once — so a value fixed for the life of the
/// process is the same value every caller would have been handed.
static PREVIEW_THEME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn preview_theme() -> &'static str {
    PREVIEW_THEME.get().map(String::as_str).unwrap_or_default()
}

/// A stretch of a label drawn in its own colour, as a character range.
///
/// Where [`SelectItem::color`] says something about the whole row, a tint says
/// something about one column of it — so a row can carry two facts that are
/// true of different parts of it without either claiming the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tint {
    pub range: Range<usize>,
    pub color: u8,
}

/// One choice in the selector: `label` is displayed and matched against,
/// [`SelectItem::value`] is returned when it is selected, `color` optionally
/// tints the row (an ANSI 256-colour index, so it respects the terminal
/// theme), and `preview` fills the preview pane while the row is highlighted.
pub struct SelectItem {
    pub label: String,
    /// Drawn ahead of the label and *not* matched against — for a column that
    /// identifies a row without being what one searches for, such as the date
    /// on a history entry. Dim, unless [`SelectItem::prefix_tints`] says
    /// otherwise.
    pub prefix: Option<String>,
    /// Colours within the prefix, as character ranges into it. Indexed from
    /// the start of the prefix, not of the row.
    pub prefix_tints: Vec<Tint>,
    /// Drawn after the label and *not* matched against — [`SelectItem::prefix`]
    /// on the other side, for a column that belongs to the right-hand edge
    /// rather than the left. Dim, unless [`SelectItem::suffix_tints`] says
    /// otherwise.
    pub suffix: Option<String>,
    /// Colours within the suffix, as character ranges into it. Indexed from
    /// the start of the suffix, not of the row.
    pub suffix_tints: Vec<Tint>,
    /// `None` when the value is the label itself. Read through
    /// [`SelectItem::value`].
    value: Option<String>,
    pub color: Option<u8>,
    pub tints: Vec<Tint>,
    pub preview: Option<Preview>,
}

impl SelectItem {
    /// An item whose displayed label is also its returned value.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            label: text.into(),
            prefix: None,
            prefix_tints: Vec::new(),
            suffix: None,
            suffix_tints: Vec::new(),
            value: None,
            color: None,
            tints: Vec::new(),
            preview: None,
        }
    }

    /// An item with a distinct display label and returned value.
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            prefix: None,
            prefix_tints: Vec::new(),
            suffix: None,
            suffix_tints: Vec::new(),
            value: Some(value.into()),
            color: None,
            tints: Vec::new(),
            preview: None,
        }
    }

    /// Draw `prefix` ahead of the label, outside what the query matches, with
    /// `tints` colouring ranges of it.
    pub fn prefix(mut self, prefix: impl Into<String>, tints: Vec<Tint>) -> Self {
        self.prefix = Some(prefix.into());
        self.prefix_tints = tints;
        self
    }

    /// Draw `suffix` after the label, outside what the query matches, with
    /// `tints` colouring ranges of it.
    pub fn suffix(mut self, suffix: impl Into<String>, tints: Vec<Tint>) -> Self {
        self.suffix = Some(suffix.into());
        self.suffix_tints = tints;
        self
    }

    /// What selecting this row yields — the label unless one was set apart.
    pub fn value(&self) -> &str {
        self.value.as_deref().unwrap_or(&self.label)
    }

    /// Tint the row with an ANSI 256-colour index.
    pub fn color(mut self, color: u8) -> Self {
        self.color = Some(color);
        self
    }

    /// Colour individual columns of the label. See [`Tint`].
    pub fn tints(mut self, tints: Vec<Tint>) -> Self {
        self.tints = tints;
        self
    }

    /// Show `preview` in the preview pane while this row is highlighted.
    pub fn preview(mut self, preview: Preview) -> Self {
        self.preview = Some(preview);
        self
    }
}

/// Colour of a [`SelectItem::prefix`] and its suffix: both sit beside the label
/// rather than in it, which is [`crate::term::SECONDARY`].
const PREFIX_COLOR: u8 = crate::term::SECONDARY;

/// Recolour the character ranges in `tints`, leaving skim's match highlighting
/// alone: a span drawn in anything but `base` is a character the query matched,
/// and why the row is on screen at all outranks which column it sits in.
fn tinted<'a>(line: Line<'a>, tints: &[Tint], base: Style) -> Line<'a> {
    let color_at = |index: usize| {
        tints
            .iter()
            .find(|tint| tint.range.contains(&index))
            .map(|tint| tint.color)
    };
    let style = |color: Option<u8>| match color {
        Some(color) => base.fg(Color::Indexed(color)),
        None => base,
    };

    let mut out = Line::default();
    let mut at = 0;
    for span in line.spans {
        let width = span.content.chars().count();
        if span.style != base {
            out.push_span(span);
            at += width;
            continue;
        }
        // One span per run of characters sharing a colour, rather than per
        // character: a row is redrawn on every keystroke.
        let mut run = String::new();
        let mut color = None;
        for (offset, ch) in span.content.chars().enumerate() {
            let next = color_at(at + offset);
            if next != color && !run.is_empty() {
                out.push_span(Span::styled(std::mem::take(&mut run), style(color)));
            }
            color = next;
            run.push(ch);
        }
        if !run.is_empty() {
            out.push_span(Span::styled(run, style(color)));
        }
        at += width;
    }
    out
}

/// Bridges a [`SelectItem`] to skim.
struct SkItem {
    item: SelectItem,
}

impl SkimItem for SkItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.item.label)
    }

    fn output(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.item.value())
    }

    fn display(&self, mut context: DisplayContext) -> Line<'_> {
        if let Some(idx) = self.item.color {
            context.base_style = context.base_style.fg(Color::Indexed(idx));
        }

        let base = context.base_style;
        let mut line = context.to_line(self.text());
        if !self.item.tints.is_empty() {
            line = tinted(line, &self.item.tints, base);
        }

        if self.item.prefix.is_none() && self.item.suffix.is_none() {
            return line;
        }

        // Added as spans rather than folded into the label, so `to_line`'s
        // highlight positions still index the matched text.
        let dim = Style::default().fg(Color::Indexed(PREFIX_COLOR));
        let mut out = Line::default();
        if let Some(prefix) = &self.item.prefix {
            let plain = Line::from(Span::styled(prefix.clone(), dim));
            out.spans
                .extend(tinted(plain, &self.item.prefix_tints, dim).spans);
        }
        out.spans.extend(line.spans);
        if let Some(suffix) = &self.item.suffix {
            let plain = Line::from(Span::styled(suffix.clone(), dim));
            out.spans
                .extend(tinted(plain, &self.item.suffix_tints, dim).spans);
        }
        out
    }

    fn preview(&self, context: PreviewContext) -> ItemPreview {
        // Every pane is marked here rather than where its content is built, so
        // a preview added later cannot forget to say that it runs long.
        match &self.item.preview {
            Some(Preview::Text(text)) => {
                ItemPreview::AnsiText(with_overflow_marker(text.clone(), context.height))
            }
            Some(Preview::Command(cmd)) => ItemPreview::Command(with_overflow_filter(cmd)),
            Some(Preview::File) => {
                ItemPreview::Command(with_overflow_filter(&file_preview_cmd(self.item.value())))
            }
            Some(Preview::Dir) => {
                ItemPreview::Command(with_overflow_filter(&dir_preview_cmd(self.item.value())))
            }
            Some(Preview::Deferred(build)) => {
                ItemPreview::AnsiText(with_overflow_marker(build(), context.height))
            }
            // Blank rather than `Global`, which would run the empty global
            // preview command.
            None => ItemPreview::Text(String::new()),
        }
    }
}

/// Select exactly one item, returning its value. Returns [`Cancelled`] as an
/// error on cancel; the caller decides whether that is a silent exit or a
/// failure.
pub fn select_one(items: Vec<SelectItem>, prompt: &str, cfg: &SelectorConfig) -> Result<String> {
    one(Feed::batch(items), prompt, cfg)
}

/// [`select_one`], opened with `query` already in the search box.
///
/// For a selector reached part-way through typing — ctrl-r after half a command —
/// where retyping what is already on the line to narrow the list is precisely
/// the work the selector was opened to save. An empty `query` is the ordinary
/// case and starts the selector on everything.
pub fn select_one_queried(
    items: Vec<SelectItem>,
    prompt: &str,
    query: &str,
    cfg: &SelectorConfig,
) -> Result<String> {
    let run = Run {
        prompt,
        multi: false,
        query,
        reload: None,
        search: None,
        modes: &[],
        views: Views::NONE,
        actions: &[],
    };
    run_selector(Feed::batch(items), run, cfg)?
        .values
        .into_iter()
        .next()
        .ok_or_else(|| Cancelled.into())
}

/// The key that asks a reloadable selector to go and get fresher rows. It
/// displaces skim's own `ctrl-r`, so only the selectors over remote data offer
/// it.
pub const REFRESH_KEY: &str = "ctrl-r";

/// skim's `ctrl-l`, made to do nothing.
///
/// The default binding is `clear-screen`, which reaches
/// `ratatui::Terminal::clear`, which asks the terminal where the cursor is by
/// writing `ESC [ 6 n` to *stdout*. A selector is read through its stdout — a
/// shell captures the path `repo sel` prints — so the query goes into a pipe,
/// no terminal ever answers it, and the run dies two seconds later on
/// crossterm's timeout. skim's `redraw` takes the same path, so there is
/// nothing safe to put the key on. It goes first in the list handed to skim,
/// which keeps the last binding written for a key.
const CLEAR_SCREEN_KEY: &str = "ctrl-l:ignore";

/// The key that hides and shows the preview pane. Offered wherever there is a
/// pane to hide: a pane costs half the width, and a row worth reading in full
/// is exactly when it is in the way.
///
/// `ctrl-v` is free in skim's default keymap, and inside a full-screen TUI it
/// carries none of the meanings it has at a shell prompt.
const PREVIEW_KEY: &str = "ctrl-v";

/// A second thing a selector can do, on a key of its own.
///
/// One list of pull requests answers "which one" whichever verb asked, and the
/// verb you wanted is often not the one you opened. An action lets the same
/// selector close on a different one; [`Outcome::acted`] says which.
pub struct Action {
    /// skim's spelling of the key — `f2`, `ctrl-x`.
    pub key: &'static str,
    /// What the header calls it. A verb, two words at most.
    pub label: &'static str,
}

impl Action {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self { key, label }
    }
}

/// What a selector with [`Action`]s came back with.
pub struct Chosen {
    /// The row's value.
    pub value: String,
    /// The action taken, when it was not the ordinary one. `None` is enter.
    pub action: Option<&'static str>,
}

/// Fetches a fresh set of rows for the view at that index. Called on a
/// background thread, so it may block for as long as the network takes.
///
/// A selector with no [`views`](Run::views) is always asked for view 0.
///
/// Infallible by construction: a reload that fails should hand back the old
/// rows and remember the error, which only the caller can do.
pub type Reload = Box<dyn FnMut(usize) -> Vec<SelectItem> + Send>;

/// [`select_one`], with [`REFRESH_KEY`] bound to reloading the list in place.
///
/// The selector does not close and reopen: `reload` runs on a background thread
/// while skim keeps drawing, and the query, cursor and preview pane are never
/// disturbed.
pub fn select_one_reloading(
    items: Vec<SelectItem>,
    prompt: &str,
    cfg: &SelectorConfig,
    reload: Reload,
    actions: &'static [Action],
) -> Result<Chosen> {
    let run = Run {
        prompt,
        multi: false,
        query: "",
        reload: Some(reload),
        search: None,
        modes: &[],
        views: Views::NONE,
        actions,
    };
    chosen(run_selector(Feed::batch(items), run, cfg)?)
}

/// [`select_one`], with `actions` offered beside enter.
pub fn select_one_acting(
    items: Vec<SelectItem>,
    prompt: &str,
    cfg: &SelectorConfig,
    actions: &'static [Action],
) -> Result<Chosen> {
    let run = Run {
        prompt,
        multi: false,
        query: "",
        reload: None,
        search: None,
        modes: &[],
        views: Views::NONE,
        actions,
    };
    chosen(run_selector(Feed::batch(items), run, cfg)?)
}

/// The one row an acting selector closed on, and how.
fn chosen(outcome: Outcome) -> Result<Chosen> {
    let action = outcome.acted;
    outcome
        .values
        .into_iter()
        .next()
        .map(|value| Chosen { value, action })
        .ok_or_else(|| Cancelled.into())
}

/// The [`CommandCollector`] behind [`select_one_reloading`]: skim thinks it is
/// running a command, and it is calling a closure.
struct ReloadCollector {
    reload: Arc<Mutex<Reload>>,
    /// Which view the next reload produces. Held across reloads because the
    /// view key says only "the next one", and [`REFRESH_KEY`] says nothing at
    /// all: both leave the collector to know where the cycle is.
    view: Arc<AtomicUsize>,
    /// How many views there are to step through, `0` where there are none.
    views: usize,
}

/// How often the counted thread looks up from waiting to see whether skim has
/// asked it to stop.
const RELOAD_POLL: Duration = Duration::from_millis(20);

impl CommandCollector for ReloadCollector {
    fn invoke(
        &mut self,
        cmd: &str,
        components_to_stop: Arc<AtomicUsize>,
    ) -> (SkimItemReceiver, Sender<i32>) {
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let (tx_interrupt, rx_interrupt) = unbounded::<i32>();
        let reload = Arc::clone(&self.reload);
        if cmd == NEXT_VIEW && self.views > 0 {
            let next = (self.view.load(Ordering::SeqCst) + 1) % self.views;
            self.view.store(next, Ordering::SeqCst);
        }
        let view = self.view.load(Ordering::SeqCst);

        // `ReaderControl::kill` *busy-waits* on this counter, so the counted
        // thread has to decrement promptly when asked. The reload itself
        // therefore runs on a second, uncounted thread that this one waits on.
        // A reload started while another runs does not cancel it; it queues.
        components_to_stop.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let (tx_rows, rx_rows) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let rows = (reload.lock().expect("reload closure poisoned"))(view);
                let _ = tx_rows.send(rows);
            });

            loop {
                if matches!(rx_interrupt.try_recv(), Ok(Some(_)) | Err(_)) {
                    break;
                }
                match rx_rows.recv_timeout(RELOAD_POLL) {
                    Ok(rows) => {
                        let _ = tx_item.send(rows.into_iter().map(into_skim).collect());
                        break;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    // The worker panicked; leave the list as it is.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // Dropping this is what stops skim's spinner, so it has to happen
            // before the count drops.
            drop(tx_item);
            components_to_stop.fetch_sub(1, Ordering::SeqCst);
        });

        (rx_item, tx_interrupt)
    }
}

/// One query's worth of rows, and the means to stop producing them.
///
/// [`Searching::stop`] is called from another thread the moment the selector
/// abandons this search — the query moved on, or the selector closed — and
/// must be safe to call while `rows` is blocked waiting for the next one.
/// Making it end the iterator is the whole contract: skim's reader busy-waits
/// for its readers to stop, so a search that keeps producing after it was
/// called burns a core.
pub struct Searching {
    pub rows: Box<dyn Iterator<Item = SelectItem> + Send>,
    pub stop: Box<dyn Fn() + Send + Sync>,
}

/// Produces the rows for a query, in the manner `mode` names. Called afresh on
/// every keystroke, on a background thread, so it may block for as long as the
/// search takes.
pub type Search = Box<dyn FnMut(&str, usize) -> Searching + Send>;

/// One way a [`Search`] can read the query, offered on a key of its own.
///
/// A key each rather than one that steps through them, because a header cannot
/// be made to alternate: skim's `set-header` writes a fixed string, and a
/// binding that rebinds itself to the next mode would have to contain its own
/// text. Deliberate keys also cost the same one keystroke and never leave a
/// doubt about which mode is in force — which is the thing the header is for.
///
/// The query survives the switch and is read again in the new mode. A mode key
/// is a second opinion on what was already typed — the same words, looked for
/// another way — so what the list holds changes and what is in the box does
/// not.
pub struct Mode {
    /// skim's spelling of the key — `ctrl-f`.
    pub key: &'static str,
    /// What the header calls it, once chosen.
    pub label: &'static str,
}

impl Mode {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self { key, label }
    }
}

/// Lists the selector can step through on one key, in order.
///
/// One key rather than a key each, because a key is a scarce thing in a
/// terminal and the caller's own bindings have first claim on them. The cost is
/// that the header cannot name the list in force: skim's `set-header` writes a
/// fixed string, so a key that alternates would have to carry the text of the
/// binding that replaces it, which carries the text of the one that replaces
/// *that*. What the list holds says which it is instead — the marks, and the
/// count skim draws under the query.
pub struct Views {
    /// skim's spelling of the key that steps to the next one — `ctrl-t`.
    pub key: &'static str,
    /// What each list is called, in the order the key steps through them. The
    /// first is what the selector opens in.
    pub labels: &'static [&'static str],
}

impl Views {
    pub const fn new(key: &'static str, labels: &'static [&'static str]) -> Self {
        Self { key, labels }
    }

    /// Nothing to step through.
    const NONE: Self = Self::new("", &[]);

    fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }
}

/// What a view key hands the collector. It says "the next one" rather than
/// which one, since the binding is fixed and the collector is what remembers
/// where the cycle has got to.
const NEXT_VIEW: &str = "\u{2}+\u{2}";

/// The marker a mode key's reload puts in front of the query, so the collector
/// can tell "search again in mode 2" from "the query changed".
///
/// A control character: skim expands the query into this template, and anything
/// a user could type would be a query somebody means.
const MODE_MARKER: char = '\u{2}';

/// How many rows to hand skim at once, and how long the counted thread waits
/// on the producer before looking up to see whether it has been called off.
const SEARCH_BATCH: usize = 256;
const SEARCH_QUEUE: usize = 4;
const SEARCH_POLL: Duration = Duration::from_millis(20);

/// Undo the shell quoting skim applies to a substituted `{q}` — the inverse of
/// [`quote`], and the reason a search sees what was typed rather than what a
/// shell would have been handed.
///
/// skim expands the query into the command template as though a shell were
/// going to run it. Nothing here runs a shell: the "command" is this crate's
/// own closure, and the quoting has to come back off before the search sees a
/// pattern with `'\''` in the middle of it.
fn unquote(quoted: &str) -> String {
    let Some(inner) = quoted
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    else {
        return quoted.to_string();
    };
    inner.replace(r"'\''", "'")
}

/// Split a mode marker off the front of what skim handed the collector.
///
/// A mode key reloads with `<marker><n><marker>` in front of the query; an
/// ordinary keystroke reloads with the query alone. Either way what follows is
/// still shell-quoted — skim expands `{q}` inside the template, not the
/// template itself — so this runs before [`unquote`], never after.
fn read_mode(command: &str) -> (Option<usize>, String) {
    let Some(rest) = command.strip_prefix(MODE_MARKER) else {
        return (None, command.to_string());
    };
    match rest.split_once(MODE_MARKER) {
        Some((mode, query)) => (mode.parse().ok(), query.to_string()),
        None => (None, command.to_string()),
    }
}

/// The [`CommandCollector`] behind [`select_many_searching`]: skim thinks it is
/// running a command per keystroke, and it is calling a closure.
struct SearchCollector {
    search: Arc<Mutex<Search>>,
    /// Which [`Mode`] the next search reads the query in. Held across
    /// keystrokes because only the mode *keys* say anything about it: an
    /// ordinary keystroke re-runs the template, which carries no marker.
    mode: Arc<AtomicUsize>,
}

impl CommandCollector for SearchCollector {
    fn invoke(
        &mut self,
        cmd: &str,
        components_to_stop: Arc<AtomicUsize>,
    ) -> (SkimItemReceiver, Sender<i32>) {
        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let (tx_interrupt, rx_interrupt) = unbounded::<i32>();
        // The marker first, then the quoting: skim quotes only the query it
        // substituted for `{q}`, so a mode key's marker arrives outside the
        // quotes rather than inside them.
        let (mode, quoted) = read_mode(cmd);
        let query = unquote(&quoted);
        if let Some(mode) = mode {
            self.mode.store(mode, Ordering::SeqCst);
        }
        let mode = self.mode.load(Ordering::SeqCst);
        let search = Arc::clone(&self.search);

        // Counted, and therefore never the thread blocked in the search: as
        // with the reload collector, `ReaderControl::kill` busy-waits on this
        // counter, so the work happens on a second, uncounted thread.
        components_to_stop.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let (tx_rows, rx_rows) = std::sync::mpsc::sync_channel::<Vec<SelectItem>>(SEARCH_QUEUE);
            let (tx_stop, rx_stop) = std::sync::mpsc::channel::<Box<dyn Fn() + Send + Sync>>();

            std::thread::spawn(move || {
                let mut searching = (search.lock().expect("search closure poisoned"))(&query, mode);
                // Handed over before the first row is read, so the thread
                // below can call it off while this one is still blocked.
                if tx_stop.send(searching.stop).is_err() {
                    return;
                }
                let mut batch = Vec::with_capacity(SEARCH_BATCH);
                for row in searching.rows.by_ref() {
                    batch.push(row);
                    if batch.len() < SEARCH_BATCH {
                        continue;
                    }
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(SEARCH_BATCH));
                    if tx_rows.send(full).is_err() {
                        return;
                    }
                }
                if !batch.is_empty() {
                    let _ = tx_rows.send(batch);
                }
            });

            let mut stop: Option<Box<dyn Fn() + Send + Sync>> = None;
            loop {
                if stop.is_none() {
                    stop = rx_stop.try_recv().ok();
                }
                if matches!(rx_interrupt.try_recv(), Ok(Some(_)) | Err(_)) {
                    // Blocking waits for it if the search has not handed one
                    // over yet: returning first would leave the search running
                    // with nobody left to call it off.
                    let stop = stop.or_else(|| rx_stop.recv().ok());
                    if let Some(stop) = stop {
                        stop();
                    }
                    break;
                }
                match rx_rows.recv_timeout(SEARCH_POLL) {
                    Ok(rows) => {
                        if tx_item
                            .send(rows.into_iter().map(into_skim).collect())
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    // The search is finished, or it panicked.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // Dropping this is what stops skim's spinner, so it has to happen
            // before the count drops.
            drop(tx_item);
            components_to_stop.fetch_sub(1, Ordering::SeqCst);
        });

        (rx_item, tx_interrupt)
    }
}

/// Select zero or more rows from a list the *query* produces, rather than one
/// it filters.
///
/// The search runs again on every keystroke and its rows replace the list. For
/// a set too large to hold — every line of every note — where something else
/// already knows how to search it.
///
/// Fuzzy matching is off while this is open, since the query has gone to the
/// search instead; skim's own `ctrl-q` toggles back to filtering what is on
/// screen.
pub fn select_many_searching(
    search: Search,
    prompt: &str,
    modes: &'static [Mode],
    cfg: &SelectorConfig,
) -> Result<Vec<String>> {
    let run = Run {
        prompt,
        multi: true,
        query: "",
        reload: None,
        search: Some(search),
        modes,
        views: Views::NONE,
        actions: &[],
    };
    Ok(run_selector(Feed::none(true), run, cfg)?.values)
}

/// What [`select_one_or_query`] came back with.
pub enum Choice {
    /// A row the user selected.
    Item(String),
    /// What the user typed, when it matched no row.
    Query(String),
}

/// Select one item, or accept what the user typed when nothing matched — for
/// lists that are suggestions rather than the whole truth. An empty query with
/// no selection is a cancel, not an empty answer.
pub fn select_one_or_query(
    items: Vec<SelectItem>,
    prompt: &str,
    cfg: &SelectorConfig,
) -> Result<Choice> {
    let (values, query) = run_with_query(Feed::batch(items), prompt, false, cfg)?;
    if let Some(value) = values.into_iter().next() {
        return Ok(Choice::Item(value));
    }
    match query.trim() {
        "" => Err(Cancelled.into()),
        typed => Ok(Choice::Query(typed.to_string())),
    }
}

/// Select zero or more items, returning their values. An empty result means the
/// user selected nothing; cancelling still yields [`Cancelled`].
pub fn select_many(
    items: Vec<SelectItem>,
    prompt: &str,
    cfg: &SelectorConfig,
) -> Result<Vec<String>> {
    run(Feed::batch(items), prompt, true, cfg)
}

/// [`select_many`], with one key stepping through `views`, each drawn by the
/// rows `rows` produces for it.
///
/// `items` is the first view, already built — the selector draws it without
/// waiting for a reload. The key and what it steps through are named in the
/// header for as long as the selector is open.
pub fn select_many_viewing(
    items: Vec<SelectItem>,
    prompt: &str,
    cfg: &SelectorConfig,
    views: Views,
    rows: Reload,
) -> Result<Vec<String>> {
    let run = Run {
        reload: Some(rows),
        views,
        ..Run::new(prompt, true)
    };
    Ok(run_selector(Feed::batch(items), run, cfg)?.values)
}

/// [`select_one`] over rows that arrive as they are found. No row exists when
/// the pane is configured, so whether previews are offered is stated up front
/// in `preview`.
pub fn select_one_streamed(
    items: impl IntoIterator<Item = SelectItem, IntoIter: Send + 'static>,
    prompt: &str,
    preview: bool,
    cfg: &SelectorConfig,
) -> Result<String> {
    one(Feed::stream(items, preview), prompt, cfg)
}

/// [`select_many`] over rows that arrive as they are found. See
/// [`select_one_streamed`].
pub fn select_many_streamed(
    items: impl IntoIterator<Item = SelectItem, IntoIter: Send + 'static>,
    prompt: &str,
    preview: bool,
    cfg: &SelectorConfig,
) -> Result<Vec<String>> {
    run(Feed::stream(items, preview), prompt, true, cfg)
}

fn one(feed: Feed, prompt: &str, cfg: &SelectorConfig) -> Result<String> {
    run(feed, prompt, false, cfg)?
        .into_iter()
        .next()
        .ok_or_else(|| Cancelled.into())
}

/// Rows to feed skim, and whether any of them can fill the preview pane.
struct Feed {
    rows: Rows,
    preview: bool,
}

enum Rows {
    /// Every row known before the selector opens.
    Batch(Vec<SelectItem>),
    /// Rows produced over time, drained on a background thread.
    Stream(Box<dyn Iterator<Item = SelectItem> + Send>),
}

/// How many rows to accumulate before handing a batch to skim, and how long to
/// let one sit unsent. The interval is under a frame, so batching costs nothing
/// visible.
const FEED_BATCH: usize = 512;
const FEED_INTERVAL: Duration = Duration::from_millis(15);

impl Feed {
    /// No rows of its own, for a selector fed by something other than a feed.
    /// `preview` states up front whether a pane is wanted, since there is no
    /// row yet to ask.
    fn none(preview: bool) -> Self {
        Self {
            rows: Rows::Batch(Vec::new()),
            preview,
        }
    }

    fn batch(items: Vec<SelectItem>) -> Self {
        let preview = items.iter().any(|item| item.preview.is_some());
        Self {
            rows: Rows::Batch(items),
            preview,
        }
    }

    fn stream(
        items: impl IntoIterator<Item = SelectItem, IntoIter: Send + 'static>,
        preview: bool,
    ) -> Self {
        Self {
            rows: Rows::Stream(Box::new(items.into_iter())),
            preview,
        }
    }

    /// Hand the rows to `tx`, closing it when they run out — that is how skim
    /// learns the source is exhausted and stops showing itself as still
    /// reading.
    fn send(self, tx: SkimItemSender) -> Result<()> {
        match self.rows {
            Rows::Batch(items) => {
                let batch: Vec<Arc<dyn SkimItem>> = items.into_iter().map(into_skim).collect();
                tx.send(batch)
                    .map_err(|e| anyhow!("feeding selector: {e}"))?;
            }
            Rows::Stream(items) => {
                // Detached: `Skim::run_with` has returned by the time the walk
                // notices the selector is gone, and there is nothing to join for.
                std::thread::spawn(move || {
                    let mut batch: Vec<Arc<dyn SkimItem>> = Vec::with_capacity(FEED_BATCH);
                    let mut flushed = Instant::now();
                    for item in items {
                        batch.push(into_skim(item));
                        if batch.len() < FEED_BATCH && flushed.elapsed() < FEED_INTERVAL {
                            continue;
                        }
                        let full = std::mem::replace(&mut batch, Vec::with_capacity(FEED_BATCH));
                        // A closed channel means the selector is gone; stop
                        // walking rather than finish the tree for nobody.
                        if tx.send(full).is_err() {
                            return;
                        }
                        flushed = Instant::now();
                    }
                    if !batch.is_empty() {
                        let _ = tx.send(batch);
                    }
                });
            }
        }
        Ok(())
    }
}

fn into_skim(item: SelectItem) -> Arc<dyn SkimItem> {
    Arc::new(SkItem { item }) as Arc<dyn SkimItem>
}

/// Everything about one run of the selector except its rows.
struct Run<'a> {
    prompt: &'a str,
    /// Whether several rows can be selected.
    multi: bool,
    /// Text the input starts with.
    query: &'a str,
    /// Given one, [`REFRESH_KEY`] reloads the list through it.
    reload: Option<Reload>,
    /// Given one, the query box drives this instead of filtering rows.
    search: Option<Search>,
    /// The ways `search` can read the query, each on a key of its own. The
    /// first is what the selector opens in.
    modes: &'static [Mode],
    /// Lists the selector steps through on one key, fed by [`Run::reload`],
    /// which is asked for the view by index.
    views: Views,
    /// Keys that close the selector meaning something other than enter.
    actions: &'static [Action],
}

impl<'a> Run<'a> {
    fn new(prompt: &'a str, multi: bool) -> Self {
        Self {
            prompt,
            multi,
            query: "",
            reload: None,
            search: None,
            modes: &[],
            views: Views::NONE,
            actions: &[],
        }
    }
}

/// What one run of the selector came back with.
struct Outcome {
    values: Vec<String>,
    /// What the user had typed — the answer itself when the list is a set of
    /// suggestions and nothing matched.
    query: String,
    /// The [`Action`] key it closed on, if it was not enter.
    acted: Option<&'static str>,
}

/// Drive skim over `feed` and return the selected values.
fn run(feed: Feed, prompt: &str, multi: bool, cfg: &SelectorConfig) -> Result<Vec<String>> {
    run_with_query(feed, prompt, multi, cfg).map(|(values, _)| values)
}

/// [`run`], also returning what the user had typed.
fn run_with_query(
    feed: Feed,
    prompt: &str,
    multi: bool,
    cfg: &SelectorConfig,
) -> Result<(Vec<String>, String)> {
    let out = run_selector(feed, Run::new(prompt, multi), cfg)?;
    Ok((out.values, out.query))
}

/// Drive skim over `feed`.
fn run_selector(feed: Feed, run: Run, cfg: &SelectorConfig) -> Result<Outcome> {
    let (prompt, multi, actions) = (run.prompt, run.multi, run.actions);
    // A clearer message than skim's raw "Device not configured". Command
    // substitution still has a tty on stdin/stderr, so it is allowed.
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() && !std::io::stderr().is_terminal() {
        anyhow::bail!("interactive selection needs a terminal");
    }

    // skim does not stop when its input ends, so a selector whose terminal has
    // gone spins at 100% CPU until something kills the process.
    let _watch = crate::term::watch_for_hangup();

    // Fixed here rather than threaded through every row: see [`PREVIEW_THEME`].
    let _ = PREVIEW_THEME.set(cfg.preview_theme.clone());

    let mut builder = SkimOptionsBuilder::default();
    builder
        .height(cfg.height.clone())
        .prompt(format!("{prompt}> "))
        .reverse(true)
        .multi(multi);

    // The per-item `preview()` supplies the content, so an empty command
    // string is all it takes to turn the pane on.
    let previewing = cfg.preview && feed.preview;
    if previewing {
        builder
            .preview("")
            .preview_window(cfg.preview_window.as_str())
            // What clears the pane when the query stops matching anything.
            //
            // skim runs a row's own `preview()` only when a row is highlighted,
            // and does nothing at all when none is — so the last match's pane
            // stays on screen after the match itself is gone, describing a note
            // no longer in the list. This callback is the branch skim takes
            // instead in exactly that case, and returning nothing from it is
            // what empties the pane.
            .preview_fn(PreviewCallback::from(|_: Vec<Arc<dyn SkimItem>>| {
                Vec::<String>::new()
            }));
    }

    if !run.query.is_empty() {
        builder.query(run.query.to_string());
    }

    // A selector whose reload exists to step through views has no refresh of
    // its own: there is nothing behind the list to fetch again.
    let reloadable = run.reload.is_some() && run.views.is_empty();
    let searching = run.search.is_some();
    let header = hints(actions, reloadable, multi, run.modes, &run.views);

    // The query drives the search rather than filtering rows, so skim runs it
    // through a "command" — `{q}` being skim's own spelling of what was typed.
    // It arrives shell-quoted; `unquote` is what takes that back off.
    if let Some(search) = run.search {
        let collector = SearchCollector {
            search: Arc::new(Mutex::new(search)),
            mode: Arc::new(AtomicUsize::new(0)),
        };
        builder
            .interactive(true)
            .cmd("{q}".to_string())
            .cmd_prompt(format!("{prompt}> "))
            .cmd_collector(Rc::new(RefCell::new(collector)) as Rc<RefCell<dyn CommandCollector>>);
    }

    // `no_clear_if_empty` keeps a quick reload from flickering; a slow one
    // still empties the list, which is what the busy header is for.
    if let Some(reload) = run.reload {
        let collector = ReloadCollector {
            reload: Arc::new(Mutex::new(reload)),
            view: Arc::new(AtomicUsize::new(0)),
            views: run.views.labels.len(),
        };
        // `no_clear_if_empty` keeps a refresh from flickering, but a view is
        // asked for: one that holds nothing has to be seen to hold nothing.
        builder
            .no_clear_if_empty(run.views.is_empty())
            .cmd_collector(Rc::new(RefCell::new(collector)) as Rc<RefCell<dyn CommandCollector>>);
    }

    // A searching selector opens in its first mode, and the header says so
    // from the outset rather than only after a key is pressed.
    let opening = match run.modes.first() {
        Some(_) => chosen_header(run.modes, 0, &header, " match"),
        None => header.clone(),
    };
    if !opening.is_empty() {
        builder.header(opening);
    }
    let mut binds = binds(
        actions, reloadable, previewing, run.modes, &run.views, &header,
    );
    binds.insert(0, CLEAR_SCREEN_KEY.to_string());
    builder.bind(binds);

    let options = builder
        .build()
        .map_err(|e| anyhow!("configuring selector: {e}"))?;

    // Nothing is fed to a searching selector: its rows come from the
    // collector, which skim only reaches for when there is no source.
    let source = if searching {
        None
    } else {
        let (tx, rx): (SkimItemSender, SkimItemReceiver) = unbounded();
        feed.send(tx)?;
        Some(rx)
    };

    let _room = room_for(&cfg.height);
    // Everything from here until skim returns is the user's time, not cid's,
    // and `cid stats` says so.
    let output = {
        let _waiting = crate::stats::interacting();
        Skim::run_with(options, source).map_err(|e| anyhow!("running selector: {e}"))?
    };

    if output.is_abort {
        return Err(Cancelled.into());
    }
    Ok(Outcome {
        values: output
            .selected_items
            .iter()
            .map(|item| item.output().to_string())
            .collect(),
        acted: acted(&output, actions),
        query: output.query,
    })
}

/// Which [`Action`] the selector closed on, if it was not enter.
///
/// Only the key's code and modifiers are compared. skim reports the whole
/// event, which also carries how it was produced — press or release, and the
/// terminal's own state — and a key parsed from a string cannot know either;
/// they differ between terminals that speak a modern key encoding and those
/// that do not.
fn acted(output: &SkimOutput, actions: &'static [Action]) -> Option<&'static str> {
    actions
        .iter()
        .find(|action| {
            skim::binds::parse_key(action.key).is_ok_and(|want| {
                want.code == output.final_key.code && want.modifiers == output.final_key.modifiers
            })
        })
        .map(|action| action.key)
}

/// The header: what this selector can do that another one cannot.
///
/// [`PREVIEW_KEY`] is deliberately absent. It does the same thing in every
/// selector that has a pane, and a header is one line competing with a preview
/// pane for the width — four hints already ran off the end of a narrow one. It
/// is in `cid --help` instead, where something true everywhere belongs.
///
/// Nothing at all when there is nothing to say, so a plain list of paths keeps
/// the row for a path.
fn hints(
    actions: &[Action],
    reloadable: bool,
    multi: bool,
    modes: &[Mode],
    views: &Views,
) -> String {
    let mut hints: Vec<String> = actions
        .iter()
        .map(|action| format!("{} {}", action.key, action.label))
        .collect();
    if reloadable {
        hints.push(format!("{REFRESH_KEY} refresh"));
    }
    if multi {
        hints.push("tab select".to_string());
    }
    // The key and what it steps through, since the header cannot say which of
    // them is in force: what the list holds does that.
    if !views.is_empty() {
        hints.push(format!("{} {}", views.key, views.labels.join("/")));
    }
    // Worth a hint only where the query is not doing what a query usually
    // does: with the search taking it, `ctrl-q` is how you filter what came
    // back rather than searching again.
    if !modes.is_empty() {
        hints.push("ctrl-q filter".to_string());
    }
    hints.join(HINT_SEPARATOR)
}

/// Between hints. Spaces alone read as one run-on line at a glance.
const HINT_SEPARATOR: &str = " · ";

/// Every skim binding this run needs: one `accept` per [`Action`], the preview
/// toggle where there is a pane, and the reload pair where there is a reload.
///
/// `header` is what the reload puts back when it finishes, so the hints the
/// selector opened with survive a refresh.
fn binds(
    actions: &[Action],
    reloadable: bool,
    previewing: bool,
    modes: &[Mode],
    views: &Views,
    header: &str,
) -> Vec<String> {
    let mut binds: Vec<String> = actions
        .iter()
        .map(|action| format!("{}:accept", action.key))
        .collect();
    if previewing {
        binds.push(format!("{PREVIEW_KEY}:toggle-preview"));
    }
    if reloadable {
        binds.extend(refresh_binds(header));
    }
    // Each mode key searches again in its own mode and says so in the header,
    // which is the only place the current one can be read. The query goes with
    // it: a mode key is a second opinion on what was already typed, and asking
    // for one should not cost typing it again.
    for (index, mode) in modes.iter().enumerate() {
        binds.push(format!(
            "{}:reload({MODE_MARKER}{index}{MODE_MARKER}{{q}})+set-header({})",
            mode.key,
            chosen_header(modes, index, header, " match"),
        ));
    }
    // One key for every view, stepping through them: what it reloads says only
    // "the next one", since a binding cannot alternate. The query is left out
    // of it — the query filters what comes back rather than fetching it.
    if !views.is_empty() {
        binds.push(format!("{}:reload({NEXT_VIEW})", views.key));
    }
    binds
}

/// The header while `chosen` is the one in force: what the selector could
/// already do, with the mode or view named at the front of it and the keys to
/// the others after it. `noun` is what the chosen one is called — a mode reads
/// as "fuzzy match", a view as its own name.
fn chosen_header(modes: &[Mode], chosen: usize, header: &str, noun: &str) -> String {
    let mut parts = vec![match modes.get(chosen) {
        Some(mode) => format!("{}{noun}", mode.label),
        None => String::new(),
    }];
    for (index, mode) in modes.iter().enumerate() {
        if index != chosen {
            parts.push(format!("{} {}", mode.key, mode.label));
        }
    }
    if !header.is_empty() {
        parts.push(header.to_string());
    }
    parts.join(HINT_SEPARATOR)
}

/// The row an inline selector opens on, so it never draws over the prompt.
/// A full-screen selector takes the alternate screen and needs no row.
fn room_for(height: &str) -> term::ScratchRow {
    if draws_inline(height) {
        term::ScratchRow::take()
    } else {
        term::ScratchRow::none()
    }
}

/// Whether skim will draw over the terminal it was launched in, rather than
/// taking the alternate screen.
///
/// Mirrors skim's own rule: `100%` is full-screen, every other height — a
/// percentage, a row count, or a negative offset — is an inline viewport.
fn draws_inline(height: &str) -> bool {
    height
        .trim()
        .strip_suffix('%')
        .and_then(|n| n.trim().parse::<u16>().ok())
        != Some(100)
}

/// The header while a reload is in flight. skim empties the list for the
/// duration, so this is what distinguishes fetching from "there are none".
const BUSY_HEADER: &str = "⟳ refreshing…";

/// The skim bindings that turn [`REFRESH_KEY`] into a reload of the item list.
/// The second is skim's `load` event, which puts `idle` back when a read
/// finishes.
fn refresh_binds(idle: &str) -> Vec<String> {
    vec![
        format!("{REFRESH_KEY}:reload+set-header({BUSY_HEADER})"),
        format!("load:set-header({idle})"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// skim hands a searching collector the query already shell-quoted, as
    /// though a shell were going to run it. Nothing here runs one, so what
    /// `quote` puts on has to come back off — exactly, or a search for a word
    /// with an apostrophe in it looks for `'\\''` instead.
    #[test]
    fn what_the_selector_quotes_the_search_unquotes() {
        for typed in [
            "plain",
            "two words",
            "it's",
            "'quoted'",
            "a'b'c",
            "",
            "--flag",
        ] {
            assert_eq!(unquote(&quote(typed)), typed, "{typed:?}");
        }
    }

    /// Nothing quoted it, so nothing should be taken off.
    #[test]
    fn an_unquoted_query_is_left_alone() {
        assert_eq!(unquote("bare"), "bare");
    }

    /// The searching collector is the one piece of this that no `tests/cli.rs`
    /// case can reach: driving it for real needs a terminal, which is what a
    /// skim test would be testing. These drive it directly instead.
    mod searching {
        use super::*;
        use std::sync::atomic::AtomicBool;

        /// Everything the collector is asked to do, run to completion.
        fn collect(search: Search, quoted: &str) -> (Vec<String>, Arc<AtomicUsize>) {
            let mut collector = SearchCollector {
                search: Arc::new(Mutex::new(search)),
                mode: Arc::new(AtomicUsize::new(0)),
            };
            let counter = Arc::new(AtomicUsize::new(0));
            let (rx, _interrupt) = collector.invoke(quoted, Arc::clone(&counter));
            let mut rows = Vec::new();
            while let Ok(batch) = rx.recv() {
                rows.extend(batch.iter().map(|item| item.text().to_string()));
            }
            (rows, counter)
        }

        /// A mode key reloads with a marker in front of the query; an
        /// ordinary keystroke reloads with the query alone. Telling them apart
        /// is the whole of how the mode survives typing.
        #[test]
        fn a_mode_marker_is_read_off_the_front_and_a_plain_query_is_left_alone() {
            assert_eq!(read_mode("hello"), (None, "hello".to_string()));
            assert_eq!(
                read_mode(&format!("{MODE_MARKER}1{MODE_MARKER}hello")),
                (Some(1), "hello".to_string())
            );
            // A marker with no closing one is not a marker.
            assert_eq!(
                read_mode(&format!("{MODE_MARKER}hello")),
                (None, format!("{MODE_MARKER}hello"))
            );
        }

        /// The mode is only ever named by a mode key. Every keystroke after one
        /// re-runs the template, which carries no marker, so the collector has
        /// to remember.
        #[test]
        fn a_mode_survives_the_keystrokes_that_follow_it() {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let recorder = Arc::clone(&seen);
            let search: Search = Box::new(move |_, mode| {
                recorder.lock().unwrap().push(mode);
                Searching {
                    rows: Box::new(std::iter::empty()),
                    stop: Box::new(|| {}),
                }
            });
            let mut collector = SearchCollector {
                search: Arc::new(Mutex::new(search)),
                mode: Arc::new(AtomicUsize::new(0)),
            };
            let drain = |collector: &mut SearchCollector, cmd: &str| {
                let (rx, _i) = collector.invoke(cmd, Arc::new(AtomicUsize::new(0)));
                while rx.recv().is_ok() {}
            };

            // Exactly the shape skim builds: the marker is part of the
            // template and only the query is quoted.
            drain(&mut collector, "'a'");
            drain(
                &mut collector,
                &format!("{MODE_MARKER}1{MODE_MARKER}{}", quote("ab")),
            );
            drain(&mut collector, "'abc'");

            assert_eq!(*seen.lock().unwrap(), vec![0, 1, 1]);
        }

        /// skim quotes the query it substitutes for `{q}` and nothing else, so
        /// a mode key's marker arrives outside the quotes. Reading it off
        /// after unquoting would leave the query still wearing them.
        #[test]
        fn a_mode_key_still_delivers_the_query_unquoted() {
            let seen = Arc::new(Mutex::new(String::new()));
            let recorder = Arc::clone(&seen);
            let search: Search = Box::new(move |query, _| {
                *recorder.lock().unwrap() = query.to_string();
                Searching {
                    rows: Box::new(std::iter::empty()),
                    stop: Box::new(|| {}),
                }
            });

            collect(
                search,
                &format!("{MODE_MARKER}1{MODE_MARKER}{}", quote("it's here")),
            );

            assert_eq!(seen.lock().unwrap().as_str(), "it's here");
        }

        /// The header is the only place the mode can be read, so a mode key
        /// names the mode it just chose and offers the other one.
        #[test]
        fn a_mode_key_says_which_mode_it_put_the_selector_in() {
            let modes = [Mode::new("ctrl-f", "fuzzy"), Mode::new("ctrl-x", "exact")];
            let header = chosen_header(&modes, 0, "tab select", " match");
            assert!(header.starts_with("fuzzy match"), "{header}");
            assert!(header.contains("ctrl-x exact"), "{header}");
            assert!(!header.contains("ctrl-f"), "{header}");
            assert!(header.contains("tab select"), "{header}");
        }

        /// One key steps through the views, so the binding cannot name which
        /// one it lands in — it says only "the next".
        #[test]
        fn one_key_steps_through_the_views_and_asks_for_no_search() {
            let views = Views::new("ctrl-t", &["all", "uncloned"]);
            let binds = binds(&[], false, false, &[], &views, "tab select");

            assert_eq!(binds, vec![format!("ctrl-t:reload({NEXT_VIEW})")]);
            // No `{q}`: the query filters what comes back rather than fetching
            // it, so a view reload carries none.
            assert!(
                binds.iter().all(|b| !b.contains("{q}")),
                "a view asked for a search: {binds:?}"
            );

            // The header cannot say which view is in force, so it says what the
            // key does with them.
            let header = hints(&[], false, true, &[], &views);
            assert!(header.contains("ctrl-t all/uncloned"), "{header}");
            assert!(header.contains("tab select"), "{header}");
        }

        #[test]
        fn every_mode_gets_a_key_that_searches_again_and_sets_the_header() {
            let modes = [Mode::new("ctrl-f", "fuzzy"), Mode::new("ctrl-x", "exact")];
            let binds = binds(&[], false, false, &modes, &Views::NONE, "");
            assert!(
                binds.iter().any(|b| b.starts_with("ctrl-f:reload(")),
                "{binds:?}"
            );
            assert!(
                binds.iter().any(|b| b.starts_with("ctrl-x:reload(")),
                "{binds:?}"
            );
            assert!(
                binds.iter().all(|b| b.contains("set-header(")),
                "a mode key that does not say so: {binds:?}"
            );
        }

        /// A mode key asks for a second opinion on what was already typed, so
        /// the query is handed to the new mode rather than thrown away.
        #[test]
        fn a_mode_key_carries_the_query_into_the_new_mode() {
            let modes = [Mode::new("ctrl-f", "fuzzy"), Mode::new("ctrl-x", "exact")];
            let binds = binds(&[], false, false, &modes, &Views::NONE, "");
            assert!(
                binds.iter().all(|b| b.contains("{q}")),
                "a mode key dropped the query: {binds:?}"
            );
            assert!(
                binds.iter().all(|b| !b.contains("set-query")),
                "a mode key rewrote the query box: {binds:?}"
            );
        }

        #[test]
        fn the_search_sees_what_was_typed_rather_than_what_a_shell_would_get() {
            let seen = Arc::new(Mutex::new(String::new()));
            let recorder = Arc::clone(&seen);
            let search: Search = Box::new(move |query, _mode| {
                *recorder.lock().unwrap() = query.to_string();
                Searching {
                    rows: Box::new(std::iter::empty()),
                    stop: Box::new(|| {}),
                }
            });

            collect(search, &quote("it's a match"));

            assert_eq!(seen.lock().unwrap().as_str(), "it's a match");
        }

        #[test]
        fn every_row_the_search_produces_reaches_the_selector() {
            let search: Search = Box::new(|_, _| Searching {
                rows: Box::new((0..1000).map(|n| SelectItem::plain(format!("row {n}")))),
                stop: Box::new(|| {}),
            });

            let (rows, counter) = collect(search, "'q'");

            assert_eq!(rows.len(), 1000);
            assert_eq!(rows[0], "row 0");
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "a finished search left its thread counted, which is what skim busy-waits on",
            );
        }

        /// The reason `stop` exists. skim kills the reader on every keystroke,
        /// and `ReaderControl::kill` busy-waits on the counter — so a search
        /// still blocked producing rows has to be called off, not waited for.
        #[test]
        fn interrupting_a_search_stops_it_even_while_it_is_blocked() {
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stopped);
            let search: Search = Box::new(move |_, _| {
                let ended = Arc::new(AtomicBool::new(false));
                let watched = Arc::clone(&ended);
                let flag = Arc::clone(&flag);
                Searching {
                    // Never yields until it is called off, the way a ripgrep
                    // over a large vault does not.
                    rows: Box::new(std::iter::from_fn(move || {
                        while !watched.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        None
                    })),
                    stop: Box::new(move || {
                        flag.store(true, Ordering::SeqCst);
                        ended.store(true, Ordering::SeqCst);
                    }),
                }
            });

            let mut collector = SearchCollector {
                search: Arc::new(Mutex::new(search)),
                mode: Arc::new(AtomicUsize::new(0)),
            };
            let counter = Arc::new(AtomicUsize::new(0));
            let (_rx, interrupt) = collector.invoke("'q'", Arc::clone(&counter));
            interrupt.send(1).expect("the collector is listening");

            let deadline = Instant::now() + Duration::from_secs(5);
            while counter.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }

            assert!(
                stopped.load(Ordering::SeqCst),
                "the search was not called off"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "skim would busy-wait here forever",
            );
        }
    }

    /// Render an item the way skim would, with `matches` standing in for a
    /// query that hit the given characters of the *label*.
    fn rendered(item: SelectItem, matches: Vec<usize>) -> Line<'static> {
        let sk = SkItem { item };
        let context = DisplayContext {
            score: 0,
            matches: Matches::CharIndices(matches),
            container_width: 80,
            base_style: Style::default(),
            matched_style: Style::default().fg(Color::Indexed(1)),
        };
        let line = sk.display(context);
        Line::from(
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), s.style))
                .collect::<Vec<_>>(),
        )
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_prefix_is_drawn_dim_before_the_label() {
        let item = SelectItem::plain("git status").prefix("2026-07-30 13:57  ", Vec::new());
        let line = rendered(item, vec![]);
        assert_eq!(text_of(&line), "2026-07-30 13:57  git status");
        assert_eq!(line.spans[0].content, "2026-07-30 13:57  ");
        assert_eq!(line.spans[0].style.fg, Some(Color::Indexed(PREFIX_COLOR)));
    }

    #[test]
    fn highlighting_lands_on_the_label_not_the_prefix() {
        // Character 0 of the label: the `g` of `git`.
        let matched = Style::default().fg(Color::Indexed(1));
        let with = rendered(
            SelectItem::plain("git status").prefix("2026-07-30 13:57  ", Vec::new()),
            vec![0],
        );
        let hit: String = with
            .spans
            .iter()
            .filter(|s| s.style.fg == matched.fg)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hit, "g", "highlight slid off the label: {with:?}");
    }

    #[test]
    fn an_item_without_a_prefix_is_unchanged() {
        let line = rendered(SelectItem::plain("git status"), vec![]);
        assert_eq!(text_of(&line), "git status");
    }

    /// The characters drawn in `color`, wherever the renderer put them.
    fn painted(line: &Line, color: u8) -> String {
        line.spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Indexed(color)))
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn a_tint_colours_its_range_and_nothing_else() {
        let item = SelectItem::plain("✓ billing-api  private").tints(vec![
            Tint {
                range: 0..1,
                color: 2,
            },
            Tint {
                range: 15..22,
                color: 3,
            },
        ]);
        let line = rendered(item, vec![]);
        assert_eq!(text_of(&line), "✓ billing-api  private");
        assert_eq!(painted(&line, 2), "✓");
        assert_eq!(painted(&line, 3), "private");
    }

    /// A tinted column that the query hit is still shown as matched: the
    /// highlight says why the row survived the search, which the colour of the
    /// column it happens to sit in must not paint over.
    #[test]
    fn match_highlighting_survives_a_tint() {
        let item = SelectItem::plain("✓ billing-api  private").tints(vec![Tint {
            range: 15..22,
            color: 3,
        }]);
        // Character 15: the `p` of `private`.
        let line = rendered(item, vec![15]);
        assert_eq!(painted(&line, 1), "p", "{line:?}");
        assert_eq!(painted(&line, 3), "rivate", "{line:?}");
    }

    #[test]
    fn an_item_without_tints_is_unchanged() {
        let line = rendered(SelectItem::plain("git status"), vec![]);
        assert_eq!(line.spans.len(), 1, "{line:?}");
    }

    /// A deferred preview carries nothing until the row is highlighted, and
    /// then previews the row's *value* — a streamed walk yields paths relative
    /// to where it started, which is where the pane's command runs.
    #[test]
    fn a_deferred_preview_builds_its_command_from_the_rows_value() {
        let context = || PreviewContext {
            query: "",
            cmd_query: "",
            width: 80,
            height: 20,
            current_index: 0,
            selected_indices: &[],
            selections: &[],
            current_selection: "",
        };

        let file = SkItem {
            item: SelectItem::new("shown", "src/main.rs").preview(Preview::File),
        };
        let ItemPreview::Command(cmd) = file.preview(context()) else {
            panic!("a file preview must run a command");
        };
        assert!(cmd.contains("'src/main.rs'"), "{cmd}");

        let dir = SkItem {
            item: SelectItem::new("shown", "src/cmd").preview(Preview::Dir),
        };
        let ItemPreview::Command(cmd) = dir.preview(context()) else {
            panic!("a directory preview must run a command");
        };
        assert!(
            cmd.contains("ls -Ap"),
            "a directory is listed, not read: {cmd}"
        );
        assert!(cmd.contains("'src/cmd'"), "{cmd}");
    }

    /// Text that ends on screen is drawn as it was written: a marker on every
    /// pane is a row of the file nobody asked to lose.
    #[test]
    fn a_pane_that_holds_all_of_its_text_is_left_alone() {
        let text = "one\ntwo\nthree".to_string();
        assert_eq!(with_overflow_marker(text.clone(), 20), text);
    }

    /// The pane draws one row fewer than its height, and text of exactly that
    /// length still fits.
    #[test]
    fn text_is_measured_against_the_row_given_up_to_the_border() {
        let five = numbered(5);
        assert_eq!(with_overflow_marker(five.clone(), 6), five);
        assert!(with_overflow_marker(five, 5).ends_with(&overflow_marker()));
    }

    /// What is cut is the end of the text, and the marker is what says so —
    /// which only works from the row the eye stops at.
    #[test]
    fn text_that_runs_long_is_cut_and_ends_in_the_marker() {
        let out = with_overflow_marker(numbered(100), 10);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 9, "the pane drew past its rows: {out}");
        assert_eq!(lines[0], "1");
        assert_eq!(lines[7], "8");
        assert_eq!(lines[8], overflow_marker());
    }

    /// A pane whose height is not known yet — skim asks for a preview before it
    /// has drawn one — says nothing rather than guessing.
    #[test]
    fn text_in_a_pane_of_unknown_height_is_left_alone() {
        let text = "one\ntwo".to_string();
        assert_eq!(with_overflow_marker(text.clone(), 0), text);
    }

    /// The shell filter is the marker for content only the shell has seen, so
    /// it is checked by running it: what it means is awk's to say.
    #[test]
    fn a_long_command_preview_is_cut_and_ends_in_the_marker() {
        let out = filtered("seq 100", "10");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 9, "the pane drew past its rows: {out}");
        assert_eq!(lines[0], "1");
        assert_eq!(lines[7], "8");
        assert!(lines[8].contains(OVERFLOW_HINT), "{out}");
    }

    #[test]
    fn a_command_preview_that_fills_the_pane_exactly_is_unmarked() {
        assert_eq!(filtered("seq 9", "10"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
    }

    #[test]
    fn a_short_command_preview_is_passed_through_unmarked() {
        assert_eq!(filtered("seq 3", "10"), "1\n2\n3\n");
    }

    /// `ROWS` unset is skim before its first draw, the same case as a height of
    /// zero above.
    #[test]
    fn a_command_preview_in_a_pane_of_unknown_height_is_unmarked() {
        assert_eq!(filtered("seq 3", ""), "1\n2\n3\n");
    }

    /// A command that fails has only stderr to explain itself, and the pane is
    /// where that has to land.
    #[test]
    fn a_failing_command_preview_shows_what_it_said() {
        let out = filtered("echo nope >&2; exit 1", "10");
        assert_eq!(out, "nope\n");
    }

    /// skim expands `{...}` in a preview command as one of its own field
    /// placeholders, reading from a `{` to the next `}` — so a pair left open
    /// across another one loses everything between them before the shell sees
    /// the command at all.
    #[test]
    fn a_preview_command_closes_every_brace_before_opening_the_next() {
        let Preview::Command(checkout) = checkout_preview("/tmp/repo") else {
            panic!("a checkout preview must run a command");
        };
        for cmd in [
            with_overflow_filter("seq 100"),
            with_overflow_filter(&file_preview_cmd("src/main.rs")),
            with_overflow_filter(&dir_preview_cmd("src")),
            with_overflow_filter(&checkout),
        ] {
            let mut open = false;
            for c in cmd.chars() {
                match c {
                    '{' => {
                        assert!(!open, "a brace opens inside another: {cmd}");
                        open = true;
                    }
                    '}' => open = false,
                    _ => {}
                }
            }
            assert!(!open, "a brace is left open: {cmd}");
        }
    }

    #[test]
    fn a_json5_file_previews_with_a_syntax_bat_ships() {
        let Preview::Command(line) = line_preview("a.json5", 3) else {
            panic!("a line preview must run a command");
        };
        for cmd in [file_preview_cmd("a.json5"), line] {
            assert!(cmd.contains("--map-syntax='*.json5:JavaScript'"), "{cmd}");
        }
    }

    /// `count` lines reading `1` to `count`.
    fn numbered(count: u32) -> String {
        (1..=count)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Run a preview command through the filter the way skim would.
    fn filtered(cmd: &str, rows: &str) -> String {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(with_overflow_filter(cmd))
            .env("ROWS", rows)
            .output()
            .expect("running a preview command");
        String::from_utf8(out.stdout).expect("preview output is text")
    }

    #[test]
    fn quotes_plain_values() {
        assert_eq!(quote("main"), "'main'");
        assert_eq!(quote("/home/u/my repo"), "'/home/u/my repo'");
    }

    #[test]
    fn only_a_full_height_selector_keeps_off_the_display() {
        assert!(!draws_inline("100%"));
        assert!(!draws_inline(" 100% "));
        for height in ["50%", "99%", "20", "-2", "", "garbage"] {
            assert!(draws_inline(height), "{height:?} does not draw inline");
        }
    }

    #[test]
    fn a_bare_hundred_is_not_full_height() {
        assert!(draws_inline("100"));
    }

    #[test]
    fn a_row_is_taken_only_when_there_is_a_terminal() {
        use std::io::IsTerminal;
        assert_eq!(room_for("50%").is_taken(), std::io::stderr().is_terminal());
    }

    #[test]
    fn a_full_height_selector_takes_no_row() {
        assert!(!room_for("100%").is_taken());
    }

    #[test]
    fn quotes_escape_embedded_single_quotes() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("'; rm -rf /; '"), r"''\''; rm -rf /; '\'''");
    }

    #[test]
    fn the_refresh_key_is_bound_to_a_reload() {
        let binds = refresh_binds("hints");
        let (key, actions) = binds[0]
            .split_once(':')
            .expect("not a `key:action` binding");
        assert_eq!(key, REFRESH_KEY);
        assert!(
            actions.starts_with("reload"),
            "an accept would close the selector: {actions}"
        );
        assert!(actions.contains(BUSY_HEADER), "no busy header: {actions}");
    }

    #[test]
    fn finishing_a_read_puts_the_hints_back() {
        let restore = refresh_binds("f2 open · ctrl-r refresh")
            .into_iter()
            .find(|b| b.starts_with("load:"))
            .expect("nothing puts the header back");
        assert!(restore.contains("f2 open · ctrl-r refresh"), "{restore}");
    }

    /// skim splits bindings on `,` and ends an argument at `)`, and both the
    /// busy header and the hints are written into one.
    #[test]
    fn header_text_cannot_break_the_binding_syntax() {
        let every_hint = hints(
            &[Action::new("f2", "open"), Action::new("f7", "check out")],
            true,
            true,
            &[],
            &Views::new("ctrl-t", &["all", "uncloned"]),
        );
        for header in [BUSY_HEADER, every_hint.as_str(), HINT_SEPARATOR] {
            assert!(!header.contains(','), "{header:?} would split the binding");
            assert!(!header.contains(')'), "{header:?} would end the argument");
        }
    }

    #[test]
    fn the_header_names_every_key_the_selector_answers_to() {
        let full = hints(&[Action::new("f2", "open")], true, true, &[], &Views::NONE);
        assert_eq!(full, "f2 open · ctrl-r refresh · tab select");
    }

    /// A plain list of paths has nothing to say, and the row is worth more as a
    /// path than as an empty hint line.
    #[test]
    fn a_selector_with_nothing_to_offer_draws_no_header() {
        assert!(hints(&[], false, false, &[], &Views::NONE).is_empty());
    }

    #[test]
    fn each_hint_appears_only_when_its_key_is_bound() {
        assert_eq!(hints(&[], false, true, &[], &Views::NONE), "tab select");
        assert_eq!(hints(&[], true, false, &[], &Views::NONE), "ctrl-r refresh");
    }

    /// skim binds `ctrl-r` to rotating the match mode and `tab` to toggling a
    /// selection. Naming either where it does not do what the header says is
    /// worse than saying nothing.
    /// Every hint has to name a key that is actually bound, or the header is a
    /// promise the selector does not keep.
    #[test]
    fn every_key_the_header_names_is_bound() {
        let actions = [Action::new("f2", "open"), Action::new("f7", "check out")];
        let header = hints(&actions, true, true, &[], &Views::NONE);
        let binds = binds(&actions, true, true, &[], &Views::NONE, &header);

        for hint in header.split(HINT_SEPARATOR) {
            let key = hint.split(' ').next().expect("a hint with no key");
            // `tab` is skim's own, and needs no binding from cid.
            if key == "tab" {
                continue;
            }
            assert!(
                binds
                    .iter()
                    .any(|bind| bind.starts_with(&format!("{key}:"))),
                "the header offers {key}, which nothing binds: {binds:?}",
            );
        }
    }

    #[test]
    fn an_action_key_closes_the_selector_and_the_preview_key_does_not() {
        let actions = [Action::new("f2", "open")];
        let binds = binds(&actions, false, true, &[], &Views::NONE, "");
        assert!(binds.contains(&"f2:accept".to_string()), "{binds:?}");
        assert!(
            binds.contains(&format!("{PREVIEW_KEY}:toggle-preview")),
            "{binds:?}",
        );
    }

    /// The key is overridden because skim binds it to something that reads the
    /// cursor position, which a selector whose stdout is a pipe never gets an
    /// answer to. Nothing to override means nothing to work around.
    #[test]
    fn the_key_skim_clears_the_screen_with_is_one_skim_still_binds() {
        let (key, action) = CLEAR_SCREEN_KEY
            .split_once(':')
            .expect("a bind with no action");
        assert_eq!(action, "ignore");
        let parsed = skim::binds::parse_key(key).expect("cid names a key skim cannot parse");
        assert!(
            skim::binds::get_default_key_map().contains_key(&parsed),
            "skim no longer binds {key}: the override can go",
        );
    }

    #[test]
    fn a_plain_selector_binds_nothing_of_its_own() {
        assert!(binds(&[], false, false, &[], &Views::NONE, "").is_empty());
    }

    #[test]
    fn the_action_keys_are_ones_skim_leaves_alone() {
        for key in ["f1", "f2", "f7", PREVIEW_KEY] {
            let parsed = skim::binds::parse_key(key).expect("cid names a key skim cannot parse");
            assert!(
                !skim::binds::get_default_key_map().contains_key(&parsed),
                "{key} already means something in skim",
            );
        }
    }

    #[test]
    fn the_collector_hands_reloaded_rows_to_skim() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut collector = ReloadCollector {
            reload: Arc::new(Mutex::new(Box::new(move |_view| {
                counter.fetch_add(1, Ordering::SeqCst);
                vec![SelectItem::plain("fresh")]
            }))),
            view: Arc::new(AtomicUsize::new(0)),
            views: 2,
        };

        let components = Arc::new(AtomicUsize::new(0));
        let (rx, _interrupt) = collector.invoke("", Arc::clone(&components));

        let batch = rx.recv().expect("the reload sent no rows");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].text(), "fresh");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The closed channel stops skim's spinner; the zeroed counter tells it
        // the collector has stopped. Both have to happen.
        assert!(rx.recv().is_err(), "the source channel was left open");
        while components.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }

    /// The key says only "the next one", so the collector is what knows where
    /// the cycle has got to — and every reload that is not a view key, a
    /// refresh included, has to stay where the user left it.
    #[test]
    fn the_view_key_steps_on_and_every_other_reload_stays_put() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::clone(&seen);
        let mut collector = ReloadCollector {
            reload: Arc::new(Mutex::new(Box::new(move |view| {
                asked.lock().expect("poisoned").push(view);
                vec![SelectItem::plain("row")]
            }))),
            view: Arc::new(AtomicUsize::new(0)),
            views: 2,
        };

        let components = Arc::new(AtomicUsize::new(0));
        for command in ["", NEXT_VIEW, "", NEXT_VIEW, NEXT_VIEW] {
            let (rx, _interrupt) = collector.invoke(command, Arc::clone(&components));
            let _ = rx.recv();
            while rx.recv().is_ok() {}
        }
        while components.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }

        // Round the cycle and back to where it started.
        assert_eq!(*seen.lock().expect("poisoned"), vec![0, 1, 1, 0, 1]);
    }

    #[test]
    fn an_interrupted_reload_stops_without_waiting_for_the_work() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let mut collector = ReloadCollector {
            reload: Arc::new(Mutex::new(Box::new(move |_view| {
                // Stands in for a fetch that has not come back yet.
                let _ = blocked.recv();
                vec![SelectItem::plain("late")]
            }))),
            view: Arc::new(AtomicUsize::new(0)),
            views: 2,
        };

        let components = Arc::new(AtomicUsize::new(0));
        let (_rx, interrupt) = collector.invoke("", Arc::clone(&components));
        interrupt.send(1).expect("interrupt not delivered");

        let start = Instant::now();
        while components.load(Ordering::SeqCst) != 0 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the collector waited for the reload it was told to abandon",
            );
            std::thread::yield_now();
        }
        drop(release);
    }
}
