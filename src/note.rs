//! Notes: a tree of Markdown files — an Obsidian vault, or anything shaped like
//! one — read as rows in a selector.
//!
//! Everything here is pure. The walk, the file reads and the clock live in
//! [`cmd::note`](crate::cmd::note); this module turns the bytes they hand back
//! into a title, a set of tags, two dates and an aligned row.
//!
//! A note says what it is in its YAML front matter, which is the only metadata
//! this reads. Inline `#tags` in the body are deliberately not indexed: finding
//! them means scanning every byte of every note and then telling a tag apart
//! from a heading, a colour literal and a URL fragment, and a tag column that is
//! right most of the time is worse than one that is right always.

use std::path::{Path, PathBuf};

use crate::select::Tint;

/// The colour of a search row's path — an index into the ANSI 256-colour
/// table, so it follows the terminal's own theme. Blue, as every file listing
/// colours a path.
const FOLDER_COLOR: u8 = 4;
/// What a note's front matter said about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Front {
    /// `title:`, when the note names itself something other than its filename.
    pub title: Option<String>,
    /// `tags:`/`tag:`, in the order written, without a leading `#`.
    pub tags: Vec<String>,
    /// `created:`/`date:`, as local midnight on the day it names.
    pub created: Option<i64>,
}

/// One note, as a listing sees it.
///
/// Built by the shell from a directory entry, its metadata and the first few
/// kilobytes of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Absolute path — what a row returns and what an editor is handed.
    pub path: PathBuf,
    /// Path below the vault root, extension and all. The name a note is known
    /// by on the command line.
    pub rel: String,
    /// The directory directly below the root that the note is filed under, or
    /// empty for a note at the root itself. What `[note] labels` labels.
    pub dir: String,
    /// Last modified, in Unix seconds.
    pub modified: i64,
    /// Created, in Unix seconds. See [`created`].
    pub created: i64,
    pub front: Front,
}

impl Note {
    /// What the note calls itself: its front matter's `title`, else the
    /// filename without its extension. Obsidian links notes by filename, so the
    /// filename is the name unless the note says otherwise.
    pub fn title(&self) -> &str {
        match &self.front.title {
            Some(title) => title,
            None => stem(&self.rel),
        }
    }

    /// The group column: the label [`Note::dir`] carries, or the directory's
    /// own name when it carries none.
    ///
    /// Unlike `repo`, which writes [`UNLABELLED`](crate::config::UNLABELLED)
    /// for an owner with no label, an unlabelled directory names itself here.
    /// A repository row still carries its owner in the path beside the label
    /// column; a note row does not, and a vault of five directories with two of
    /// them labelled would otherwise show three rows that say only `-`.
    pub fn group<'a>(&'a self, labels: &'a crate::config::NoteConfig) -> &'a str {
        labels.label_of(&self.dir).unwrap_or(&self.dir)
    }

    /// Whether the group column is a configured label rather than a bare
    /// directory name — which is what decides whether it takes a colour.
    pub fn labelled(&self, labels: &crate::config::NoteConfig) -> bool {
        labels.label_of(&self.dir).is_some()
    }

    /// The path a *report* shows: absolute, with the home directory collapsed
    /// to `~`.
    ///
    /// `note ls` prints the path itself, uncollapsed, because it is read by
    /// whatever the listing is piped into. `--status` is read by a person, and
    /// a vault under `$HOME` repeats the same twenty characters down the left
    /// of every row otherwise.
    pub fn shown(&self, home: &str) -> String {
        crate::path::display_path(&self.path.to_string_lossy(), home, false)
    }

    /// The tags column: every tag, `#`-prefixed, space-separated.
    pub fn tag_column(&self) -> String {
        self.front
            .tags
            .iter()
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The directory directly below `root` that `rel` is filed under, or empty for
/// a path at the root itself.
pub fn top_dir(rel: &str) -> &str {
    match rel.find('/') {
        Some(at) => &rel[..at],
        None => "",
    }
}

/// Whether `rel` — a path below the vault root — is one of the `archives`
/// directories or sits inside one.
///
/// A directory matches itself as well as everything below it, so a walk can
/// prune on the same answer that hides a note. Segments are compared
/// case-insensitively, as a path is on Darwin, and empty and `.` segments are
/// dropped from both sides so `work/archive`, `work/archive/` and
/// `./work/archive` name the same directory. An entry naming no directory at
/// all matches nothing, rather than archiving the whole vault.
pub fn is_archived(rel: &str, archives: &[String]) -> bool {
    archives.iter().any(|archive| under(rel, archive))
}

/// Whether `rel` is `archive` or lies below it.
fn under(rel: &str, archive: &str) -> bool {
    let mut wanted = segments(archive).peekable();
    if wanted.peek().is_none() {
        return false;
    }
    let mut have = segments(rel);
    wanted.all(|want| {
        have.next()
            .is_some_and(|seg| seg.eq_ignore_ascii_case(want))
    })
}

/// The meaningful parts of a relative path.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|seg| !seg.is_empty() && *seg != ".")
}

/// When a note was created: what its front matter says, else what the
/// filesystem says.
///
/// The front matter wins because the filesystem's answer is so often wrong — a
/// vault that arrives by `git clone`, by a sync client or by a copy has every
/// note born the same afternoon, and a column that says that of five hundred
/// notes says nothing. `birth` is `None` on a filesystem that does not record
/// one, and the modification time is the last resort: nothing is newer than
/// itself.
pub fn created(front: Option<i64>, birth: Option<i64>, modified: i64) -> i64 {
    front.or(birth).unwrap_or(modified)
}

/// The filename, without its directories or its extension.
fn stem(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rfind('.') {
        // A leading dot is the whole name, not an extension.
        Some(at) if at > 0 => &name[..at],
        _ => name,
    }
}

// --- front matter -----------------------------------------------------------

/// Split `text` into its YAML front matter block and the body below it.
///
/// The block is what sits between a first line of exactly `---` and the next
/// line of exactly `---` or `...`. An unterminated block is not one: a note
/// opening with a horizontal rule would otherwise lose its entire body.
pub fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let Some(after) = open_fence(text) else {
        return (None, text);
    };
    let mut at = 0;
    for line in after.split_inclusive('\n') {
        if matches!(line.trim_end_matches(['\n', '\r']), "---" | "...") {
            return (Some(&after[..at]), &after[at + line.len()..]);
        }
        at += line.len();
    }
    (None, text)
}

/// What follows the opening `---` line, when that is how `text` begins.
fn open_fence(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
}

/// Read a front matter block.
///
/// A deliberately small reader rather than a YAML parser: it takes top-level
/// `key: value` lines and the block or flow sequences under them, and skips
/// everything else — nested mappings, anchors, multi-line scalars. The three
/// keys it looks for are a scalar or a list of scalars in every vault that
/// writes them, and a YAML dependency would parse a great deal this never
/// reads.
///
/// `offset` dates a `created:` value, which names a day rather than an instant:
/// the day is the writer's own, so it becomes local midnight rather than UTC.
pub fn parse_front(block: &str, offset: time::UtcOffset) -> Front {
    let mut front = Front::default();
    // Whether the block sequence being read is the one `tags:` opened.
    let mut in_tags = false;

    for raw in block.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = sequence_item(trimmed) {
            if in_tags {
                push_tag(&mut front.tags, item);
            }
            continue;
        }
        // Indented and not a sequence item: part of a mapping this does not
        // read, rather than a key of its own.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        in_tags = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "title" => front.title = scalar(value),
            "tags" | "tag" => {
                // A key with nothing after it opens a block sequence; anything
                // else is the whole list, on this line.
                if value.is_empty() {
                    in_tags = true;
                } else {
                    for tag in split_tags(value) {
                        push_tag(&mut front.tags, tag);
                    }
                }
            }
            // `created` is the one a vault writes deliberately, so it is not
            // displaced by a `date` further down the block.
            "created" | "date" => front.created = front.created.or(date_at(value, offset)),
            _ => {}
        }
    }
    front
}

