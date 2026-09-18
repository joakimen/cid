//! Terminal capability checks shared by the printing commands.
//!
//! Colour is decided once, at startup, by [`ColorChoice::resolve`] and carried
//! on [`Ctx`](crate::Ctx). [`Spinner`], [`Board`] and [`ScratchRow`] touch the
//! display only when there is one to touch.

use std::io::{IsTerminal, Write};

use rustix::fd::AsFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// When to colour printed output, as `--color` states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ColorChoice {
    /// Colour when stdout is a terminal and `CID_NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always colour, terminal or not — for a pager (`less -R`) or a recording.
    Always,
    /// Never colour.
    Never,
}

impl ColorChoice {
    /// Whether printed output should carry ANSI colour. An explicit
    /// `always`/`never` outranks `CID_NO_COLOR`, which applies under `auto`.
    pub fn resolve(self, is_tty: bool, no_color: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => is_tty && !no_color,
        }
    }

    /// [`ColorChoice::resolve`] against this process's stdout and environment.
    pub fn for_stdout(self) -> bool {
        self.resolve(std::io::stdout().is_terminal(), no_color())
    }
}

/// The variable that turns cid's colour off: set and non-empty means no
/// colour. Deliberately cid's own rather than the cross-tool `NO_COLOR`
/// (<https://no-color.org>), which cid does not read.
pub fn no_color() -> bool {
    std::env::var_os(NO_COLOR_ENV_VAR).is_some_and(|v| !v.is_empty())
}

/// The environment variable [`no_color`] reads.
pub const NO_COLOR_ENV_VAR: &str = "CID_NO_COLOR";

/// Stdout for a listing, which ends quietly when the reader stops reading.
/// `println!` panics on a closed pipe, so `cid history ls | head` would end
/// in a stack trace where every other command-line tool simply stops.
///
/// Rows are buffered rather than handed straight to the OS: `std::io::Stdout`
/// is line-buffered whatever it is pointed at, so an unbuffered listing costs
/// one `write` syscall per row — half the wall clock of `history ls` over a long
/// fish history. What that buys is paid for by noticing a closed pipe a buffer
/// late, as every other tool does.
pub struct Listing<W: Write> {
    out: W,
    open: bool,
}

impl Listing<std::io::BufWriter<std::io::Stdout>> {
    /// The listing every `ls` command writes: stdout, buffered.
    pub fn stdout() -> Self {
        Self::new(std::io::BufWriter::new(std::io::stdout()))
    }
}

impl<W: Write> Listing<W> {
    /// Wrap a writer.
    pub fn new(out: W) -> Self {
        Self { out, open: true }
    }

    /// Write one line. `Ok(false)` means the reader has closed the pipe: stop
    /// producing rows. Any other write failure is returned as an error.
    pub fn line(&mut self, text: &str) -> std::io::Result<bool> {
        if !self.open {
            return Ok(false);
        }
        match writeln!(self.out, "{text}") {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.open = false;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Push out whatever is still buffered. Call it once the rows run out: a
    /// buffered listing has written nothing yet at that point, and this is the
    /// only place a write failure has anywhere to be reported.
    ///
    /// [`Drop`] flushes as well, so an early return still prints its rows — but
    /// it can only discard the error, which is why this exists.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.flush_open()
    }

    fn flush_open(&mut self) -> std::io::Result<()> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        match self.out.flush() {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        }
    }
}

impl<W: Write> Drop for Listing<W> {
    fn drop(&mut self) {
        let _ = self.flush_open();
    }
}

/// Whether an answer read from a prompt means yes. Only an explicit yes counts.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Ask `question` on stderr, since stdout is a result, and read the answer
/// from stdin. Whether there is anyone to answer is [`Confirm`]'s business.
pub fn confirm(question: &str) -> std::io::Result<bool> {
    // The wait is the user's, so it is not counted against the command that
    // asked. See [`crate::stats::interacting`].
    let _waiting = crate::stats::interacting();
    let mut err = std::io::stderr().lock();
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    drop(err);

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(is_yes(&answer))
}

