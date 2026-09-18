//! Rendering a run of project steps for a terminal: the plan as a tree that is
//! redrawn while it runs, or a coloured prefix per step when output streams.
//!
//! Everything here is pure: [`crate::cmd::project`] runs the steps and prints
//! what comes back. Colour is the resolved [`crate::Ctx::color`], never a
//! terminal check of its own.

use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::term::paint;

use super::Step;

/// How a step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Done,
    /// The tool is not installed, so there was nothing to run. Not a failure:
    /// a project that pins a toolchain the machine does not have is a machine
    /// to install it on, not a broken run.
    Skipped {
        reason: String,
    },
    Failed {
        code: Option<i32>,
    },
}

/// One finished step.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: &'static str,
    pub status: Status,
    /// Every line the command wrote to either stream, in the order it
    /// arrived; empty when it never ran.
    pub output: String,
    pub duration: Duration,
}

impl Outcome {
    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Failed { .. })
    }
}

/// Colours cycled in plan order, so one step is told from another at a glance.
/// Red is left to failures, and these are the terminal's own palette, as the
/// rest of cid's colouring is.
const STEP_COLORS: &[u8] = &[6, 5, 2, 3, 4];

/// The colour of the step at `index` in the plan.
pub fn color(index: usize) -> u8 {
    STEP_COLORS[index % STEP_COLORS.len()]
}

/// For everything a row says about itself rather than names.
const DIM: u8 = crate::term::SECONDARY;
const GREEN: u8 = 2;
const RED: u8 = 1;

/// A step as the tree shows it: its name, and the step it waits for.
pub struct Node<'a> {
    pub name: &'a str,
    pub after: Option<&'a str>,
}

/// Where a step's line sits in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    indent: &'static str,
    /// The step's name, padded so every line's second column starts in one
    /// place whatever the indent.
    name: String,
}

/// Set before a step that waits for another, placing it under that step.
const INDENT: &str = "  ";

/// Indent every step that waits for another, and pad the names so the column
/// after them lines up across the whole tree.
pub fn layout(nodes: &[Node<'_>]) -> Vec<Slot> {
    let indent = |node: &Node<'_>| if node.after.is_some() { INDENT } else { "" };
    let width = nodes
        .iter()
        .map(|node| indent(node).len() + node.name.chars().count())
        .max()
        .unwrap_or(0);

    nodes
        .iter()
        .map(|node| {
            let indent = indent(node);
            Slot {
                indent,
                name: pad(node.name, width - indent.len()),
            }
        })
        .collect()
}

/// What a step in the tree is doing at the moment it is drawn.
pub enum Row<'a> {
    /// Not started: held back by the named step, or only not yet reached.
    Waiting {
        after: Option<&'a str>,
    },
    /// Started `elapsed` ago, and last said `activity`.
    Running {
        elapsed: Duration,
        activity: &'a str,
    },
    Finished(&'a Outcome),
}

/// One line of the live tree, no wider than `columns` so a redraw never
/// wraps onto a row it does not own. `frame` is the spinner's current glyph.
pub fn tree_line(slot: &Slot, row: &Row<'_>, frame: &str, columns: usize, color: bool) -> String {
    let Slot { indent, name } = slot;
    match row {
        Row::Waiting { after } => {
            let waiting = after.map_or_else(
                || "queued".to_string(),
                |after| format!("waiting for {after}"),
            );
            format!(
                "{indent}{} {}  {}",
                paint("◌", DIM, color),
                paint(name, DIM, color),
                paint(&waiting, DIM, color)
            )
        }
        Row::Running { elapsed, activity } => {
            let clock = format!("{}s", elapsed.as_secs());
            let used = indent.len() + 2 + name.width() + 2 + clock.len() + 2;
            let activity = truncate(activity, columns.saturating_sub(used));
            let line = format!(
                "{indent}{} {name}  {}",
                paint(frame, SPINNER, color),
                paint(&clock, DIM, color)
            );
            if activity.is_empty() {
                return line;
            }
            format!("{line}  {}", paint(&activity, DIM, color))
        }
        Row::Finished(outcome) => status_line(outcome, slot, color),
    }
}

/// The spinner's hue, the one [`crate::term::spinner`] turns in.
const SPINNER: u8 = 6;

/// One step's final line: what the tree settles on, and what is printed alone
/// when there is no terminal to draw the tree on.
pub fn status_line(outcome: &Outcome, slot: &Slot, color: bool) -> String {
    let Slot { indent, name } = slot;
    match &outcome.status {
        Status::Done => format!(
            "{indent}{} {name}  {}",
            paint("✓", GREEN, color),
            paint(&format_duration(outcome.duration), DIM, color)
        ),
        Status::Skipped { reason } => format!(
            "{indent}{} {}  {}",
            paint("-", DIM, color),
            paint(name, DIM, color),
            paint(reason, DIM, color)
        ),
        Status::Failed { code } => format!(
            "{indent}{} {name}  {}",
            paint("✗", RED, color),
            paint(&failure_reason(*code), RED, color)
        ),
    }
}

/// A line of a step's output reduced to what is worth showing beside its
/// spinner: escape sequences removed, and only the last frame of a line that
/// redraws itself with carriage returns. A blank line is nothing to show.
pub fn activity(line: &str) -> Option<String> {
    strip_escapes(line)
        .split('\r')
        .map(|frame| crate::term::one_row(frame).trim().to_string())
        .rfind(|frame| !frame.is_empty())
}

/// `text` without its ANSI escape sequences: CSI (`ESC [` … final byte) and
/// OSC (`ESC ]` … BEL or `ESC \`). What is left of anything else is dropped
/// with the other control characters by [`crate::term::one_row`].
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `text` cut to `columns` terminal cells, ending in `…` when anything was
/// cut. Counted in cells rather than characters, since a wide character that
/// overflows the row wraps it just as surely as two narrow ones.
fn truncate(text: &str, columns: usize) -> String {
    if text.width() <= columns {
        return text.to_string();
    }
    let room = columns.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if used + width > room {
            break;
        }
        used += width;
        out.push(c);
    }
    if columns > 0 {
        out.push('…');
    }
    out
}