/// The value of a `- item` line, at any indentation.
fn sequence_item(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix('-')?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix(' ').map(str::trim)
}

/// A scalar value: unquoted and trimmed, `None` when nothing is left of it.
fn scalar(value: &str) -> Option<String> {
    let value = unquote(value.trim());
    (!value.is_empty()).then(|| value.to_string())
}

/// Drop one matching pair of surrounding quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Split a one-line tag list. `[a, b]`, `a, b` and `a b` are all written in the
/// wild, and none of them can hold a tag with a space in it.
fn split_tags(value: &str) -> impl Iterator<Item = &str> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    inner.split([',', ' ', '\t'])
}

/// Add `tag` to `tags` unless it is empty or already there. The leading `#` an
/// Obsidian tag is often written with is not part of the tag.
fn push_tag(tags: &mut Vec<String>, tag: &str) {
    let Some(tag) = scalar(tag) else { return };
    let tag = tag.trim_start_matches('#').to_string();
    if !tag.is_empty() && !tags.contains(&tag) {
        tags.push(tag);
    }
}

/// A front matter date as local midnight on the day it names.
///
/// Whatever follows the date — a time, a zone — is dropped: the column shows a
/// day, and a note that writes `09:00` has not said which zone that was.
fn date_at(value: &str, offset: time::UtcOffset) -> Option<i64> {
    let date = unquote(value.trim()).split(['T', ' ']).next()?;
    let mut parts = date.split('-');
    let year: i32 = parts.next()?.trim().parse().ok()?;
    let month: u8 = parts.next()?.trim().parse().ok()?;
    let day: u8 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = time::Month::try_from(month).ok()?;
    Some(
        time::Date::from_calendar_date(year, month, day)
            .ok()?
            .midnight()
            .assume_offset(offset)
            .unix_timestamp(),
    )
}

// --- dates ------------------------------------------------------------------

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
const YEAR: i64 = 365 * DAY;
const MONTH: i64 = YEAR / 12;

/// How long ago `then` was, in three or four characters — `now`, `42m`, `6d`,
/// `11mo`, `3y`.
///
/// The selector dates a note this way rather than with the calendar date the
/// listing prints: two calendar columns are twenty-two characters of a pane
/// already sharing its width with a preview, and the question a note list
/// answers is "which of these did I touch recently", not "on which Tuesday".
///
/// A note dated in the future — a clock that went backwards, a front matter
/// date typed ahead — reads as `now` rather than as a negative age.
pub fn age(now: i64, then: i64) -> String {
    let seconds = (now - then).max(0);
    match seconds {
        s if s < MINUTE => "now".to_string(),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s if s < 5 * WEEK => format!("{}w", s / WEEK),
        // Never `0mo`: the arm above stops five weeks short of the average
        // month this divides by.
        s if s < YEAR => format!("{}mo", (s / MONTH).max(1)),
        s => format!("{}y", s / YEAR),
    }
}

/// A calendar date, `YYYY-MM-DD`, in local time.
pub fn date(when: i64, offset: time::UtcOffset) -> String {
    match local(when, offset) {
        Some(dt) => format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day()),
        None => BLANK_DATE.to_string(),
    }
}

/// A calendar date and the time of day, `YYYY-MM-DD HH:MM`, in local time.
pub fn timestamp(when: i64, offset: time::UtcOffset) -> String {
    match local(when, offset) {
        Some(dt) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            dt.year(),
            dt.month() as u8,
            dt.day(),
            dt.hour(),
            dt.minute(),
        ),
        None => BLANK_TIMESTAMP.to_string(),
    }
}

/// [`date`]- and [`timestamp`]-shaped blanks, so a time no calendar can
/// represent costs the listing a value rather than a column.
const BLANK_DATE: &str = "          ";
const BLANK_TIMESTAMP: &str = "                ";

fn local(when: i64, offset: time::UtcOffset) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(when)
        .ok()
        .map(|dt| dt.to_offset(offset))
}

// --- rows -------------------------------------------------------------------

/// The width of every column a note listing shares, measured over the whole
/// list so the columns line up.
///
/// Counted in characters, not bytes: `{:<width$}` pads by character count, so a
/// byte length would over-pad a title with an accent in it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Widths {
    pub title: usize,
    pub group: usize,
    pub tags: usize,
    pub path: usize,
    /// The vault-relative path, which the cleanup list shows where a report
    /// shows the whole thing.
    pub rel: usize,
    /// The widest rendered [`size`], so the sizes right-align under each other.
    pub size: usize,
}

impl Widths {
    pub fn of(notes: &[Note], cfg: &crate::config::NoteConfig, home: &str) -> Self {
        let widest = |f: &dyn Fn(&Note) -> usize| notes.iter().map(f).max().unwrap_or(0);
        Self {
            title: widest(&|n| n.title().chars().count()),
            group: widest(&|n| n.group(cfg).chars().count()),
            tags: widest(&|n| n.tag_column().chars().count()),
            path: widest(&|n| n.shown(home).chars().count()),
            rel: widest(&|n| n.rel.chars().count()),
            size: 0,
        }
    }

    /// The widths a cleanup list needs on top: the sizes it is going to draw.
    pub fn with_sizes(mut self, sizes: impl Iterator<Item = u64>) -> Self {
        self.size = sizes.map(|b| size(b).chars().count()).max().unwrap_or(0);
        self
    }
}

/// The columns ahead of a selector row: the note's group, padded to `width`
/// and tinted with its label's colour, then the day the note was created.
///
/// A [`crate::select::SelectItem::prefix`] rather than part of the label, so
/// both are drawn in their own colours and — the reason they are not simply
/// columns — never matched: typed into the search box, `2024` would otherwise
/// rank a year's worth of notes above the one being looked for, and a group
/// name would rank the whole directory.
///
/// `width` is measured over the entire vault by [`group_width`], not over
/// whatever the query has left on screen, so the titles hold their column as
/// the list narrows.
pub fn prefix(
    note: &Note,
    cfg: &crate::config::NoteConfig,
    width: usize,
    offset: time::UtcOffset,
) -> (String, Vec<Tint>) {
    let mut group = String::new();
    if width > 0 {
        push_column(&mut group, note.group(cfg), width);
        group.push_str(COLUMN_GAP);
    }
    let tints = row_color(note, cfg).map_or_else(Vec::new, |color| {
        vec![Tint {
            range: 0..group.chars().count(),
            color,
        }]
    });
    (
        format!("{group}{}{COLUMN_GAP}", date(note.created, offset)),
        tints,
    )
}

/// The width of the group column, measured over every note the selector was
/// opened on. Zero for a vault whose notes all sit at the root, which is a
/// column of nothing.
pub fn group_width(notes: &[Note], cfg: &crate::config::NoteConfig) -> usize {
    notes
        .iter()
        .map(|note| note.group(cfg).chars().count())
        .max()
        .unwrap_or(0)
}

/// One selector row: what the note calls itself, and nothing else.
///
/// What a note is tagged and how much of it is done are in the preview pane,
/// which has the width for them and is one keystroke away. A row is for telling
/// one note from another at a glance, and six columns of attributes is a worse
/// way to do that than a name, a date and where it is filed.
///
/// The row takes its group's colour where it has one, the way a repository row
/// does, and the group itself is named in [`prefix`].
pub fn row(note: &Note) -> String {
    note.title().to_string()
}

/// The colour the whole row is drawn in: its label's, or the terminal's own
/// foreground where its directory carries none.
pub fn row_color(note: &Note, cfg: &crate::config::NoteConfig) -> Option<u8> {
    note.labelled(cfg)
        .then(|| cfg.color_of(note.group(cfg)))
        .flatten()
}

