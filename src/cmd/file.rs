//! `cid file` — manage the list of files you visit regularly (formerly `kf`).

use std::path::Path;

use anyhow::{Result, bail};

use crate::path::{display_path, expand_tilde, sanitize_file_path};
use crate::select::{Preview, SelectItem, file_preview};
use crate::term;
use crate::{Ctx, files, select, walk};

/// `cid file ls` — print known files, optionally with existence status.
pub fn ls(ctx: &Ctx, status: bool, missing: bool, exists: bool) -> Result<()> {
    ctx.ensure_files_migrated()?;
    let lines = files::read_lines(&ctx.files_path)?;

    let mut out = term::Listing::stdout();

    // Plain listing: expand `~` and print, nothing else.
    if !status && !missing && !exists {
        for line in &lines {
            if !out.line(&expand_tilde(line, ctx.home_str()))? {
                break;
            }
        }
        return Ok(out.finish()?);
    }

    let use_color = status && ctx.color();

    for line in &lines {
        let expanded = expand_tilde(line, ctx.home_str());
        let present = Path::new(&expanded).exists();

        if missing && present {
            continue;
        }
        if exists && !present {
            continue;
        }

        let row = if status {
            status_row(&expanded, present, use_color)
        } else {
            expanded
        };
        if !out.line(&row)? {
            break;
        }
    }
    out.finish()?;
    Ok(())
}

/// A file with its existence marked: green tick for there, red cross for gone.
/// Shared by `file ls --status` and `file prune`, and the same glyphs and
/// colour indices `doctor` marks a passing and a failing row with.
fn status_row(path: &str, present: bool, color: bool) -> String {
    let (glyph, tint) = if present { ("✓", 2) } else { ("✗", 1) };
    term::paint(&format!("{glyph} {path}"), tint, color)
}

/// `cid file prune` — drop the tracked files that are no longer there. What
/// will go is printed before the question is asked, since "remove 4 entries?"
/// is answerable only by someone who already knows which four.
pub fn prune(ctx: &Ctx, yes: bool) -> Result<()> {
    ctx.ensure_files_migrated()?;
    let lines = files::read_lines(&ctx.files_path)?;

    let (kept, missing) = files::partition_missing(&lines, |line| {
        Path::new(&expand_tilde(line, ctx.home_str())).exists()
    });
    if missing.is_empty() {
        println!("Nothing to prune — every tracked file is still there");
        return Ok(());
    }

    let mut out = term::Listing::stdout();
    for line in &missing {
        let expanded = expand_tilde(line, ctx.home_str());
        if !out.line(&status_row(&expanded, false, ctx.color()))? {
            return Ok(());
        }
    }
    // Emptied before the question is put: "remove 4 entries?" is answerable
    // only by someone who has already seen the four.
    out.finish()?;

    match term::Confirm::resolve(yes) {
        term::Confirm::Assumed => {}
        term::Confirm::Ask => {
            let question = format!(
                "Remove {} {} from the list?",
                missing.len(),
                if missing.len() == 1 {
                    "entry"
                } else {
                    "entries"
                }
            );
            if !term::confirm(&question)? {
                println!("Nothing removed");
                return Ok(());
            }
        }
        term::Confirm::Impossible => bail!(
            "no terminal to ask for confirmation on — pass `--yes` to prune without being asked"
        ),
    }

    files::write_lines(&ctx.files_path, &kept)?;
    for line in &missing {
        println!("Removed {line}");
    }
    Ok(())
}

/// `cid file add [path]` — record a file, canonicalising the path first.
///
/// With no `file`, a file is chosen interactively from the current directory
/// tree via the configured selector.
pub fn add(ctx: &Ctx, file: Option<&str>) -> Result<()> {
    ctx.ensure_files_migrated()?;

    let file = match file {
        Some(file) => file.to_string(),
        None => match select_from_cwd(ctx)? {
            Some(file) => file,
            None => return Ok(()),
        },
    };

    // One path per line, so a path containing a newline would come back as two
    // entries that `file rm` can never match. Refused on the way in.
    if let Some(control) = file.chars().find(|c| c.is_control()) {
        bail!(
            "the known-files list is one path per line, so it cannot hold a path \
             containing {}",
            match control {
                '\n' => "a newline".to_string(),
                '\t' => "a tab".to_string(),
                c => format!("the control character U+{:04X}", c as u32),
            }
        );
    }

    let sanitized = sanitize_file_path(&file, ctx.home_str(), ctx.pwd_str());
    let expanded = expand_tilde(&sanitized, ctx.home_str());
    if !Path::new(&expanded).exists() {
        eprintln!("warning: {expanded} does not exist");
    }

    let mut lines = files::read_lines(&ctx.files_path)?;
    if lines.iter().any(|line| line == &sanitized) {
        bail!("entry already exists in the known-files list");
    }

    lines.push(sanitized.clone());
    files::write_lines(&ctx.files_path, &lines)?;
    println!("Added {sanitized}");
    Ok(())
}