/// Whether a question can be asked at all. A command that needs confirmation
/// and cannot ask for it should say so and name the flag that skips it.
pub enum Confirm {
    /// There is a terminal on stdin: ask.
    Ask,
    /// `--yes` was given: do not ask.
    Assumed,
    /// stdin is a pipe or a file, and no `--yes`: refuse rather than guess.
    Impossible,
}

impl Confirm {
    /// Decide from the `--yes` flag and whether stdin is a terminal.
    pub fn decide(yes: bool, stdin_is_tty: bool) -> Self {
        match (yes, stdin_is_tty) {
            (true, _) => Self::Assumed,
            (false, true) => Self::Ask,
            (false, false) => Self::Impossible,
        }
    }

    /// [`Confirm::decide`] against this process's stdin.
    pub fn resolve(yes: bool) -> Self {
        Self::decide(yes, std::io::stdin().is_terminal())
    }
}

/// The status cid exits with when its terminal disappears underneath it:
/// 128 + SIGHUP, what the shell would have reported.
pub const EXIT_HANGUP: u8 = 129;

/// How often the terminal is probed while a selector is open.
const HANGUP_POLL: Duration = Duration::from_millis(500);

/// How many probes in a row must fail before the terminal is called gone.
/// More than one, because the consequence is ending the process.
const HANGUP_STRIKES: u32 = 2;

/// Whether a failed probe means the terminal has gone, rather than that the
/// probe itself was interrupted.
pub fn is_hangup(err: rustix::io::Errno) -> bool {
    use rustix::io::Errno;
    matches!(err, Errno::IO | Errno::NXIO | Errno::BADF | Errno::PIPE)
}

/// Ask the terminal whether it is still there. A zero-length write runs the
/// driver's checks without disturbing what the selector has drawn, and unlike
/// a read it cannot take a keystroke skim was going to act on.
fn still_attached(fd: rustix::fd::BorrowedFd<'_>) -> bool {
    match rustix::io::write(fd, &[]) {
        Ok(_) => true,
        Err(e) => !is_hangup(e),
    }
}

/// Ends the process if the terminal goes away while a selector is open.
///
/// skim's input loop does not stop when its event stream ends: on a pty whose
/// other end has closed it spins at 100% CPU indefinitely. `SIGHUP` normally
/// ends cid first; this is for when it does not, such as an orphaned process
/// group. Remove it once skim's loop terminates on its own.
#[must_use]
pub struct HangupWatch {
    stop: Arc<AtomicBool>,
    /// `None` when there was no terminal to watch.
    thread: Option<JoinHandle<()>>,
}

/// Watch the terminal for as long as the returned guard is held.
///
/// Bind it — `let _watch = term::watch_for_hangup()`. A bare statement drops it
/// immediately and watches nothing.
pub fn watch_for_hangup() -> HangupWatch {
    // stderr is what the selector draws on, so it is the terminal that matters.
    watch_for_hangup_on(std::io::stderr().is_terminal())
}

fn watch_for_hangup_on(on_terminal: bool) -> HangupWatch {
    let stop = Arc::new(AtomicBool::new(false));
    if !on_terminal {
        return HangupWatch { stop, thread: None };
    }

    let flag = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let mut strikes = 0;
        while !flag.load(Ordering::Relaxed) {
            std::thread::sleep(HANGUP_POLL);
            if flag.load(Ordering::Relaxed) {
                return;
            }
            if still_attached(std::io::stderr().as_fd()) {
                strikes = 0;
                continue;
            }
            strikes += 1;
            if strikes >= HANGUP_STRIKES {
                // Nothing to unwind: every destructor between here and `main`
                // would be writing to a terminal that is gone.
                std::process::exit(EXIT_HANGUP as i32);
            }
        }
    });
    HangupWatch {
        stop,
        thread: Some(thread),
    }
}

impl Drop for HangupWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Not joined: the thread sleeps in half-second steps, and no selector's
        // exit should wait one out.
        drop(self.thread.take());
    }
}

/// Stands in for a line break in text folded onto one row.
pub const NEWLINE_GLYPH: &str = "⏎";

/// [`NEWLINE_GLYPH`] with the spaces that set it off, written out rather than
/// built per call: [`one_row`] runs once per row of every listing and again on
/// every selector reload. A test holds it to the glyph above.
const NEWLINE_JOINER: &str = " ⏎ ";

