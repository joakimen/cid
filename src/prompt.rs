//! A one-line text prompt with a live preview of what the answer will become.
//!
//! [`Line`] and [`render`] decide everything and are pure; [`ask`] owns the
//! terminal — raw mode, the key reads, the redraws — and nothing else.
//!
//! Drawn on stderr, since stdout is a result, and erased on the way out: what
//! was typed is the caller's to report.

use std::io::{IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, queue, terminal};
use unicode_width::UnicodeWidthStr;

use crate::select::Cancelled;
use crate::term;

/// The text being typed and where the cursor sits in it, in chars.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Line {
    text: String,
    cursor: usize,
}

/// What a key did to the prompt as a whole.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Keep reading keys.
    Edit,
    /// The answer is in: the text, trimmed.
    Submit(String),
    /// Esc, Ctrl-C, or Ctrl-D on an empty line.
    Cancel,
}

impl Line {
    /// A line already holding `text`, the cursor at its end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text before the cursor, which is what places it on screen.
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.byte(self.cursor)]
    }

    /// Apply one key. Emacs bindings, as a shell's own line editor has them.
    ///
    /// Enter on a blank line does nothing: there is no answer to give yet.
    pub fn apply(&mut self, key: KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter if self.text.trim().is_empty() => Step::Edit,
            KeyCode::Enter => Step::Submit(self.text.trim().to_string()),
            KeyCode::Esc => Step::Cancel,
            KeyCode::Char('c') if ctrl => Step::Cancel,
            KeyCode::Char('d') if ctrl && self.text.is_empty() => Step::Cancel,
            KeyCode::Char('d') if ctrl => self.delete_at_cursor(),
            KeyCode::Char('a') if ctrl => self.to(0),
            KeyCode::Char('e') if ctrl => self.to(self.len()),
            KeyCode::Char('b') if ctrl => self.to(self.cursor.saturating_sub(1)),
            KeyCode::Char('f') if ctrl => self.to(self.cursor + 1),
            KeyCode::Char('u') if ctrl => self.cut(0, self.cursor),
            KeyCode::Char('k') if ctrl => self.cut(self.cursor, self.len()),
            KeyCode::Char('w') if ctrl => self.cut(self.word_start(), self.cursor),
            KeyCode::Char(_) if ctrl || key.modifiers.contains(KeyModifiers::ALT) => Step::Edit,
            KeyCode::Char(c) => {
                self.text.insert(self.byte(self.cursor), c);
                self.cursor += 1;
                Step::Edit
            }
            KeyCode::Backspace if self.cursor > 0 => self.cut(self.cursor - 1, self.cursor),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Left => self.to(self.cursor.saturating_sub(1)),
            KeyCode::Right => self.to(self.cursor + 1),
            KeyCode::Home => self.to(0),
            KeyCode::End => self.to(self.len()),
            _ => Step::Edit,
        }
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(at, _)| at)
    }

    fn to(&mut self, chars: usize) -> Step {
        self.cursor = chars.min(self.len());
        Step::Edit
    }

    fn cut(&mut self, from: usize, to: usize) -> Step {
        let (from, to) = (self.byte(from), self.byte(to));
        self.text.replace_range(from..to, "");
        self.cursor = self.text[..from].chars().count();
        Step::Edit
    }

    fn delete_at_cursor(&mut self) -> Step {
        match self.cursor < self.len() {
            true => self.cut(self.cursor, self.cursor + 1),
            false => Step::Edit,
        }
    }

    /// Where Ctrl-W cuts back to: over any spaces behind the cursor, then over
    /// the word before them.
    fn word_start(&self) -> usize {
        let before: Vec<char> = self.text.chars().take(self.cursor).collect();
        let spaces = before
            .iter()
            .rev()
            .take_while(|c| c.is_whitespace())
            .count();
        let word = before
            .iter()
            .rev()
            .skip(spaces)
            .take_while(|c| !c.is_whitespace())
            .count();
        self.cursor - spaces - word
    }
}

/// The glyph between a label and its value, cyan.
const ARROW: &str = "›";
const ARROW_COLOR: u8 = 6;

/// The colour of the previewed answer: green, as a thing about to be made.
const PREVIEW_COLOR: u8 = 2;

/// What one frame of the prompt draws, row by row, and the column the cursor
/// belongs in on the first. The labels are padded to one width so the two
/// values line up.
pub fn render(
    label: &str,
    line: &Line,
    preview_label: &str,
    preview: &str,
    color: bool,
) -> (Vec<String>, usize) {
    let width = label.width().max(preview_label.width());
    let arrow = term::paint(ARROW, ARROW_COLOR, color);
    let head = |text: &str| term::bold(&format!("{text:>width$}"), color);
    let rows = vec![
        format!("{} {arrow} {}", head(label), line.text()),
        format!(
            "{} {arrow} {}",
            head(preview_label),
            term::style(preview, Some(PREVIEW_COLOR), true, color)
        ),
        term::paint(
            &format!("{:width$}   enter to create · esc to cancel", ""),
            term::SECONDARY,
            color,
        ),
    ];
    // `width` columns of label, then ` › `.
    let column = width + 3 + line.before_cursor().width();
    (rows, column)
}

