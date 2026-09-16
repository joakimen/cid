//! `cid edit` — fuzzy-find a file or a directory and open it in your editor.
//!
//! Unlike the other groups, this one is not a registry over a set cid
//! maintains: `file` and `dir` name what is being *looked for* in the tree the
//! user is standing in, so neither has an `ls`. `--tracked` is the one
//! exception, pointing `file` at the known-files list instead.
//!
//! Nothing here needs shell integration: running an editor works perfectly well
//! from a child process, and `cd` is what [`crate::cmd::repo`] is for.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, anyhow, bail};

use crate::path::{display_path, expand_tilde};
use crate::select::{Preview, SelectItem, file_preview};
use crate::{Ctx, Reported, files, select, stats, walk};

/// `cid edit file [FILE]...` — open `paths`, or select interactively when
/// empty.
///
/// With `tracked`, selection comes from the known-files list; otherwise from
/// the current directory tree. Multiple selections open together.
pub fn file(ctx: &Ctx, paths: &[String], tracked: bool) -> Result<()> {
    open_or_select(ctx, paths, || {
        if tracked {
            select_tracked(ctx)
        } else {
            select_files(ctx)
        }
    })
}

/// `cid edit dir [DIR]...` — open `paths`, or select interactively when
/// empty.
///
/// What opening a directory means is the editor's business; cid's part is
/// finding it without a `cd` and a `ls` per level.
pub fn dir(ctx: &Ctx, paths: &[String]) -> Result<()> {
    open_or_select(ctx, paths, || select_dirs(ctx))
}

/// Open `paths`, or whatever `select` yields when there are none.
fn open_or_select(
    ctx: &Ctx,
    paths: &[String],
    select: impl FnOnce() -> Result<Option<Vec<String>>>,
) -> Result<()> {
    let targets = if paths.is_empty() {
        match select()? {
            Some(targets) => targets,
            None => return Ok(()),
        }
    } else {
        paths.to_vec()
    };

    if targets.is_empty() {
        return Ok(());
    }

    open(ctx, &targets)
}

/// Choose files from the current directory tree, streamed into the selector so
/// it opens on the first filename rather than the last.
fn select_files(ctx: &Ctx) -> Result<Option<Vec<String>>> {
    let items =
        walk::files(Path::new(".")).map(|file| SelectItem::plain(file).preview(Preview::File));

    cancellable(select::select_many_streamed(
        items,
        "Edit",
        true,
        &ctx.config.selector,
    ))
}

/// Choose directories from the current directory tree.
///
/// [`select_files`] over [`walk::dirs`], previewing what is inside each one:
/// `bat` on a directory is an error message.
fn select_dirs(ctx: &Ctx) -> Result<Option<Vec<String>>> {
    let items = walk::dirs(Path::new(".")).map(|dir| SelectItem::plain(dir).preview(Preview::Dir));

    cancellable(select::select_many_streamed(
        items,
        "Edit directory",
        true,
        &ctx.config.selector,
    ))
}

/// Choose files from the known-files list.
fn select_tracked(ctx: &Ctx) -> Result<Option<Vec<String>>> {
    ctx.ensure_files_migrated()?;
    let lines = files::read_lines(&ctx.files_path)?;
    if lines.is_empty() {
        bail!("no known files yet — add one with `cid file add <path>`");
    }

    // A `~`-collapsed label, but the editor gets the absolute path.
    let items = lines
        .iter()
        .map(|line| {
            let abs = expand_tilde(line, ctx.home_str());
            let shown = display_path(&abs, ctx.home_str(), false);
            SelectItem::new(shown, abs.clone()).preview(file_preview(&abs))
        })
        .collect();

    let (items, now) = ctx.by_recency(items, |row| row.value());
    let chosen = cancellable(select::select_many(items, "Edit", &ctx.config.selector))?;
    // Every file opened together counts: the next selector is being asked
    // which of them you reach for, not which you happened to list first.
    for file in chosen.iter().flatten() {
        ctx.remember(file, now);
    }
    Ok(chosen)
}

/// Map a cancelled selector to `None`, leaving real errors alone.
fn cancellable(result: Result<Vec<String>>) -> Result<Option<Vec<String>>> {
    match result {
        Ok(selected) => Ok(Some(selected)),
        Err(e) if e.is::<select::Cancelled>() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Launch `$VISUAL`/`$EDITOR` on `targets`.
fn open(ctx: &Ctx, targets: &[String]) -> Result<()> {
    launch(ctx, &ctx.editor()?, targets)
}

/// Launch `editor` — a program and its arguments — on `targets`, inheriting the
/// terminal. A non-zero exit is [`Reported`], since the editor has already had
/// the terminal and said whatever it had to say on it.
///
/// Shared with [`crate::cmd::note`], which launches a different command over
/// the same contract: `--` first, the terminal inherited, the child's status
/// passed through.
pub(crate) fn launch(ctx: &Ctx, editor: &[String], targets: &[String]) -> Result<()> {
    let (program, args) = editor
        .split_first()
        .expect("an editor command is resolved non-empty");

    ctx.log
        .info(&format!("opening {} file(s) with {program}", targets.len()));

    // The editor owns the terminal until the user leaves it, which is their
    // time in the same way a selector left open is.
    let _waiting = stats::interacting();
    let status = Command::new(program)
        .args(args)
        // `--` first: the walk yields relative paths, so a file named `-c`
        // arrives as exactly that, and to vim `-c` is an Ex command. clap
        // cannot catch it on the selector path, which never reaches clap.
        .arg("--")
        .args(targets)
        .status()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => anyhow!("`{program}` was not found on PATH"),
            _ => anyhow!(e).context(format!("running {program}")),
        })?;

    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selection_passes_through() {
        let selected = vec!["a.rs".to_string(), "b.rs".to_string()];
        assert_eq!(cancellable(Ok(selected.clone())).unwrap(), Some(selected));
    }

    #[test]
    fn a_cancelled_selector_is_nothing_to_open_rather_than_an_error() {
        assert_eq!(cancellable(Err(select::Cancelled.into())).unwrap(), None);
    }

    #[test]
    fn any_other_error_is_left_alone() {
        let err = cancellable(Err(anyhow!("walk failed"))).unwrap_err();
        assert_eq!(err.to_string(), "walk failed");
    }
}