/// Text from outside cid, made safe to draw on one row of a terminal.
///
/// Control characters are dropped: a terminal *acts on* what it is sent, so a
/// pull request title carrying `\x1b[32m` could otherwise make a listing say
/// the opposite of what cid found. cid's own colour is applied after this,
/// never before. Newlines fold to [`NEWLINE_GLYPH`] so one entry stays one row,
/// and tabs become a space so columns stay aligned.
pub fn one_row(text: &str) -> String {
    // Folded in place rather than collected and joined: all but a handful of
    // rows are a single line, and that case now allocates once.
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut out = drop_controls(first);
    for line in lines {
        out.push_str(NEWLINE_JOINER);
        out.push_str(&drop_controls(line));
    }
    out
}

/// [`one_row`] for text that is allowed to keep its line breaks — a preview
/// pane, which is a block rather than a row.
pub fn block(text: &str) -> String {
    text.lines()
        .map(drop_controls)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line of foreign text with its control characters removed. Covers the C1
/// controls as well as the ASCII ones: on a terminal in a non-UTF-8 locale,
/// `U+009B` *is* a control sequence introducer.
fn drop_controls(line: &str) -> String {
    line.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

/// The hue secondary text takes: what a row says about itself rather than what
/// it names — a duration, a count, a path, the tool a version came from, where
/// a setting's value came from.
///
/// Colour is the low sixteen indices throughout, which a terminal resolves from
/// the theme its user chose rather than from a fixed table. 0, 7, 8 and 15 are
/// never a foreground here: each is the background in a large share of themes,
/// which leaves the text either invisible or the faintest thing on the row.
/// Emphasis is [`bold`] for the same reason — an attribute reads the same
/// whatever is behind it.
pub const SECONDARY: u8 = 5;

/// Wrap `text` in an ANSI 256-colour sequence when `on`, so the same colour
/// indices the selector uses also drive plain listings.
pub fn paint(text: &str, color: u8, on: bool) -> String {
    style(text, Some(color), false, on)
}

/// Bold `text` when `on`, without giving it a colour.
///
/// What a heading or a field label takes. Bold is an attribute rather than a
/// hue, so it is the one emphasis that reads the same whatever the terminal's
/// theme has painted behind it.
pub fn bold(text: &str, on: bool) -> String {
    style(text, None, true, on)
}

/// `text` under an optional foreground colour and an optional bold attribute,
/// as one sequence — or unchanged when `on` is false.
///
/// Colours are the low sixteen indices, which a terminal resolves from its own
/// theme rather than from a fixed table. Indices outside them are a colour
/// chosen for somebody else's background.
pub fn style(text: &str, color: Option<u8>, bold: bool, on: bool) -> String {
    if !on {
        return text.to_string();
    }
    let mut codes = String::new();
    if bold {
        codes.push_str("\x1b[1m");
    }
    if let Some(color) = color {
        codes.push_str(&format!("\x1b[38;5;{color}m"));
    }
    if codes.is_empty() {
        return text.to_string();
    }
    format!("{codes}{text}\x1b[0m")
}

/// Paint `text`, then return to `back` rather than to the terminal default —
/// for a cell that carries its own colour inside a row that already has one.
/// [`paint`] would end with a reset and leave the rest of the row uncoloured.
pub fn paint_within(text: &str, color: u8, back: u8, on: bool) -> String {
    if on {
        format!("\x1b[38;5;{color}m{text}\x1b[38;5;{back}m")
    } else {
        text.to_string()
    }
}

/// A row of the terminal to draw on that is not the shell's.
///
/// Anything drawn inline starts on the row the cursor is on, which from a key
/// binding is the last row of the prompt — a two-line prompt is left cut in
/// half. Taking a fresh row and stepping back up on the way out leaves the
/// cursor where the shell's repaint expects it.
///
/// Bind it for as long as the drawing lasts — `let _row = ScratchRow::take()`.
/// A bare statement gives the row straight back.
#[must_use]
pub struct ScratchRow {
    /// `false` when there was no terminal to take a row on.
    taken: bool,
}

impl ScratchRow {
    /// Move to a fresh row below the cursor, giving it back when dropped.
    pub fn take() -> Self {
        Self::take_on(std::io::stderr().is_terminal())
    }

    fn take_on(taken: bool) -> Self {
        if taken {
            let mut err = std::io::stderr().lock();
            // An explicit carriage return: whether a bare newline returns to
            // column 0 depends on a terminal mode the shell owns.
            let _ = err.write_all(b"\r\n");
            let _ = err.flush();
        }
        Self { taken }
    }

    /// No row at all, for a caller that has nothing to protect.
    pub fn none() -> Self {
        Self { taken: false }
    }

    /// Whether a row was actually taken — false without a terminal.
    pub fn is_taken(&self) -> bool {
        self.taken
    }
}

impl Drop for ScratchRow {
    fn drop(&mut self) {
        if !self.taken {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(CURSOR_UP);
        let _ = err.flush();
    }
}

/// Move the cursor up one row, staying in its column.
const CURSOR_UP: &[u8] = b"\x1b[A";

/// Frames of the spinner, in order. Braille dots are one cell wide everywhere,
/// so the line never changes width as it turns.
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each frame is held: ten frames make a full turn per second.
const FRAME_TIME: Duration = Duration::from_millis(100);

/// Erase the line the cursor is on and return to its start.
const CLEAR_LINE: &str = "\r\x1b[2K";

/// A one-line "working on it" animation on stderr, erased when dropped.
///
/// Drawn on stderr because stdout is a result. Takes a [`ScratchRow`], since
/// each frame erases the whole row it is drawn on. Nothing is drawn at all
/// when stderr is not a terminal.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    /// What the animation says it is waiting for, which the caller can change
    /// as the wait goes on.
    label: Arc<Mutex<String>>,
    /// `None` when there was no terminal to draw on.
    thread: Option<JoinHandle<()>>,
    /// Dropped after the animation is erased, handing the row back.
    _row: ScratchRow,
}

impl Spinner {
    /// Say what the wait is now for — a page of a listing, a repository being
    /// cloned. Cheap enough to call per step, and a no-op when there was no
    /// terminal to draw on.
    pub fn say(&self, label: impl Into<String>) {
        if let Ok(mut current) = self.label.lock() {
            *current = label.into();
        }
    }
}

/// Start a spinner labelled `label` (e.g. `fetching`), running until it is
/// dropped. `color` is the resolved [`ColorChoice`].
///
/// Bind it to a name — `let _spinner = term::spinner(…)` — for as long as the
/// wait lasts. A bare `term::spinner(…)` statement drops it immediately and
/// spins for nothing.
#[must_use]
pub fn spinner(label: &str, color: bool) -> Spinner {
    spinner_on(label, color, std::io::stderr().is_terminal())
}

fn spinner_on(label: &str, color: bool, on_terminal: bool) -> Spinner {
    let stop = Arc::new(AtomicBool::new(false));
    let label = Arc::new(Mutex::new(label.to_string()));
    if !on_terminal {
        return Spinner {
            stop,
            label,
            thread: None,
            _row: ScratchRow::none(),
        };
    }
    let row = ScratchRow::take_on(true);

    let said = Arc::clone(&label);
    let flag = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let mut frames = FRAMES.iter().cycle();
        while !flag.load(Ordering::Relaxed) {
            let frame = frames.next().unwrap_or(&FRAMES[0]);
            let label = said.lock().map(|l| l.clone()).unwrap_or_default();
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "{CLEAR_LINE}{} {label}", paint(frame, 6, color));
            let _ = err.flush();
            // Woken in fractions of a frame so the erase is not delayed by a
            // whole one after the work finishes.
            for _ in 0..10 {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(FRAME_TIME / 10);
            }
        }
    });
    Spinner {
        stop,
        label,
        thread: Some(thread),
        _row: row,
    }
}