/// One `note ls --status` row: the note's absolute path, its group, its tags,
/// and both dates.
///
/// The path comes first, with the home directory collapsed to `~` — this is
/// the listing a person reads, where `note ls` on its own is the one a pipe
/// does. Modified carries a time of day and created does not: a created date
/// may have come from front matter, which names a day.
pub fn status_row(
    note: &Note,
    cfg: &crate::config::NoteConfig,
    widths: &Widths,
    offset: time::UtcOffset,
    home: &str,
) -> String {
    let mut row = String::new();
    push_column(&mut row, &note.shown(home), widths.path);
    for (text, width) in [
        (note.group(cfg).to_string(), widths.group),
        (note.tag_column(), widths.tags),
    ] {
        if width > 0 {
            row.push_str(COLUMN_GAP);
            push_column(&mut row, &text, width);
        }
    }
    row.push_str(COLUMN_GAP);
    row.push_str(&timestamp(note.modified, offset));
    row.push_str(COLUMN_GAP);
    row.push_str(&date(note.created, offset));
    row.trim_end().to_string()
}

/// Between two columns. Two spaces, everywhere, so a row reads as columns
/// rather than as a sentence.
const COLUMN_GAP: &str = "  ";

/// Append `text` and pad it out to `width` characters.
fn push_column(row: &mut String, text: &str, width: usize) {
    row.push_str(text);
    for _ in text.chars().count()..width {
        row.push(' ');
    }
}

/// Order a vault: most recently modified first, then by path so notes written
/// in the same second do not shuffle between runs.
///
/// Deliberately not [`crate::Ctx::by_recency`], which every other selector is
/// ordered by: opening a note is what changes its modification time, so the
/// file already records what that store would, and two orderings of one fact
/// can only disagree.
pub fn newest_first(notes: &mut [Note]) {
    notes.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.rel.cmp(&b.rel)));
}

/// The notes whose names match `query`, best first.
///
/// A searching selector hands what is typed to the search rather than to the
/// fuzzy matcher, so a list of names already in memory has to be matched here.
/// It goes through the selector's own scorer, which is what keeps this list
/// ranked the way every other list in cid is.
///
/// An empty query is every note, in the order [`newest_first`] left them: the
/// whole vault is a useful answer to nothing typed yet, where an empty search
/// of the text is not.
pub fn by_name<'a>(notes: &'a [Note], query: &str) -> Vec<&'a Note> {
    if query.trim().is_empty() {
        return notes.iter().collect();
    }
    let mut scored: Vec<(i64, &Note)> = notes
        .iter()
        .filter_map(|note| crate::select::score(&row(note), query).map(|score| (score, note)))
        .collect();
    // Stable, so notes that score alike stay newest first rather than in
    // whatever order the walk found them.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, note)| note).collect()
}

/// Whether a path is one a note listing offers, by extension.
pub fn is_note(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            NOTE_EXTENSIONS
                .iter()
                .any(|kind| ext.eq_ignore_ascii_case(kind))
        })
}