/// `cid file rm [path]` — remove a file, by argument or interactively.
pub fn remove(ctx: &Ctx, file: Option<&str>) -> Result<()> {
    ctx.ensure_files_migrated()?;
    match file {
        Some(file) => remove_by_arg(ctx, file),
        None => remove_interactive(ctx),
    }
}

fn remove_by_arg(ctx: &Ctx, file: &str) -> Result<()> {
    let sanitized = sanitize_file_path(file, ctx.home_str(), ctx.pwd_str());

    let lines = files::read_lines(&ctx.files_path)?;
    let (kept, removed) = files::partition_remove(&lines, std::slice::from_ref(&sanitized));
    if removed.is_empty() {
        println!("No matching entry found");
        return Ok(());
    }

    files::write_lines(&ctx.files_path, &kept)?;
    println!("Removed {sanitized}");
    Ok(())
}

fn remove_interactive(ctx: &Ctx) -> Result<()> {
    let lines = files::read_lines(&ctx.files_path)?;
    if lines.is_empty() {
        println!("No known files");
        return Ok(());
    }

    // The returned value is the stored line, so it matches for removal.
    let items: Vec<SelectItem> = lines
        .iter()
        .map(|line| {
            let expanded = expand_tilde(line, ctx.home_str());
            let shown = display_path(&expanded, ctx.home_str(), false);
            SelectItem::new(shown, line.clone()).preview(file_preview(&expanded))
        })
        .collect();

    let selected = match select::select_many(items, "Select files to remove", &ctx.config.selector)
    {
        Ok(selected) => selected,
        Err(e) if e.is::<select::Cancelled>() => return Ok(()),
        Err(e) => return Err(e),
    };

    let (kept, removed) = files::partition_remove(&lines, &selected);
    if removed.is_empty() {
        return Ok(());
    }

    files::write_lines(&ctx.files_path, &kept)?;
    for file in &removed {
        println!("Removed {file}");
    }
    Ok(())
}

/// `cid file sel` — fuzzy-select a known file and print its absolute path.
///
/// The selector shows `~`-collapsed paths; the printed path is absolute.
pub fn sel(ctx: &Ctx) -> Result<()> {
    ctx.ensure_files_migrated()?;
    let lines = files::read_lines(&ctx.files_path)?;
    if lines.is_empty() {
        bail!("no known files yet — add one with `cid file add <path>`");
    }

    let items: Vec<SelectItem> = lines
        .iter()
        .map(|line| {
            let abs = expand_tilde(line, ctx.home_str());
            let shown = display_path(&abs, ctx.home_str(), false);
            SelectItem::new(shown, abs.clone()).preview(file_preview(&abs))
        })
        .collect();

    let (items, now) = ctx.by_recency(items, |row| row.value());
    let choice = select::select_one(items, "Select a file", &ctx.config.selector)?;
    ctx.remember(&choice, now);
    println!("{choice}");
    Ok(())
}

/// Interactively choose a file from the current directory tree to add. Returns
/// `Ok(None)` when the user cancels the selector.
fn select_from_cwd(ctx: &Ctx) -> Result<Option<String>> {
    let items =
        walk::files(Path::new(".")).map(|file| SelectItem::plain(file).preview(Preview::File));
    match select::select_one_streamed(items, "Add a file", true, &ctx.config.selector) {
        Ok(choice) => Ok(Some(choice)),
        Err(e) if e.is::<select::Cancelled>() => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The glyph carries the answer on its own, so `--color never` and a
    /// terminal whose theme eats one of the two hues both still read.
    #[test]
    fn the_status_glyph_says_what_the_colour_says() {
        for color in [true, false] {
            assert!(status_row("/a", true, color).contains("✓ /a"));
            assert!(status_row("/a", false, color).contains("✗ /a"));
        }
    }

    #[test]
    fn an_uncoloured_row_is_the_glyph_and_the_path_and_nothing_else() {
        assert_eq!(status_row("~/notes/a.md", true, false), "✓ ~/notes/a.md");
        assert_eq!(status_row("~/notes/a.md", false, false), "✗ ~/notes/a.md");
    }

    #[test]
    fn a_coloured_row_returns_the_terminal_to_its_default() {
        for present in [true, false] {
            let row = status_row("/a", present, true);
            assert!(row.starts_with("\x1b["), "{row:?}");
            assert!(row.ends_with("\x1b[0m"), "{row:?}");
        }
    }
}