impl Drop for Spinner {
    /// Stop the animation and erase its line before the row it was drawn on
    /// goes back.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Join before erasing, or a frame in flight lands after the erase.
        let _ = thread.join();
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "{CLEAR_LINE}");
        let _ = err.flush();
    }
}

/// What a [`Board`] draws: every row, given the spinner's current frame and
/// the terminal's width in columns. Each row must fit in that width.
pub type Render = dyn Fn(&str, usize) -> Vec<String> + Send + Sync;

/// A block of rows on stderr redrawn in place until dropped, then drawn once
/// more and left behind — a spinner for several things at once.
///
/// The rows are whatever [`Render`] returns on each frame, so the caller keeps
/// its own state and the board only asks for it. Every redraw steps back up
/// over the rows the last one drew, which is why a row must never wrap: the
/// row count it steps over would then be short.
pub struct Board {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    render: Arc<Render>,
    drawn: Arc<Mutex<usize>>,
}

/// Start drawing `render` on stderr, or `None` when stderr is not a terminal —
/// the caller then reports each result as a line of its own.
#[must_use]
pub fn board(render: Arc<Render>) -> Option<Board> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let drawn = Arc::new(Mutex::new(0));

    let flag = Arc::clone(&stop);
    let rows = Arc::clone(&render);
    let count = Arc::clone(&drawn);
    let thread = std::thread::spawn(move || {
        let mut frames = FRAMES.iter().cycle();
        while !flag.load(Ordering::Relaxed) {
            let frame = frames.next().unwrap_or(&FRAMES[0]);
            redraw(&*rows, frame, &count);
            for _ in 0..10 {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(FRAME_TIME / 10);
            }
        }
    });

    Some(Board {
        stop,
        thread: Some(thread),
        render,
        drawn,
    })
}