/// What counts as a note. Obsidian writes Markdown and reads nothing else, and
/// a vault's other files — its attachments, its `.obsidian` settings — are not
/// things to hand an editor.
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown"];

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> time::UtcOffset {
        time::UtcOffset::UTC
    }

    fn note(rel: &str, front: Front) -> Note {
        Note {
            path: PathBuf::from("/vault").join(rel),
            dir: top_dir(rel).to_string(),
            rel: rel.to_string(),
            modified: 0,
            created: 0,
            front,
        }
    }

    /// The one note a cleanup list never offers, however empty it is.
    fn scratch() -> &'static Path {
        Path::new("/vault/scratch/scratch.md")
    }

    /// A vault where `work` is labelled and `scratch` is not.
    fn config() -> crate::config::NoteConfig {
        let mut labels = crate::config::Labels::new();
        labels.insert("work".to_string(), vec!["work".to_string()]);
        crate::config::NoteConfig {
            root: Some("/vault".into()),
            labels,
            ..crate::config::NoteConfig::default()
        }
    }

    fn front(text: &str) -> Front {
        let (block, _) = split_front_matter(text);
        parse_front(block.expect("front matter"), utc())
    }

    // --- front matter ---

    #[test]
    fn a_block_is_what_sits_between_the_two_fences() {
        let (block, body) = split_front_matter("---\ntitle: A\n---\n# Heading\n");
        assert_eq!(block, Some("title: A\n"));
        assert_eq!(body, "# Heading\n");
    }

    /// A note that opens with a horizontal rule has no front matter, and must
    /// not lose its body to a block that never closes.
    #[test]
    fn an_unterminated_block_is_not_front_matter() {
        let text = "---\njust a rule, then prose\n";
        assert_eq!(split_front_matter(text), (None, text));
    }

    #[test]
    fn a_note_with_no_block_keeps_its_whole_body() {
        let text = "# Heading\n\nbody\n";
        assert_eq!(split_front_matter(text), (None, text));
    }

    #[test]
    fn a_block_may_close_with_the_other_yaml_terminator() {
        let (block, body) = split_front_matter("---\ntitle: A\n...\nbody\n");
        assert_eq!(block, Some("title: A\n"));
        assert_eq!(body, "body\n");
    }

    #[test]
    fn tags_are_read_from_a_flow_sequence_a_block_sequence_or_a_bare_list() {
        for written in [
            "tags: [rust, til]",
            "tags:\n  - rust\n  - til",
            "tags:\n- rust\n- til",
            "tags: rust til",
            "tags: rust, til",
            "tags: [\"rust\", 'til']",
            "tag: #rust #til",
        ] {
            let parsed = front(&format!("---\n{written}\n---\n"));
            assert_eq!(parsed.tags, vec!["rust", "til"], "{written:?}");
        }
    }

    /// The tag column is drawn with a `#`, so a vault that writes them that way
    /// must not end up with two.
    #[test]
    fn a_tag_is_stored_without_the_hash_it_is_drawn_with() {
        let parsed = front("---\ntags: [\"#rust\"]\n---\n");
        assert_eq!(parsed.tags, vec!["rust"]);
        assert_eq!(note("a.md", parsed).tag_column(), "#rust");
    }

    #[test]
    fn a_repeated_tag_is_listed_once() {
        assert_eq!(front("---\ntags: [a, b, a]\n---\n").tags, vec!["a", "b"]);
    }

    /// A block sequence belongs to the key that opened it. `aliases` is the one
    /// every vault has, and its items are not tags.
    #[test]
    fn a_sequence_under_another_key_is_not_read_as_tags() {
        let parsed = front("---\ntags:\n  - real\naliases:\n  - other\n  - names\n---\n");
        assert_eq!(parsed.tags, vec!["real"]);
    }

    #[test]
    fn a_nested_mapping_is_skipped_rather_than_read_as_keys() {
        let parsed = front("---\nobsidian:\n  title: nested\ntitle: real\n---\n");
        assert_eq!(parsed.title.as_deref(), Some("real"));
    }

    #[test]
    fn a_quoted_title_loses_its_quotes() {
        assert_eq!(
            front("---\ntitle: \"Error handling\"\n---\n")
                .title
                .as_deref(),
            Some("Error handling")
        );
    }

    /// A title with a colon in it is one value, not a key and a value: only the
    /// first colon separates them.
    #[test]
    fn a_title_may_contain_a_colon() {
        assert_eq!(
            front("---\ntitle: Rust: error handling\n---\n")
                .title
                .as_deref(),
            Some("Rust: error handling")
        );
    }

    #[test]
    fn a_created_date_becomes_local_midnight_on_the_day_it_names() {
        let offset = time::UtcOffset::from_hms(2, 0, 0).unwrap();
        let (block, _) = split_front_matter("---\ncreated: 2024-03-01\n---\n");
        let parsed = parse_front(block.unwrap(), offset);
        assert_eq!(date(parsed.created.unwrap(), offset), "2024-03-01");
    }

    #[test]
    fn a_created_value_carrying_a_time_still_names_a_day() {
        for written in ["2024-03-01T09:30:00", "2024-03-01 09:30", "\"2024-03-01\""] {
            let parsed = front(&format!("---\ncreated: {written}\n---\n"));
            assert_eq!(
                date(parsed.created.expect(written), utc()),
                "2024-03-01",
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_created_value_that_is_not_a_date_is_no_date_at_all() {
        for written in ["someday", "2024", "not-a-date-x", "2024-13-01"] {
            let parsed = front(&format!("---\ncreated: {written}\n---\n"));
            assert_eq!(parsed.created, None, "{written:?}");
        }
    }

    #[test]
    fn a_vault_writing_date_rather_than_created_is_read_the_same_way() {
        let parsed = front("---\ndate: 2024-03-01\n---\n");
        assert_eq!(date(parsed.created.unwrap(), utc()), "2024-03-01");
    }

    // --- archives ---

    #[test]
    fn an_archive_covers_itself_and_everything_below_it() {
        let archives = vec!["work/archive".to_string()];
        assert!(is_archived("work/archive", &archives));
        assert!(is_archived("work/archive/old.md", &archives));
        assert!(is_archived("work/archive/2023/old.md", &archives));
    }

    #[test]
    fn a_note_outside_every_archive_stays_listed() {
        let archives = vec!["work/archive".to_string(), "personal/archive".to_string()];
        assert!(!is_archived("work/plan.md", &archives));
        assert!(!is_archived("personal/idea.md", &archives));
        assert!(!is_archived("inbox.md", &archives));
        assert!(!is_archived("archive/old.md", &archives));
    }

    /// A directory whose name merely starts with the archive's is a different
    /// directory, and a path is matched by segment rather than by prefix.
    #[test]
    fn an_archive_is_not_a_prefix_of_the_name_beside_it() {
        let archives = vec!["work/archive".to_string()];
        assert!(!is_archived("work/archived-plans.md", &archives));
        assert!(!is_archived("work/archive-2023/old.md", &archives));
    }

    /// The config is written by hand, and a trailing slash or a leading `./`
    /// names the same directory as the plain path does.
    #[test]
    fn an_archive_path_is_read_the_way_it_was_written() {
        for written in [
            "work/archive/",
            "./work/archive",
            "/work/archive",
            "work//archive",
        ] {
            let archives = vec![written.to_string()];
            assert!(is_archived("work/archive/old.md", &archives), "{written}");
        }
    }

    /// Darwin does not tell `Archive` and `archive` apart, and neither does the
    /// label a directory carries.
    #[test]
    fn an_archive_matches_a_directory_of_any_case() {
        let archives = vec!["Work/Archive".to_string()];
        assert!(is_archived("work/archive/old.md", &archives));
    }

    /// An entry naming no directory would otherwise archive the whole vault.
    #[test]
    fn an_empty_archive_entry_hides_nothing() {
        for empty in ["", "/", ".", "./"] {
            assert!(
                !is_archived("work/plan.md", &[empty.to_string()]),
                "{empty}"
            );
        }
    }

    #[test]
    fn nothing_is_archived_when_nothing_is_configured() {
        assert!(!is_archived("work/archive/old.md", &[]));
    }

    // --- what a note calls itself ---

    #[test]
    fn a_note_falls_back_to_its_filename_and_takes_a_title_over_it() {
        assert_eq!(note("work/standup.md", Front::default()).title(), "standup");
        assert_eq!(
            note(
                "work/standup.md",
                Front {
                    title: Some("Daily standup".into()),
                    ..Front::default()
                }
            )
            .title(),
            "Daily standup"
        );
    }

    /// The filesystem dates a synced or freshly cloned vault the day it
    /// arrived, which is why front matter wins.
    #[test]
    fn a_creation_date_prefers_front_matter_then_the_filesystem_then_the_edit() {
        assert_eq!(created(Some(10), Some(20), 30), 10);
        assert_eq!(created(None, Some(20), 30), 20);
        assert_eq!(created(None, None, 30), 30);
    }

    // --- dates ---

    #[test]
    fn an_age_reads_as_the_largest_unit_that_fits() {
        let cases = [
            (0, "now"),
            (59, "now"),
            (60, "1m"),
            (59 * MINUTE, "59m"),
            (HOUR, "1h"),
            (DAY, "1d"),
            (6 * DAY, "6d"),
            (WEEK, "1w"),
            (4 * WEEK, "4w"),
            (5 * WEEK, "1mo"),
            (364 * DAY, "11mo"),
            (YEAR, "1y"),
        ];
        for (elapsed, want) in cases {
            assert_eq!(age(elapsed, 0), want, "{elapsed}s");
        }
    }

    /// A clock that went backwards, or a front matter date typed ahead, must
    /// not print a negative age into a right-aligned column.
    #[test]
    fn a_note_dated_in_the_future_reads_as_now() {
        assert_eq!(age(0, 10_000), "now");
    }

    #[test]
    fn an_age_never_rounds_down_to_no_months_at_all() {
        for elapsed in (5 * WEEK)..YEAR {
            let stamp = age(elapsed, 0);
            assert!(stamp.starts_with(|c: char| c != '0'), "{elapsed}s: {stamp}");
        }
    }

    // --- rows ---

    fn vault() -> Vec<Note> {
        vec![
            Note {
                modified: 3 * DAY,
                created: 400 * DAY,
                ..note(
                    "work/meetings/standup.md",
                    Front {
                        tags: vec!["daily".into()],
                        ..Front::default()
                    },
                )
            },
            Note {
                modified: 0,
                created: 0,
                ..note("scratch/idea.md", Front::default())
            },
            Note {
                modified: 0,
                created: 0,
                ..note("inbox.md", Front::default())
            },
        ]
    }

    /// A row is a name and nothing else. Everything a note *is* lives in the
    /// preview pane, which has the width for it.
    #[test]
    fn a_row_is_what_the_note_calls_itself() {
        let rows: Vec<String> = vault().iter().map(row).collect();
        assert_eq!(rows, vec!["standup", "idea", "inbox"]);
    }

    /// The group and the date are drawn in their own colours ahead of the name,
    /// and outside what the query matches — typed, `2024` would otherwise rank
    /// a year of notes above the one being looked for, and `work` the whole
    /// directory.
    #[test]
    fn the_group_and_the_created_date_lead_the_row_without_being_searched() {
        let (cfg, notes) = (config(), vault());
        let width = group_width(&notes, &cfg);
        assert_eq!(
            prefix(&notes[0], &cfg, width, utc()).0,
            "work     1971-02-05  "
        );
        assert_eq!(
            prefix(&notes[2], &cfg, width, utc()).0,
            "         1970-01-01  "
        );
        assert!(!row(&notes[0]).contains("work"));
        assert!(!row(&notes[0]).contains("1971"));
    }

    /// Every prefix is the same width, so the names line up under each other.
    #[test]
    fn every_prefix_is_the_same_width() {
        let (cfg, notes) = (config(), vault());
        let width = group_width(&notes, &cfg);
        let widths: Vec<usize> = notes
            .iter()
            .map(|n| prefix(n, &cfg, width, utc()).0.chars().count())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    /// The group takes its label's colour, and a directory carrying no label
    /// takes none — the same rule the row itself is drawn by.
    #[test]
    fn the_group_column_is_tinted_only_where_the_directory_carries_a_label() {
        let (cfg, notes) = (config(), vault());
        let width = group_width(&notes, &cfg);
        let tints = |note: &Note| prefix(note, &cfg, width, utc()).1;
        assert_eq!(
            tints(&notes[0]),
            vec![Tint {
                range: 0..width + COLUMN_GAP.len(),
                color: cfg.color_of("work").unwrap(),
            }]
        );
        assert!(tints(&notes[1]).is_empty());
    }

    /// A vault filed flat has nothing to say in a group column, and pays no
    /// width for one.
    #[test]
    fn a_vault_with_every_note_at_the_root_gets_no_group_column() {
        let (cfg, notes) = (config(), vec![note("inbox.md", Front::default())]);
        let width = group_width(&notes, &cfg);
        assert_eq!(width, 0);
        assert_eq!(prefix(&notes[0], &cfg, width, utc()).0, "1970-01-01  ");
    }

    /// A label still reads off the list without costing a column, the way a
    /// repository row carries its own.
    #[test]
    fn a_labelled_note_takes_its_labels_colour_and_an_unlabelled_one_takes_none() {
        let cfg = config();
        let notes = vault();
        assert_eq!(row_color(&notes[0], &cfg), cfg.color_of("work"));
        assert_eq!(row_color(&notes[1], &cfg), None);
        assert_eq!(row_color(&notes[2], &cfg), None);
    }

    #[test]
    fn the_group_column_is_the_label_or_the_directory_that_has_none() {
        let cfg = config();
        let notes = vault();
        assert_eq!(notes[0].group(&cfg), "work");
        assert!(notes[0].labelled(&cfg));
        assert_eq!(notes[1].group(&cfg), "scratch");
        assert!(!notes[1].labelled(&cfg));
        assert_eq!(notes[2].group(&cfg), "");
    }

    /// `ls` is read by other tools, so the path it prints is one they can open.
    #[test]
    fn a_status_row_leads_with_a_readable_path_and_ends_with_both_dates() {
        let cfg = config();
        let notes = vault();
        let widths = Widths::of(&notes, &cfg, "/vault");
        let row = status_row(&notes[0], &cfg, &widths, utc(), "/vault");
        assert!(row.starts_with("~/work/meetings/standup.md"), "{row:?}");
        assert!(row.contains("work"), "{row:?}");
        assert!(row.contains("#daily"), "{row:?}");
        assert!(row.ends_with("1971-02-05"), "{row:?}");
    }

    #[test]
    fn column_width_counts_characters_not_bytes() {
        let notes = vec![
            note("café.md", Front::default()),
            note("a.md", Front::default()),
        ];
        assert_eq!(
            Widths::of(&notes, &config(), "/vault").title,
            "café".chars().count()
        );
    }

    #[test]
    fn a_listing_leads_with_what_was_touched_most_recently() {
        let mut notes = vault();
        newest_first(&mut notes);
        assert_eq!(notes[0].rel, "work/meetings/standup.md");
    }

    /// The list is worth something before anything is typed: the whole vault,
    /// in the order the listing already put it in.
    #[test]
    fn nothing_typed_matches_every_note_in_the_order_it_arrived() {
        let notes = vault();
        let matched = by_name(&notes, "  ");
        assert_eq!(matched.len(), notes.len());
        assert_eq!(matched[0].rel, notes[0].rel);
    }

    /// A query matches what the row shows — the title — and the letters need
    /// only be in order, so `stnd` finds "standup".
    #[test]
    fn a_query_matches_the_name_the_row_shows() {
        let notes = vault();
        let matched = by_name(&notes, "stnd");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].rel, "work/meetings/standup.md");
    }

    #[test]
    fn a_query_nothing_is_called_matches_nothing() {
        assert!(by_name(&vault(), "zzzz").is_empty());
    }

    /// Notes that score alike keep the order the listing left them in, rather
    /// than shuffling between keystrokes.
    #[test]
    fn notes_that_score_alike_stay_newest_first() {
        let mut notes = vec![
            Note {
                modified: 0,
                ..note("older-draft.md", Front::default())
            },
            Note {
                modified: DAY,
                ..note("newer-draft.md", Front::default())
            },
        ];
        newest_first(&mut notes);

        let matched = by_name(&notes, "draft");

        assert_eq!(
            matched.iter().map(|note| &note.rel).collect::<Vec<_>>(),
            vec!["newer-draft.md", "older-draft.md"]
        );
    }

    #[test]
    fn only_markdown_is_a_note() {
        for name in ["a.md", "a.markdown", "a.MD"] {
            assert!(is_note(Path::new(name)), "{name}");
        }
        for name in ["a.png", "a.canvas", "a", "a.md.bak"] {
            assert!(!is_note(Path::new(name)), "{name}");
        }
    }

    // --- cleanup ---

    fn body_of(len: usize) -> String {
        "word ".repeat(len)
    }

    #[test]
    fn a_note_with_nothing_in_it_is_a_candidate() {
        let note = note("thoughts.md", Front::default());
        assert_eq!(junk(&note, "", scratch(), "daily"), Some(Junk::Empty));
        assert_eq!(
            junk(&note, "# Thoughts\n", scratch(), "daily"),
            Some(Junk::Empty)
        );
        assert_eq!(junk(&note, &body_of(20), scratch(), "daily"), None);
    }

    /// What an editor calls a file nobody named, and everything it goes on to
    /// call the next one.
    #[test]
    fn an_untitled_note_is_a_candidate_however_it_is_numbered() {
        for name in [
            "Untitled.md",
            "untitled 1.md",
            "Untitled-2.md",
            "untitled_3.md",
        ] {
            let note = note(name, Front::default());
            assert_eq!(
                junk(&note, &body_of(20), scratch(), "daily"),
                Some(Junk::Untitled),
                "{name}"
            );
        }
        // Not every name that begins with the word.
        for name in ["Untitled thoughts.md", "untitledness.md"] {
            let note = note(name, Front::default());
            assert_eq!(
                junk(&note, &body_of(20), scratch(), "daily"),
                None,
                "{name}"
            );
        }
    }

    /// A name with no letters in it is a timestamp somebody meant to replace —
    /// unless the front matter names the note, in which case it has a name.
    #[test]
    fn a_note_named_only_by_numbers_is_a_candidate_unless_it_says_otherwise() {
        let jotted = note("2026-08-25 1043.md", Front::default());
        assert_eq!(
            junk(&jotted, &body_of(20), scratch(), "daily"),
            Some(Junk::Unnamed)
        );

        let titled = note(
            "2026-08-25 1043.md",
            Front {
                title: Some("Bank call".into()),
                ..Front::default()
            },
        );
        assert_eq!(junk(&titled, &body_of(20), scratch(), "daily"), None);
    }

    #[test]
    fn a_daily_note_named_only_by_its_day_is_not_a_candidate_for_that() {
        let daily = note("personal/daily/2026-09-23.md", Front::default());
        assert_eq!(
            junk(&daily, &body_of(40), scratch(), "personal/daily"),
            None
        );
        assert_eq!(
            junk(&daily, "", scratch(), "personal/daily"),
            Some(Junk::Empty)
        );
    }

    /// A name is whatever the writer typed, and this repository's owner writes
    /// Norwegian. Every one of these panicked when a byte offset — the length
    /// of `untitled`, or the length of a directory plus one — landed inside a
    /// character rather than between two.
    ///
    /// Byte offsets into a name are the bug, not any one name: `&s[..8]` is a
    /// panic waiting for the eighth byte to be the middle of `ø`. These are the
    /// shapes that reach a byte offset, so this is where they are held.
    #[test]
    fn a_name_that_is_not_ascii_does_not_panic_anywhere() {
        let names = [
            "Løsninger",
            "Møtereferat",
            "Størrelsesorden",
            "ø",
            "Untitled ø",
            "Untitledø",
            "ÅRSMØTE",
            "日本語のノート",
            "café",
            "naïveté",
            "emoji 🎉 note",
            "Prosjektø",
        ];
        let cfg = config();
        for name in names {
            for rel in [
                format!("{name}.md"),
                format!("{name}/{name}.md"),
                format!("work/{name}.md"),
                format!("{name}/deeper/{name}.md"),
            ] {
                let note = note(&rel, Front::default());
                // Every accessor a row, a report or the cleanup list reaches
                // for. A panic in any is the bug this is here for.
                let _ = note.title();
                let _ = note.group(&cfg);
                let _ = note.tag_column();
                let _ = note.shown("/home/me");
                let _ = row(&note);
                let _ = prefix(&note, &cfg, 8, utc());
                let _ = junk(&note, "", scratch(), "daily");
                let _ = junk(&note, &"word ".repeat(20), scratch(), "daily");
                let widths = Widths::of(std::slice::from_ref(&note), &cfg, "/home/me");
                let _ = status_row(&note, &cfg, &widths, utc(), "/home/me");
                let _ = junk_row(&note, Junk::Untitled, 10, &widths);
            }
        }
    }

    /// The name that crashed it: `untitled` is eight bytes, and the eighth byte
    /// of a Norwegian filename is as likely as not to be half of an `ø`.
    #[test]
    fn a_name_whose_eighth_byte_is_inside_a_character_is_not_untitled() {
        for name in ["Prosjektø", "Løsningø", "abcdefgø", "Størrelse"] {
            let note = note(&format!("{name}.md"), Front::default());
            assert_ne!(
                junk(&note, &"word ".repeat(20), scratch(), "daily"),
                Some(Junk::Untitled),
                "{name}"
            );
        }
    }

    /// The reason is what the list is read for, so it leads and it is coloured.
    #[test]
    fn a_cleanup_row_says_why_the_note_is_on_the_list() {
        let notes = vault();
        let widths = Widths::of(&notes, &config(), "/home/me").with_sizes([0].into_iter());
        let (row, tints) = junk_row(&notes[0], Junk::Empty, 0, &widths);
        assert!(row.starts_with("empty"), "{row:?}");
        assert!(row.contains("work/meetings/standup.md"), "{row:?}");
        assert!(row.ends_with("0b"), "{row:?}");
        assert_eq!(tints[0].range, 0.."empty".len());
        assert_eq!(tints[0].color, Junk::Empty.color());
    }

    /// One number and one letter, so a column of sizes reads down its last
    /// character rather than being measured.
    #[test]
    fn a_size_is_one_number_and_one_letter() {
        assert_eq!(size(0), "0b");
        assert_eq!(size(512), "512b");
        assert_eq!(size(1024), "1k");
        assert_eq!(size(4096), "4k");
        assert_eq!(size(2 * 1024 * 1024), "2M");
    }

    /// The whole point of right-aligning it: the units stack, so a glance down
    /// the list tells bytes from kilobytes without reading a single number.
    #[test]
    fn every_size_ends_in_the_same_column() {
        let notes = vault();
        let sizes = [0u64, 900, 4096, 5 * 1024 * 1024];
        let widths = Widths::of(&notes, &config(), "/home/me").with_sizes(sizes.into_iter());
        let rows: Vec<String> = sizes
            .iter()
            .map(|bytes| junk_row(&notes[0], Junk::Empty, *bytes, &widths).0)
            .collect();
        let lengths: Vec<usize> = rows.iter().map(|r| r.chars().count()).collect();
        assert!(
            lengths.windows(2).all(|w| w[0] == w[1]),
            "the sizes do not line up: {rows:?}"
        );
    }

    /// Grouped by what is wrong, then smallest first — which is what makes a
    /// cleanup list answerable a group at a time rather than a row at a time.
    #[test]
    fn a_cleanup_list_is_grouped_by_reason_and_then_by_size() {
        let at = |rel: &str| note(rel, Front::default());
        let mut candidates = vec![
            (at("d.md"), Junk::Unnamed, 900),
            (at("a.md"), Junk::Empty, 12),
            (at("c.md"), Junk::Unnamed, 40),
            (at("b.md"), Junk::Untitled, 700),
            (at("e.md"), Junk::Empty, 0),
        ];
        cleanup_order(&mut candidates);

        let order: Vec<(&str, Junk, u64)> = candidates
            .iter()
            .map(|(n, reason, size)| (n.rel.as_str(), *reason, *size))
            .collect();
        assert_eq!(
            order,
            vec![
                ("e.md", Junk::Empty, 0),
                ("a.md", Junk::Empty, 12),
                ("b.md", Junk::Untitled, 700),
                ("c.md", Junk::Unnamed, 40),
                ("d.md", Junk::Unnamed, 900),
            ]
        );
    }

    /// Being empty is what the scratch note is *for*, so offering to delete it
    /// would offer that on every run.
    #[test]
    fn the_scratch_note_is_never_a_candidate() {
        let pad = Note {
            path: scratch().to_path_buf(),
            ..note("scratch/scratch.md", Front::default())
        };
        assert_eq!(junk(&pad, "", scratch(), "daily"), None);
        assert_eq!(junk(&pad, "Untitled", scratch(), "daily"), None);

        // Its neighbours are offered as usual.
        let other = note("scratch/other.md", Front::default());
        assert_eq!(junk(&other, "", scratch(), "daily"), Some(Junk::Empty));
    }

    /// Front matter is read character by character too, and a Norwegian tag is
    /// exactly as valid as an English one.
    #[test]
    fn front_matter_that_is_not_ascii_parses() {
        let parsed = front("---\ntitle: Årsmøte\ntags: [løsning, ærlig, 日本語]\n---\n");
        assert_eq!(parsed.title.as_deref(), Some("Årsmøte"));
        assert_eq!(parsed.tags, vec!["løsning", "ærlig", "日本語"]);
    }
}

// --- searching --------------------------------------------------------------

/// One line of a note that a search matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Absolute path, which is what an editor and a quickfix entry both want.
    pub path: PathBuf,
    /// Path below the vault root, which is what a row shows.
    pub rel: String,
    pub line: u32,
    pub column: u32,
    /// The matched line, as ripgrep printed it.
    pub text: String,
}