/// Ask for a line of text on the terminal, showing what `preview` makes of it
/// below as it is typed.
///
/// Returns [`Cancelled`] when the user backs out. Fails when stdin or stderr is
/// not a terminal, since there is nobody to ask.
pub fn ask(
    label: &str,
    preview_label: &str,
    preview: impl Fn(&str) -> String,
    color: bool,
) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        anyhow::bail!("no terminal to ask on");
    }
    // The wait is the user's. See [`crate::stats::interacting`].
    let _waiting = crate::stats::interacting();
    let _raw = RawMode::enable()?;
    let mut err = std::io::stderr().lock();
    let mut line = Line::default();

    let answer = loop {
        let (rows, column) = render(label, &line, preview_label, &preview(line.text()), color);
        draw(&mut err, &rows, column)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match line.apply(key) {
            Step::Edit => {}
            Step::Submit(text) => break Ok(text),
            Step::Cancel => break Err(Cancelled.into()),
        }
    };
    queue!(
        err,
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;
    err.flush()?;
    answer
}

/// Redraw the prompt from the row the cursor is on, leaving the cursor on the
/// first row at `column`.
///
/// Raw mode leaves a bare newline moving down without returning to column 0,
/// so every row break is an explicit `\r\n`.
fn draw(out: &mut impl Write, rows: &[String], column: usize) -> std::io::Result<()> {
    queue!(
        out,
        cursor::MoveToColumn(0),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    )?;
    out.write_all(rows.join("\r\n").as_bytes())?;
    if rows.len() > 1 {
        queue!(out, cursor::MoveUp((rows.len() - 1) as u16))?;
    }
    queue!(out, cursor::MoveToColumn(column as u16))?;
    out.flush()
}

/// Raw mode for as long as it is held, restored however the prompt ends.
struct RawMode;

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(text: &str) -> Line {
        let mut line = Line::default();
        for c in text.chars() {
            line.apply(key(KeyCode::Char(c)));
        }
        line
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut line = typed("Migrate AWS");
        for _ in 0..3 {
            line.apply(key(KeyCode::Left));
        }
        for c in "to ".chars() {
            line.apply(key(KeyCode::Char(c)));
        }
        assert_eq!(line.text(), "Migrate to AWS");
        assert_eq!(line.before_cursor(), "Migrate to ");
    }

    #[test]
    fn enter_submits_the_trimmed_text_but_not_a_blank_line() {
        assert_eq!(typed("   ").apply(key(KeyCode::Enter)), Step::Edit);
        assert_eq!(
            typed(" Migrate ").apply(key(KeyCode::Enter)),
            Step::Submit("Migrate".into())
        );
    }

    #[test]
    fn esc_and_ctrl_c_cancel_and_ctrl_d_cancels_only_an_empty_line() {
        assert_eq!(typed("a").apply(key(KeyCode::Esc)), Step::Cancel);
        assert_eq!(typed("a").apply(ctrl('c')), Step::Cancel);
        assert_eq!(typed("").apply(ctrl('d')), Step::Cancel);
        assert_eq!(typed("a").apply(ctrl('d')), Step::Edit);
    }

    #[test]
    fn backspace_removes_a_whole_character_whatever_its_width_in_bytes() {
        let mut line = typed("Møte");
        line.apply(key(KeyCode::Left));
        line.apply(key(KeyCode::Left));
        line.apply(key(KeyCode::Backspace));
        assert_eq!(line.text(), "Mte");
    }

    #[test]
    fn ctrl_w_cuts_the_word_behind_the_cursor_and_the_spaces_after_it() {
        let mut line = typed("Migrate to  ");
        line.apply(ctrl('w'));
        assert_eq!(line.text(), "Migrate ");
    }

    #[test]
    fn ctrl_u_and_ctrl_k_cut_either_side_of_the_cursor() {
        let mut line = typed("Migrate to AWS");
        line.apply(ctrl('b'));
        line.apply(ctrl('b'));
        line.apply(ctrl('b'));
        let mut after = line.clone();
        line.apply(ctrl('u'));
        assert_eq!(line.text(), "AWS");
        after.apply(ctrl('k'));
        assert_eq!(after.text(), "Migrate to ");
    }

    #[test]
    fn the_cursor_stays_inside_the_text() {
        let mut line = Line::new("ab");
        line.apply(key(KeyCode::Right));
        line.apply(ctrl('f'));
        assert_eq!(line.before_cursor(), "ab");
        line.apply(key(KeyCode::Home));
        line.apply(key(KeyCode::Left));
        assert_eq!(line.before_cursor(), "");
    }

    #[test]
    fn a_modified_letter_is_not_typed() {
        let mut line = typed("a");
        line.apply(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        line.apply(ctrl('z'));
        assert_eq!(line.text(), "a");
    }

    #[test]
    fn a_frame_lines_up_the_values_and_places_the_cursor_after_the_text() {
        let (rows, column) = render("Title", &Line::new("Møte"), "File", "x.md", false);
        assert_eq!(rows[0], "Title › Møte");
        assert_eq!(rows[1], " File › x.md");
        assert_eq!(column, "Title › ".width() + "Møte".width());
    }

    #[test]
    fn a_frame_without_colour_carries_no_escape_codes() {
        let (rows, _) = render("Title", &Line::new("a"), "File", "a.md", false);
        assert!(rows.iter().all(|row| !row.contains('\x1b')));
    }
}