/// Step back over the rows drawn last time and draw every row afresh.
fn redraw(render: &Render, frame: &str, drawn: &Mutex<usize>) {
    // One column short of the edge: a row that fills the last column leaves
    // some terminals waiting to wrap on the next character.
    let rows = render(frame, columns().saturating_sub(1));
    let Ok(mut drawn) = drawn.lock() else {
        return;
    };
    let mut out = String::new();
    if *drawn > 0 {
        out.push_str(&format!("\x1b[{}A", *drawn));
    }
    for row in &rows {
        out.push_str(CLEAR_LINE);
        out.push_str(row);
        out.push_str("\r\n");
    }
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(out.as_bytes());
    let _ = err.flush();
    *drawn = rows.len();
}

/// The width of the terminal on stderr, or 80 when it will not say.
fn columns() -> usize {
    match rustix::termios::tcgetwinsize(std::io::stderr()) {
        Ok(size) if size.ws_col > 0 => usize::from(size.ws_col),
        _ => 80,
    }
}

impl Drop for Board {
    /// Stop the animation and draw the rows as they finally stand, leaving
    /// them on screen with the cursor below.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Joined before the last draw, or a frame in flight lands after it.
            let _ = thread.join();
        }
        redraw(&*self.render, FRAMES[0], &self.drawn);
    }
}

#[cfg(test)]
mod tests {
    use super::{NEWLINE_GLYPH, block, one_row};

    #[test]
    fn control_characters_never_survive_a_row() {
        let row = one_row("ok\x1b[2K\x1b[1;31mFAKE\x07\x7f");
        assert!(!row.contains('\x1b'), "{row:?}");
        assert_eq!(row, "ok[2K[1;31mFAKE");
    }

    #[test]
    fn c1_controls_are_dropped_as_well() {
        assert_eq!(one_row("a\u{9b}31mb"), "a31mb");
    }

    #[test]
    fn a_newline_folds_into_one_visible_row() {
        let row = one_row("first\nsecond");
        assert!(!row.contains('\n'), "{row:?}");
        assert_eq!(row, format!("first {NEWLINE_GLYPH} second"));
    }

    #[test]
    fn a_tab_becomes_one_space() {
        assert_eq!(one_row("a\tb"), "a b");
    }

    /// The joiner is written out for speed, so nothing but this holds it to the
    /// glyph it is supposed to be showing.
    #[test]
    fn the_written_out_joiner_is_the_glyph_it_claims() {
        assert_eq!(super::NEWLINE_JOINER, format!(" {NEWLINE_GLYPH} "));
    }

    #[test]
    fn several_line_breaks_all_fold() {
        assert_eq!(
            one_row("a\nb\nc"),
            format!("a {NEWLINE_GLYPH} b {NEWLINE_GLYPH} c")
        );
    }

    #[test]
    fn text_with_nothing_wrong_with_it_is_unchanged() {
        for text in ["plain", "æøå — ünïcode", "✓ passed", ""] {
            assert_eq!(one_row(text), text);
        }
    }