/// Read one `--vimgrep` line: `path:line:column:text`.
///
/// Split from the left exactly three times, because only the first three fields
/// are known not to contain a colon — a path may, and the matched text very
/// often does.
pub fn parse_match(line: &str, root: &Path) -> Option<Match> {
    // The path is whatever precedes the last colon that still leaves two
    // numbers and a text behind it, which reading from the right finds without
    // guessing where the path ends.
    let (head, text) = split_three(line)?;
    let (path, line_no, column) = head;
    let path = PathBuf::from(path);
    Some(Match {
        rel: path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned(),
        path,
        line: line_no,
        column,
        text: text.to_string(),
    })
}

/// `path:line:column:` and the rest, where `line` and `column` are the last two
/// colon-separated numbers before the text.
fn split_three(line: &str) -> Option<((&str, u32, u32), &str)> {
    let mut at = 0;
    // Walk the colons left to right: the first one whose next two fields are
    // numbers ends the path. A path holding `x:1:2:` before the real fields is
    // a file nobody has.
    while let Some(next) = line[at..].find(':') {
        let end = at + next;
        let rest = &line[end + 1..];
        if let Some((line_no, rest)) = take_number(rest)
            && let Some((column, rest)) = take_number(rest)
        {
            return Some(((&line[..end], line_no, column), rest));
        }
        at = end + 1;
    }
    None
}