fn failure_reason(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit {code}"),
        None => "did not run".to_string(),
    }
}

/// The width the labels in [`summary`] are padded to, wide enough for the
/// longest of them and a gap.
const LABEL_WIDTH: usize = 11;

/// What the run came to: the steps that did not finish, then the wall time.
/// Both are named in plan order, since that is the order they were listed in.
pub fn summary(outcomes: &[Outcome], elapsed: Duration, color: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut group = |label: &str, tint: u8, entries: Vec<String>| {
        if !entries.is_empty() {
            lines.push(format!(
                "{}{}",
                paint(&pad(label, LABEL_WIDTH), tint, color),
                entries.join(", ")
            ));
        }
    };

    group(
        "skipped",
        DIM,
        collect(outcomes, |outcome| match &outcome.status {
            Status::Skipped { reason } => Some(format!("{} ({reason})", outcome.name)),
            _ => None,
        }),
    );
    group(
        "failed",
        RED,
        collect(outcomes, |outcome| {
            outcome.failed().then(|| outcome.name.to_string())
        }),
    );
    lines.push(format!(
        "{}{}",
        paint(&pad("total", LABEL_WIDTH), DIM, color),
        paint(&format_duration(elapsed), DIM, color)
    ));

    lines
}

fn collect(outcomes: &[Outcome], of: impl Fn(&Outcome) -> Option<String>) -> Vec<String> {
    outcomes.iter().filter_map(of).collect()
}

/// The `[name]` each streamed line is written behind: coloured per step, and
/// padded so every step's output starts in the same column.
pub fn prefixes(names: &[&str], color: bool) -> Vec<String> {
    let width = names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
        + 3;

    names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let label = format!("[{name}]");
            // The padding stays outside the colour: a trailing run of coloured
            // spaces is a background nobody asked for.
            format!(
                "{}{}",
                paint(&label, self::color(index), color),
                " ".repeat(width.saturating_sub(label.chars().count()))
            )
        })
        .collect()
}