    #[test]
    fn a_block_keeps_its_line_breaks_and_nothing_else() {
        assert_eq!(block("a\n\x1b[2Kb"), "a\n[2Kb");
    }

    /// A writer that takes `ok` complete lines and then fails with `kind`,
    /// standing in for a `head` that has read its rows and gone. Counts
    /// newlines, not calls: `writeln!` reaches the writer more than once per
    /// line, and the stub has to fail between rows rather than inside one.
    struct FailsAfter {
        ok: usize,
        kind: std::io::ErrorKind,
        buf: String,
    }

    impl FailsAfter {
        fn new(ok: usize, kind: std::io::ErrorKind) -> Self {
            Self {
                ok,
                kind,
                buf: String::new(),
            }
        }

        fn lines(&self) -> Vec<&str> {
            self.buf.lines().collect()
        }
    }

    impl std::io::Write for FailsAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.buf.matches('\n').count() >= self.ok {
                return Err(std::io::Error::from(self.kind));
            }
            self.buf.push_str(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_reader_that_stops_reading_ends_the_listing_rather_than_failing_it() {
        let mut listing = super::Listing::new(FailsAfter::new(2, std::io::ErrorKind::BrokenPipe));
        assert!(listing.line("one").unwrap());
        assert!(listing.line("two").unwrap());
        assert!(
            !listing.line("three").unwrap(),
            "kept writing past the reader"
        );
        assert!(!listing.line("four").unwrap());
        assert_eq!(listing.out.lines(), vec!["one", "two"]);
    }

    #[test]
    fn other_write_failures_still_fail_the_listing() {
        for kind in [
            std::io::ErrorKind::StorageFull,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let mut listing = super::Listing::new(FailsAfter::new(1, kind));
            assert!(listing.line("one").unwrap());
            assert_eq!(
                listing.line("two").unwrap_err().kind(),
                kind,
                "{kind:?} was treated as the reader going away"
            );
        }
    }

    /// A writer that takes every row and fails only when asked to flush,
    /// standing in for a listing whose buffer reaches the pipe at the end.
    struct FailsOnFlush(std::io::ErrorKind);

    impl std::io::Write for FailsOnFlush {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(self.0))
        }
    }

    /// The rows are buffered, so the write that fails is the flush — and it is
    /// the last chance anything has to say so.
    #[test]
    fn finishing_reports_a_failure_that_only_the_flush_could_find() {
        let mut listing = super::Listing::new(FailsOnFlush(std::io::ErrorKind::StorageFull));
        assert!(listing.line("one").unwrap());
        assert_eq!(
            listing.finish().unwrap_err().kind(),
            std::io::ErrorKind::StorageFull
        );
    }

    #[test]
    fn finishing_stays_quiet_when_the_reader_has_already_gone() {
        let mut listing = super::Listing::new(FailsOnFlush(std::io::ErrorKind::BrokenPipe));
        assert!(listing.line("one").unwrap());
        listing.finish().expect("a closed pipe is not a failure");
    }

    use super::*;

    #[test]
    fn auto_colours_only_a_terminal_without_no_color() {
        assert!(ColorChoice::Auto.resolve(true, false));
        assert!(!ColorChoice::Auto.resolve(false, false), "coloured a pipe");
        assert!(
            !ColorChoice::Auto.resolve(true, true),
            "ignored CID_NO_COLOR"
        );
    }

    #[test]
    fn always_colours_a_pipe_too() {
        assert!(ColorChoice::Always.resolve(false, false));
        assert!(!ColorChoice::Never.resolve(false, false));
    }

    #[test]
    fn the_errors_that_mean_the_terminal_has_gone() {
        use rustix::io::Errno;
        for err in [Errno::IO, Errno::NXIO, Errno::BADF, Errno::PIPE] {
            assert!(is_hangup(err), "{err:?} was not read as a hangup");
        }
    }

    #[test]
    fn an_interrupted_probe_is_not_a_hangup() {
        use rustix::io::Errno;
        for err in [Errno::INTR, Errno::AGAIN, Errno::NOSPC, Errno::PERM] {
            assert!(!is_hangup(err), "{err:?} would have killed a live selector");
        }
    }

    #[test]
    fn no_terminal_means_nothing_is_watched() {
        assert!(
            watch_for_hangup_on(false).thread.is_none(),
            "watched a terminal that was not there"
        );
    }

    #[test]
    fn the_hangup_exit_status_is_the_one_a_signal_would_have_given() {
        assert_eq!(EXIT_HANGUP, 128 + 1);
    }

    /// Pinned because nothing else can catch it: no test in this crate can set
    /// an environment variable, so the name `no_color` reads is invisible to
    /// every other check.
    #[test]
    fn the_variable_is_cids_own_and_not_the_shared_convention() {
        assert_eq!(NO_COLOR_ENV_VAR, "CID_NO_COLOR");
    }

    #[test]
    fn an_explicit_choice_outranks_no_color() {
        assert!(
            ColorChoice::Always.resolve(true, true),
            "CID_NO_COLOR beat --color always"
        );
        assert!(!ColorChoice::Never.resolve(true, false));
    }

    #[test]
    fn only_an_explicit_yes_is_a_yes() {
        for answer in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(is_yes(answer), "{answer:?} was not read as yes");
        }
        for answer in ["", "\n", "n", "no", "ye", "yep", "sure", "1"] {
            assert!(!is_yes(answer), "{answer:?} was read as yes");
        }
    }

    #[test]
    fn the_yes_flag_skips_the_question() {
        assert!(matches!(Confirm::decide(true, true), Confirm::Assumed));
        assert!(matches!(Confirm::decide(true, false), Confirm::Assumed));
    }

    #[test]
    fn a_question_that_cannot_be_asked_is_not_answered_for_the_user() {
        assert!(matches!(Confirm::decide(false, false), Confirm::Impossible));
        assert!(matches!(Confirm::decide(false, true), Confirm::Ask));
    }

    #[test]
    fn paint_is_identity_when_off() {
        assert_eq!(paint("main", 2, false), "main");
    }

    #[test]
    fn paint_wraps_when_on() {
        assert_eq!(paint("main", 2, true), "\x1b[38;5;2mmain\x1b[0m");
    }

    #[test]
    fn bold_is_an_attribute_rather_than_a_colour() {
        assert_eq!(bold("root", true), "\x1b[1mroot\x1b[0m");
        assert_eq!(bold("root", false), "root");
    }

    #[test]
    fn a_style_with_both_writes_both_and_resets_once() {
        assert_eq!(
            style("failed", Some(1), true, true),
            "\x1b[1m\x1b[38;5;1mfailed\x1b[0m"
        );
        // Nothing asked for is nothing written, rather than a bare reset.
        assert_eq!(style("plain", None, false, true), "plain");
    }

    #[test]
    fn paint_within_returns_to_the_row_colour() {
        assert_eq!(
            paint_within("✓", 2, 5, true),
            "\x1b[38;5;2m✓\x1b[38;5;5m",
            "the cell must not end in a reset",
        );
        assert_eq!(paint_within("✓", 2, 5, false), "✓");
    }

    #[test]
    fn spinner_frames_are_a_single_character() {
        for frame in FRAMES {
            assert_eq!(frame.chars().count(), 1, "{frame:?} is not one character");
        }
    }

    #[test]
    fn clearing_erases_the_line_it_returns_to() {
        assert!(CLEAR_LINE.starts_with('\r'));
        assert!(CLEAR_LINE.contains("\x1b[2K"), "no erase-line sequence");
    }

    #[test]
    fn no_terminal_means_no_animation() {
        let spinner = spinner_on("fetching", true, false);
        assert!(spinner.thread.is_none(), "spun without a terminal");
        assert!(!spinner._row.is_taken(), "took a row it could not draw on");
    }

    #[test]
    fn the_spinner_draws_on_a_row_of_its_own() {
        let spinner = spinner_on("fetching", true, true);
        assert_eq!(
            spinner._row.is_taken(),
            spinner.thread.is_some(),
            "the spinner drew somewhere it did not own",
        );
    }

    #[test]
    fn no_terminal_means_no_row_taken() {
        assert!(!ScratchRow::take_on(false).is_taken());
        assert!(!ScratchRow::none().is_taken());
    }

    #[test]
    fn the_row_is_given_back_by_moving_up_one() {
        assert_eq!(CURSOR_UP, b"\x1b[A");
    }
}