/// A run of digits followed by a colon, and what comes after it.
fn take_number(text: &str) -> Option<(u32, &str)> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let rest = text[digits..].strip_prefix(':')?;
    Some((text[..digits].parse().ok()?, rest))
}

/// The field separator inside a search row's value.
///
/// A control character rather than a colon or a tab: the value carries a path,
/// which may hold either, and it is read back by [`decode_match`] rather than
/// by a person.
const FIELD: char = '\u{1}';

/// Pack a match into the string a selector row returns.
pub fn encode_match(found: &Match) -> String {
    format!(
        "{}{FIELD}{}{FIELD}{}{FIELD}{}",
        found.path.display(),
        found.line,
        found.column,
        crate::term::one_row(&found.text),
    )
}

/// Read back what [`encode_match`] wrote.
pub fn decode_match(value: &str, root: &Path) -> Option<Match> {
    let mut fields = value.splitn(4, FIELD);
    let path = PathBuf::from(fields.next()?);
    let line = fields.next()?.parse().ok()?;
    let column = fields.next()?.parse().ok()?;
    let text = fields.next().unwrap_or_default().to_string();
    Some(Match {
        rel: path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned(),
        path,
        line,
        column,
        text,
    })
}

/// One entry of a quickfix list, in the `file:line:column:text` shape vim's
/// `errorformat` is told to expect.
pub fn quickfix_line(found: &Match) -> String {
    format!(
        "{}:{}:{}:{}",
        found.path.display(),
        found.line,
        found.column,
        crate::term::one_row(&found.text)
    )
}

/// The widest `rel:line` in a batch of matches, so their text columns line up.
pub fn match_width(matches: &[Match]) -> usize {
    matches
        .iter()
        .map(|m| m.rel.chars().count() + 1 + m.line.to_string().chars().count())
        .max()
        .unwrap_or(0)
}