/// The plan as a dry run prints it: each step, the files that selected it, and
/// the command it would run. Each step keeps the colour its streamed output is
/// prefixed with, so it looks the same in both modes.
pub fn plan_listing(steps: &[&Step], color: bool) -> Vec<String> {
    let name_width = width(steps.iter().map(|step| step.name));
    let evidence_width = width(steps.iter().map(|step| step.evidence.as_str()));

    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            format!(
                "{}  {}  {} {}",
                paint(&pad(step.name, name_width), self::color(index), color),
                paint(&pad(&step.evidence, evidence_width), DIM, color),
                paint("$", DIM, color),
                step.command_line()
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

fn width<'a>(texts: impl Iterator<Item = &'a str>) -> usize {
    texts.map(|text| text.chars().count()).max().unwrap_or(0)
}

/// Pad to `width` columns, counting characters rather than bytes.
pub fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    out.push_str(&" ".repeat(width.saturating_sub(text.chars().count())));
    out
}

/// Milliseconds under a second, one decimal of a second above it. A step that
/// takes minutes is still reported in seconds: it is a number to compare with
/// the step beside it, not a clock.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &'static str, status: Status) -> Outcome {
        Outcome {
            name,
            status,
            output: String::new(),
            duration: Duration::from_millis(120),
        }
    }

    fn skipped(reason: &str) -> Status {
        Status::Skipped {
            reason: reason.to_string(),
        }
    }

    #[test]
    fn durations_switch_from_milliseconds_to_seconds() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(1000)), "1.0s");
        assert_eq!(format_duration(Duration::from_millis(4321)), "4.3s");
    }

    fn slot(name: &str, width: usize) -> Slot {
        Slot {
            indent: "",
            name: pad(name, width),
        }
    }

    #[test]
    fn a_status_line_says_which_of_the_three_things_happened_without_colour() {
        let done = status_line(&outcome("rust", Status::Done), &slot("rust", 6), false);
        let skip = status_line(
            &outcome("maven", skipped("mvn not found")),
            &slot("maven", 6),
            false,
        );
        let fail = status_line(
            &outcome("go", Status::Failed { code: Some(1) }),
            &slot("go", 6),
            false,
        );

        assert_eq!(done, "✓ rust    120ms");
        assert_eq!(skip, "- maven   mvn not found");
        assert_eq!(fail, "✗ go      exit 1");
    }

    #[test]
    fn steps_that_wait_are_indented_under_the_step_they_wait_for() {
        let slots = layout(&[
            Node {
                name: "mise",
                after: None,
            },
            Node {
                name: "terraform",
                after: Some("mise"),
            },
        ]);

        assert_eq!(
            slots,
            [
                Slot {
                    indent: "",
                    name: "mise       ".to_string(),
                },
                Slot {
                    indent: INDENT,
                    name: "terraform".to_string(),
                },
            ]
        );
    }

    #[test]
    fn a_tree_without_dependencies_is_flat() {
        let slots = layout(&[
            Node {
                name: "go",
                after: None,
            },
            Node {
                name: "rust",
                after: None,
            },
        ]);

        assert!(slots.iter().all(|slot| slot.indent.is_empty()));
        assert_eq!(slots[0].name, "go  ");
    }

    #[test]
    fn a_tree_line_shows_each_state_of_a_step_without_colour() {
        let slots = layout(&[
            Node {
                name: "mise",
                after: None,
            },
            Node {
                name: "rust",
                after: Some("mise"),
            },
        ]);
        let line = |slot: &Slot, row: Row<'_>| tree_line(slot, &row, "⠋", 80, false);
        let done = outcome("rust", Status::Done);

        assert_eq!(
            line(
                &slots[1],
                Row::Waiting {
                    after: Some("mise")
                }
            ),
            "  ◌ rust  waiting for mise"
        );
        assert_eq!(
            line(&slots[0], Row::Waiting { after: None }),
            "◌ mise    queued"
        );
        assert_eq!(
            line(
                &slots[0],
                Row::Running {
                    elapsed: Duration::from_millis(14_600),
                    activity: "installing node",
                }
            ),
            "⠋ mise    14s  installing node"
        );
        let silent = Row::Running {
            elapsed: Duration::from_secs(3),
            activity: "",
        };
        assert_eq!(line(&slots[0], silent), "⠋ mise    3s");
        assert!(
            tree_line(
                &slots[0],
                &Row::Running {
                    elapsed: Duration::from_secs(3),
                    activity: "",
                },
                "⠋",
                80,
                true
            )
            .ends_with("3s\x1b[0m"),
            "a step that has said nothing still drew a segment for it"
        );
        assert_eq!(line(&slots[1], Row::Finished(&done)), "  ✓ rust  120ms");
    }

    #[test]
    fn a_running_line_never_outgrows_the_terminal() {
        let slots = layout(&[Node {
            name: "bun",
            after: None,
        }]);
        let row = Row::Running {
            elapsed: Duration::from_secs(3),
            activity: "resolving 情報 packages from the registry",
        };

        for columns in [0, 10, 20, 30] {
            let line = tree_line(&slots[0], &row, "⠋", columns, false);
            assert!(
                line.width() <= columns.max(10),
                "{columns}: {line:?} is {} wide",
                line.width()
            );
        }
    }

    #[test]
    fn truncation_marks_the_cut_and_counts_cells() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("情報情報", 5), "情報…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn activity_is_the_last_visible_frame_of_a_line() {
        assert_eq!(activity("  fetching  "), Some("fetching".to_string()));
        assert_eq!(activity("\x1b[32mdone\x1b[0m"), Some("done".to_string()));
        assert_eq!(activity("10%\r50%\r"), Some("50%".to_string()));
        assert_eq!(
            activity("\x1b]8;;https://x\x07link\x1b]8;;\x1b\\"),
            Some("link".to_string())
        );
    }

    #[test]
    fn blank_output_is_not_activity() {
        assert_eq!(activity(""), None);
        assert_eq!(activity("   \r  "), None);
        assert_eq!(activity("\x1b[2K"), None);
    }

    #[test]
    fn a_step_that_never_started_says_so_rather_than_naming_a_status() {
        assert_eq!(failure_reason(None), "did not run");
        assert_eq!(failure_reason(Some(2)), "exit 2");
    }

    #[test]
    fn the_summary_names_skips_and_failures_in_plan_order() {
        let outcomes = [
            outcome("mise", Status::Done),
            outcome("maven", skipped("mvn not found")),
            outcome("go", Status::Failed { code: Some(1) }),
            outcome("rust", Status::Done),
            outcome("deno", skipped("deno not found")),
        ];

        assert_eq!(
            summary(&outcomes, Duration::from_millis(3500), false),
            vec![
                "skipped    maven (mvn not found), deno (deno not found)",
                "failed     go",
                "total      3.5s",
            ]
        );
    }

    #[test]
    fn a_run_with_nothing_to_report_is_the_total_alone() {
        let outcomes = [outcome("rust", Status::Done)];
        assert_eq!(
            summary(&outcomes, Duration::from_millis(12), false),
            vec!["total      12ms"]
        );
    }

    #[test]
    fn every_step_gets_a_prefix_of_the_same_width() {
        let names = ["mise", "rust", "terraform"];
        let prefixes = prefixes(&names, false);

        assert_eq!(prefixes.len(), names.len());
        let width = prefixes[0].chars().count();
        for (prefix, name) in prefixes.iter().zip(names) {
            assert!(prefix.starts_with(&format!("[{name}]")), "{prefix}");
            assert_eq!(prefix.chars().count(), width, "{prefix}");
        }
    }

    #[test]
    fn the_plan_listing_shows_the_name_the_evidence_and_the_command() {
        let steps = [
            Step::new("mise", "mise.toml".into(), "mise", &["install"]),
            Step::new("bun", "package.json + bun.lock".into(), "bun", &["install"]),
        ];
        let refs: Vec<&Step> = steps.iter().collect();

        assert_eq!(
            plan_listing(&refs, false),
            vec![
                "mise  mise.toml                $ mise install",
                "bun   package.json + bun.lock  $ bun install",
            ]
        );
    }

    #[test]
    fn colours_cycle_and_never_land_on_the_one_failures_use() {
        assert_eq!(color(0), color(STEP_COLORS.len()));
        for index in 0..STEP_COLORS.len() {
            assert_ne!(color(index), RED, "a step was coloured like a failure");
        }
    }

    #[test]
    fn colour_wraps_a_row_without_changing_what_it_says() {
        let line = status_line(&outcome("rust", Status::Done), &slot("rust", 4), true);
        assert!(line.contains("\x1b["), "nothing was coloured");
        assert!(line.contains("rust"), "{line}");
    }

    #[test]
    fn padding_counts_characters_rather_than_bytes() {
        assert_eq!(pad("æøå", 5), "æøå  ");
        assert_eq!(pad("wider", 2), "wider");
    }
}