/// One search row: where the match is, then the line that matched.
///
/// The location leads for the reason a note's name does — it is what the eye
/// runs down — and is the only part coloured, since the matched text is the
/// note's own words and colouring those would be cid talking over them.
pub fn match_row(found: &Match, width: usize) -> (String, Vec<Tint>) {
    let location = format!("{}:{}", found.rel, found.line);
    let mut row = String::new();
    push_column(&mut row, &location, width);
    row.push_str(COLUMN_GAP);
    // Trimmed at the front only: the indentation of a matched line is noise in
    // a list, and its trailing text is what the match is.
    row.push_str(crate::term::one_row(found.text.trim_start()).trim_end());

    let at = found.rel.chars().count();
    (
        row,
        vec![
            Tint {
                range: 0..at,
                color: FOLDER_COLOR,
            },
            Tint {
                range: at..location.chars().count(),
                color: LINE_COLOR,
            },
        ],
    )
}

/// The colour of a `:line` suffix on a search row: yellow, as every grep-alike
/// numbers its lines.
const LINE_COLOR: u8 = 3;

// --- new notes --------------------------------------------------------------

/// Where `note new` puts a note when `[note] inbox` does not say.
pub const DEFAULT_INBOX: &str = "inbox";

/// Where `note daily` puts the day's note when `[note] daily` does not say.
pub const DEFAULT_DAILY: &str = "daily";

/// The file a note called `title` is started in: its title in kebab case. A
/// title with nothing in it a slug can use is `note.md`.
pub fn titled_name(title: &str) -> String {
    match slug(title).as_str() {
        "" => "note.md".to_string(),
        slug => format!("{slug}.md"),
    }
}

/// The file the note for `day`, a `YYYY-MM-DD` date, lives in.
pub fn daily_name(day: &str) -> String {
    format!("{day}.md")
}

/// The title the note for `day` carries.
pub fn daily_title(day: &str) -> String {
    format!("Daily - {day}")
}

/// `title` in kebab case: lowercase words joined by `-`.
///
/// Anything that is not a letter or a digit separates words, so a `/` in a
/// title cannot become a directory. Apostrophes are dropped instead, which
/// keeps `Don't` one word. Letters outside ASCII are kept rather than
/// transliterated.
pub fn slug(title: &str) -> String {
    title
        .chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// What a new note's file starts with: front matter dating it to `day`, a
/// `YYYY-MM-DD` date, then its title as the one H1.
///
/// The date lives in the front matter rather than the filename, which is where
/// a listing reads a note's creation date from.
pub fn new_note(title: &str, day: &str) -> String {
    format!("---\ncreated: {day}\n---\n\n# {}\n", title.trim())
}

/// A name nothing is using yet: `name`, else `name-2`, `name-3` and so on.
///
/// Two notes started in the same minute is not an error, and neither is one
/// name typed twice — the second is a second note.
pub fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    // Bounded only by the filesystem: a caller that finds every name taken has
    // a directory problem rather than a naming one.
    (2..)
        .map(|n| format!("{stem}-{n}{ext}"))
        .find(|candidate| !taken(candidate))
        .expect("the integers run out before the filenames do")
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/vault")
    }

    #[test]
    fn a_vimgrep_line_splits_into_a_place_and_the_text_that_matched() {
        let found = parse_match("/vault/work/a.md:12:5:the matched text", &root()).unwrap();
        assert_eq!(found.rel, "work/a.md");
        assert_eq!((found.line, found.column), (12, 5));
        assert_eq!(found.text, "the matched text");
    }

    /// The matched text is a line of prose, and prose is full of colons. Only
    /// the first three fields are known not to be.
    #[test]
    fn a_matched_line_may_contain_colons_of_its_own() {
        let found = parse_match("/vault/a.md:1:1:see also: the other note", &root()).unwrap();
        assert_eq!(found.text, "see also: the other note");
    }

    /// A path may contain a colon too, which is why the split looks for the
    /// two numbers rather than counting from the left.
    #[test]
    fn a_path_with_a_colon_in_it_still_parses() {
        let found = parse_match("/vault/a:b.md:7:2:text", &root()).unwrap();
        assert_eq!(found.path, PathBuf::from("/vault/a:b.md"));
        assert_eq!(found.line, 7);
    }

    /// A search row is built by slicing a ripgrep line at byte offsets, and a
    /// vault's paths and prose are as Norwegian as its names.
    #[test]
    fn a_match_in_a_norwegian_note_parses_and_draws() {
        let found =
            parse_match("/vault/nøtter/møte.md:12:5:ærlig talt, en løsning", &root()).unwrap();
        assert_eq!(found.rel, "nøtter/møte.md");
        assert_eq!((found.line, found.column), (12, 5));
        assert_eq!(found.text, "ærlig talt, en løsning");

        let (row, tints) = match_row(&found, 40);
        assert!(row.contains("nøtter/møte.md"), "{row:?}");
        // Tints are character ranges, so they must land on characters.
        for tint in &tints {
            assert!(
                row.chars().count() >= tint.range.end,
                "a tint ran past the row: {tint:?} in {row:?}"
            );
        }
        let back = decode_match(&encode_match(&found), &root()).unwrap();
        assert_eq!(back.rel, found.rel);
    }

    #[test]
    fn a_line_that_is_not_a_match_is_not_one() {
        for line in ["", "no colons at all", "/vault/a.md:not:a:number"] {
            assert!(parse_match(line, &root()).is_none(), "{line:?}");
        }
    }

    /// The value travels through skim as one string and comes back to be
    /// opened, so what goes in has to come out — including a path holding the
    /// characters a simpler separator would have split on.
    #[test]
    fn a_match_survives_the_trip_through_the_selector() {
        let found = Match {
            path: PathBuf::from("/vault/a:b c.md"),
            rel: "a:b c.md".into(),
            line: 41,
            column: 3,
            text: "a line with: colons and\ttabs".into(),
        };
        let back = decode_match(&encode_match(&found), &root()).unwrap();
        assert_eq!(back.path, found.path);
        assert_eq!((back.line, back.column), (41, 3));
        assert_eq!(back.rel, "a:b c.md");
    }

    /// vim reads the list with `errorformat=%f:%l:%c:%m`, so this is the one
    /// shape it parses.
    #[test]
    fn a_quickfix_entry_is_file_line_column_text() {
        let found = parse_match("/vault/a.md:12:5:text", &root()).unwrap();
        assert_eq!(quickfix_line(&found), "/vault/a.md:12:5:text");
    }

    /// A note's own bytes reach the terminal here; an escape sequence in one
    /// would otherwise repaint the list around it.
    #[test]
    fn a_matched_line_cannot_colour_the_row_it_is_drawn_in() {
        let found = parse_match("/vault/a.md:1:1:red \x1b[31mhere", &root()).unwrap();
        let (row, _) = match_row(&found, 10);
        assert!(!row.contains('\x1b'), "{row:?}");
        assert!(!quickfix_line(&found).contains('\x1b'));
    }

    #[test]
    fn a_search_row_colours_where_the_match_is_and_not_what_it_says() {
        let found = parse_match("/vault/work/a.md:12:1:some words", &root()).unwrap();
        let (row, tints) = match_row(&found, 20);
        let text = |tint: &Tint| -> String {
            row.chars()
                .skip(tint.range.start)
                .take(tint.range.len())
                .collect()
        };
        assert_eq!(text(&tints[0]), "work/a.md");
        assert_eq!(text(&tints[1]), ":12");
        assert!(row.contains("some words"));
        assert!(
            tints
                .iter()
                .all(|t| t.range.end <= "work/a.md:12".chars().count()),
            "the matched text is coloured over"
        );
    }

    // --- new notes ---

    #[test]
    fn a_titled_name_is_the_title_in_kebab_case() {
        assert_eq!(titled_name("Migrate to AWS"), "migrate-to-aws.md");
    }

    #[test]
    fn a_title_with_nothing_to_slug_is_still_a_name() {
        assert_eq!(titled_name("  ?! "), "note.md");
    }

    #[test]
    fn a_daily_note_is_named_and_titled_for_its_day() {
        assert_eq!(daily_name("2026-09-23"), "2026-09-23.md");
        assert_eq!(daily_title("2026-09-23"), "Daily - 2026-09-23");
    }

    #[test]
    fn a_slug_joins_words_on_single_hyphens() {
        let cases = [
            ("Migrate to AWS", "migrate-to-aws"),
            ("  CI/CD -- pipeline  ", "ci-cd-pipeline"),
            ("Don't panic", "dont-panic"),
            ("It\u{2019}s v2.0", "its-v2-0"),
            ("Møte med Åse", "møte-med-åse"),
            ("", ""),
        ];
        for (title, want) in cases {
            assert_eq!(slug(title), want, "{title:?}");
        }
    }

    /// Front matter the listing can read the creation date back out of, a
    /// blank line, then the title.
    #[test]
    fn a_new_note_is_front_matter_then_its_title() {
        let text = new_note(" Keychain project spec ", "2024-11-22");
        assert_eq!(
            text,
            "---\ncreated: 2024-11-22\n---\n\n# Keychain project spec\n"
        );
        let (block, body) = split_front_matter(&text);
        let front = parse_front(block.unwrap(), utc());
        assert_eq!(date(front.created.unwrap(), utc()), "2024-11-22");
        assert_eq!(body.trim(), "# Keychain project spec");
    }

    #[test]
    fn a_name_already_taken_takes_the_next_one() {
        let taken = |name: &str| ["a.md", "a-2.md"].contains(&name);
        assert_eq!(free_name("a.md", taken), "a-3.md");
        assert_eq!(free_name("b.md", taken), "b.md");
    }

    #[test]
    fn a_name_with_no_extension_is_still_given_a_free_one() {
        assert_eq!(free_name("a", |name| name == "a"), "a-2");
    }

    fn utc() -> time::UtcOffset {
        time::UtcOffset::UTC
    }
}

// --- cleanup ----------------------------------------------------------------

/// Why a note is worth a second look before it is kept.
///
/// The three shapes a note takes when it was never really written: one that was
/// started and abandoned, one the editor named because nobody did, and one
/// whose name is a timestamp somebody meant to replace.
///
/// The order the variants are written in is the order a cleanup list is read
/// in: the notes with nothing in them, which are the easiest call, before the
/// ones that only want a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Junk {
    /// Nothing in it, or so little that nothing was said.
    Empty,
    /// Called `Untitled`, which is what an editor calls a file nobody named.
    Untitled,
    /// Named without a letter in it — a date, a clock reading, a number — and
    /// carrying no front matter title either.
    Unnamed,
}

impl Junk {
    /// What the row says about it, in a couple of words.
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Untitled => "untitled",
            Self::Unnamed => "no name",
        }
    }

    /// Red for a note with nothing in it, yellow for one that has content and
    /// only wants a name — the second is a judgement and the first is not.
    pub fn color(self) -> u8 {
        match self {
            Self::Empty => 1,
            Self::Untitled | Self::Unnamed => 3,
        }
    }
}

/// How little a body can hold before there is nothing in the note.
///
/// Counted in non-whitespace characters after the front matter, so a note that
/// is a heading and nothing else — the shape a note takes when it was opened,
/// titled and abandoned — is under it.
const MIN_BODY: usize = 24;

/// Whether `note` is worth offering for deletion, and why.
///
/// Deliberately three rules and no more. Anything cleverer — a note nothing
/// links to, a note nothing has opened in a year — is a judgement about what
/// the vault is *for*, and this is a list to look through rather than a verdict.
///
/// `scratch` is the one note that is never on it, whatever it holds. The notes
/// under `daily`, a directory below the vault, are named for their day by
/// design, so a name without a letter in it is no reason to offer one.
pub fn junk(note: &Note, body: &str, scratch: &Path, daily: &str) -> Option<Junk> {
    // Being empty is what the scratch note is *for* — it is emptied every time
    // it is used up — so offering to delete it would offer that on every run.
    if note.path == scratch {
        return None;
    }
    if body.chars().filter(|c| !c.is_whitespace()).count() < MIN_BODY {
        return Some(Junk::Empty);
    }
    let name = stem(&note.rel);
    if is_untitled(name) {
        return Some(Junk::Untitled);
    }
    // A front matter title is a name, wherever the file's own name came from.
    if note.front.title.is_none()
        && !name.chars().any(char::is_alphabetic)
        && !under(&note.rel, daily)
    {
        return Some(Junk::Unnamed);
    }
    None
}

/// Whether a name is the one an editor gives a file nobody named: `Untitled`,
/// and the `Untitled 1`, `untitled-2` it goes on to.
fn is_untitled(name: &str) -> bool {
    // Character by character, never `name[..UNTITLED.len()]`. A name is
    // whatever its writer typed, and a byte offset into one is a panic waiting
    // for that byte to be the middle of a character rather than the start of
    // it — `untitled` is eight bytes, and the eighth byte of a name like
    // `Oppgaveøkt` is half of the `ø`.
    let mut rest = name.chars();
    let matched = UNTITLED.chars().all(|want| {
        rest.next()
            .is_some_and(|got| got.eq_ignore_ascii_case(&want))
    });
    if !matched {
        return false;
    }
    rest.all(|c| c.is_ascii_digit() || c == ' ' || c == '-' || c == '_')
}

const UNTITLED: &str = "untitled";

/// Order a cleanup list: by what is wrong with each note, then smallest first.
///
/// Grouping by reason is what makes the list answerable — the empty ones are
/// one decision taken together, and the ones that only want a name are another.
/// Within a group the smallest come first, since size is what separates a note
/// that was abandoned from one that was written and never titled. Ties go to
/// the path, so a listing does not shuffle between runs.
pub fn cleanup_order(candidates: &mut [(Note, Junk, u64)]) {
    candidates.sort_by(|(a, a_reason, a_size), (b, b_reason, b_size)| {
        a_reason
            .cmp(b_reason)
            .then_with(|| a_size.cmp(b_size))
            .then_with(|| a.rel.cmp(&b.rel))
    });
}

/// One row of the cleanup selector: why it is here, where it is, and how much
/// of it there is.
///
/// The reason leads because it is what the list is being read for — the same
/// three words over and over, which is exactly what makes the odd one out
/// visible. The size trails, right-aligned, so the units stack into a column
/// instead of wandering with the length of each number.
pub fn junk_row(note: &Note, reason: Junk, bytes: u64, widths: &Widths) -> (String, Vec<Tint>) {
    let label = reason.label();
    let mut row = String::new();
    push_column(&mut row, label, JUNK_REASON_WIDTH);
    row.push_str(COLUMN_GAP);
    push_column(&mut row, &note.rel, widths.rel);
    row.push_str(COLUMN_GAP);
    push_right(&mut row, &size(bytes), widths.size);

    let tints = vec![Tint {
        range: 0..label.chars().count(),
        color: reason.color(),
    }];
    (row, tints)
}

/// Append `text` padded to `width` characters on its left.
fn push_right(row: &mut String, text: &str, width: usize) {
    for _ in text.chars().count()..width {
        row.push(' ');
    }
    row.push_str(text);
}

/// Wide enough for the longest of [`Junk::label`], so the paths line up.
const JUNK_REASON_WIDTH: usize = 8;

/// A file size a person reads rather than counts.
///
/// One letter, not two: the unit is the last character of every size, so a
/// column of them lines up under itself and the eye reads down the letters
/// rather than hunting for where each number ended.
pub fn size(bytes: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * K;
    match bytes {
        b if b < K => format!("{b}b"),
        b if b < M => format!("{}k", b / K),
        b => format!("{}M", b / M),
    }
}

// --- the scratch note -------------------------------------------------------

/// Where the scratch note lives when nothing says otherwise: its own directory,
/// so a vault that files everything by folder has somewhere obvious to put the
/// one note that is filed nowhere.
pub const DEFAULT_SCRATCH: &str = "scratch/scratch.md";
