//! `cid worktree` — list and select the working trees of the repository the
//! shell is standing in.
//!
//! Switching to one is a `cd`, which a child process cannot do to its parent,
//! so `sel` prints the path and the fish integration's ctrl-t moves there —
//! the same split as `repo sel`. `add` prints its path for the same reason.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::git::{self, Landed, MARK_WIDTH, Worktree};
use crate::path::{display_path, expand_home_dir};
use crate::select::{Choice, SelectItem};
use crate::{Ctx, select, term};

/// Every working tree of the current repository, in git's order.
fn load(ctx: &Ctx) -> Result<Vec<Worktree>> {
    // Both questions — is this a repository, and which tree is the shell in —
    // out of the one `rev-parse`.
    let here = git::require_repo_root()?;
    let worktrees = git::worktrees(&here)?;
    ctx.log
        .info(&format!("found {} worktrees", worktrees.len()));
    if worktrees.is_empty() {
        bail!("no worktrees found");
    }
    Ok(worktrees)
}

/// The column widths a listing shares, so `worktree ls --status` and the
/// selector draw the same row.
///
/// Widths are counted in characters, not bytes: `{:<width$}` pads by character
/// count, so a byte length would over-pad a non-ASCII row.
struct Columns {
    head: usize,
    tags: usize,
}

impl Columns {
    fn of(worktrees: &[Worktree]) -> Self {
        Self {
            head: worktrees
                .iter()
                .map(|w| w.head_label().chars().count())
                .max()
                .unwrap_or(0),
            tags: worktrees
                .iter()
                .map(|w| w.tags().join(" ").chars().count())
                .max()
                .unwrap_or(0),
        }
    }

    /// Render one row: the current-tree marker, what the tree has checked out,
    /// any state tag, and `path` as the caller wants it shown. The tag column
    /// is dropped entirely when no tree carries one, which is the usual case.
    fn row(&self, worktree: &Worktree, path: &str) -> String {
        let marker = if worktree.current { "*" } else { " " };
        let mut row = format!(
            "{marker} {head:<width$}",
            head = worktree.head_label(),
            width = self.head,
        );
        if self.tags > 0 {
            row.push_str(&format!(
                "  {tags:<width$}",
                tags = worktree.tags().join(" "),
                width = self.tags,
            ));
        }
        row.push_str("  ");
        row.push_str(path);
        row
    }
}

/// `cid worktree ls` — print the path of each working tree, one per line,
/// home-collapsed unless `absolute`.
///
/// `--status` adds the current-tree marker, what it has checked out, and any
/// `locked` or `prunable` tag. Colour follows the same mapping as the selector
/// and is dropped when stdout is not a terminal.
pub fn ls(ctx: &Ctx, absolute: bool, status: bool) -> Result<()> {
    let worktrees = load(ctx)?;
    let color = ctx.color();
    let columns = Columns::of(&worktrees);

    let mut out = term::Listing::stdout();
    for worktree in &worktrees {
        let path = worktree.path.to_string_lossy();
        let path = display_path(&path, ctx.home_str(), absolute);
        let line = if status {
            let row = columns.row(worktree, &path);
            match worktree.color() {
                Some(index) => term::paint(&row, index, color),
                None => row,
            }
        } else {
            path
        };
        if !out.line(&line)? {
            break;
        }
    }
    out.finish()?;
    Ok(())
}

/// Build the selector rows, tinted by [`Worktree::color`]. Every row's value is
/// the absolute path, so the shell `cd`s without re-expanding `~`.
fn items(worktrees: &[Worktree], home: &str) -> Vec<SelectItem> {
    let columns = Columns::of(worktrees);
    worktrees
        .iter()
        .map(|worktree| {
            let abs = worktree.path.to_string_lossy().into_owned();
            let label = columns.row(worktree, &display_path(&abs, home, false));
            let item = SelectItem::new(label, abs.clone()).preview(select::checkout_preview(&abs));
            match worktree.color() {
                Some(color) => item.color(color),
                None => item,
            }
        })
        .collect()
}

/// `cid worktree sel` — fuzzy-select a working tree and print its absolute
/// path, which is what a shell needs to `cd` there.
pub fn sel(ctx: &Ctx) -> Result<()> {
    let worktrees = load(ctx)?;
    let choice = select::select_one(
        items(&worktrees, ctx.home_str()),
        "Select a worktree",
        &ctx.config.selector,
    )?;
    println!("{choice}");
    Ok(())
}

// --- add --------------------------------------------------------------------

/// A branch name as one directory name: `feat/x` becomes `feat-x` rather than
/// an `x` inside a `feat`, so however branches are named the trees stay one
/// flat list to look at.
fn slug(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Where a new tree for `branch` goes.
///
/// A relative root is inside the repository, so a tree sits beside the checkout
/// it came from and goes when that does. An absolute one holds the trees of
/// every repository, and would collide on the first two branches named `main`,
/// so the repository's own directory name is a level of it.
fn tree_path(repo_root: &Path, root: &Path, branch: &str) -> PathBuf {
    let slug = slug(branch);
    if !root.is_absolute() {
        return repo_root.join(root).join(slug);
    }
    match repo_root.file_name() {
        Some(name) => root.join(name).join(slug),
        None => root.join(slug),
    }
}

/// Fuzzy-select the branch a new tree checks out, accepting a name that matches
/// nothing — which is how a tree for work that has not started yet is made.
fn choose_branch(ctx: &Ctx, branches: &[git::Branch]) -> Result<String> {
    let items = items_for_branches(branches);
    match select::select_one_or_query(items, "Branch (type a new name)", &ctx.config.selector)? {
        Choice::Item(name) => Ok(name),
        Choice::Query(typed) => Ok(typed),
    }
}

/// Selector rows for the branch list: the name, and what it last carried.
fn items_for_branches(branches: &[git::Branch]) -> Vec<SelectItem> {
    let width = branches
        .iter()
        .map(|b| b.name.chars().count())
        .max()
        .unwrap_or(0);
    branches
        .iter()
        .map(|branch| {
            let label = format!(
                "{name:<width$}  {date}  {subject}",
                name = branch.name,
                date = branch.date,
                subject = branch.subject,
            );
            SelectItem::new(label.trim_end(), branch.name.clone()).color(branch.kind.color())
        })
        .collect()
}

/// `cid worktree add [BRANCH]` — create a working tree, selecting or naming
/// the branch it checks out.
///
/// The path is cid's to decide (see [`tree_path`]) and is printed on stdout,
/// so the tree can be entered with `cd (cid worktree add feat/x)`. git's own
/// narration goes to stderr, where it does not get in the way of that.
pub fn add(ctx: &Ctx, branch: Option<&str>) -> Result<()> {
    let repo_root = git::require_repo_root()?;
    let branches = git::branches()?;

    let input = match branch {
        Some(branch) => branch.to_string(),
        None => choose_branch(ctx, &branches)?,
    };
    let source = git::tree_source(&branches, &input);
    ctx.log.info(&format!("tree source: {source:?}"));

    let path = new_tree_path(ctx, &repo_root, source.branch())?;
    git::add_worktree(&path, &source)?;
    exclude_root(ctx, &repo_root);

    println!("{}", path.display());
    Ok(())
}

/// Where a new tree for `branch` goes under `[worktree] root`, refusing a path
/// that is already there. Shared with `pr checkout --worktree`, so a tree
/// reaches the same place whichever command made it.
pub(crate) fn new_tree_path(ctx: &Ctx, repo_root: &Path, branch: &str) -> Result<PathBuf> {
    let root = expand_home_dir(&ctx.config.worktree.root, ctx.home());
    let path = tree_path(repo_root, &root, branch);
    if path.exists() {
        bail!(
            "{} already exists — `cid worktree sel` will take you there",
            path.display()
        );
    }
    Ok(path)
}

/// Hide a `[worktree] root` inside the repository from the repository itself,
/// once a tree has been made there.
///
/// Only for a relative root: an absolute one is nobody's working copy and there
/// is nothing to hide from `git status`.
pub(crate) fn exclude_root(ctx: &Ctx, repo_root: &Path) {
    if expand_home_dir(&ctx.config.worktree.root, ctx.home()).is_absolute() {
        return;
    }
    match git::ignore_locally(repo_root, &ctx.config.worktree.root) {
        Ok(true) => eprintln!(
            "note: added `{}/` to this clone's .git/info/exclude",
            ctx.config.worktree.root
        ),
        Ok(false) => {}
        Err(err) => ctx
            .log
            .warn(&format!("could not write info/exclude: {err:#}")),
    }
}

// --- rm ---------------------------------------------------------------------

/// The trees `rm` may offer: neither the main tree, which git will not remove,
/// nor the one the shell is standing in, which it will not remove either.
fn removable(worktrees: &[Worktree]) -> Vec<&Worktree> {
    worktrees
        .iter()
        .skip(1) // git lists the main tree first, and it cannot be removed
        .filter(|worktree| !worktree.current)
        .collect()
}

/// What `rm` will do with one tree: its path as given, and the branch it has
/// checked out when that branch goes too.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Removal {
    path: String,
    branch: Option<(String, Landed)>,
}

/// The tree at `path`, compared through `resolve` for the reason
/// [`git::mark_current`] does: a path typed at the prompt may reach the tree
/// through a symlink git has already resolved.
fn tree_at<'a>(
    worktrees: &'a [Worktree],
    path: &Path,
    resolve: impl Fn(&Path) -> PathBuf,
) -> Option<&'a Worktree> {
    let wanted = resolve(path);
    worktrees.iter().find(|tree| resolve(&tree.path) == wanted)
}

/// Pair each target with the branch that goes with it, when `with_branch`.
/// A detached or bare tree has no branch to take, and a path that is no tree
/// is left for git to refuse.
fn plan(
    targets: Vec<String>,
    worktrees: &[Worktree],
    branches: &[git::Branch],
    merged: &HashSet<String>,
    with_branch: bool,
    resolve: impl Fn(&Path) -> PathBuf,
) -> Vec<Removal> {
    targets
        .into_iter()
        .map(|path| {
            let branch = with_branch
                .then(|| tree_at(worktrees, Path::new(&path), &resolve))
                .flatten()
                .filter(|tree| !tree.branch.is_empty())
                .map(|tree| {
                    let landed = Landed::of(&tree.branch, branches, merged);
                    (tree.branch.clone(), landed)
                });
            Removal { path, branch }
        })
        .collect()
}

/// `cid worktree rm [PATH]...` — remove working trees, selecting them when
/// none are named.
///
/// What will go is printed before the question is put, as `file prune` does:
/// "remove 2 trees?" is answerable only by someone who has seen the two. The
/// branches they had checked out are left alone unless `with_branch`, and are
/// then deleted the way `cid branch rm` deletes them — forced where git cannot
/// see they have landed, since the list said so and was agreed to.
pub fn remove(
    ctx: &Ctx,
    paths: &[String],
    force: bool,
    with_branch: bool,
    yes: bool,
) -> Result<()> {
    let worktrees = load(ctx)?;
    let branches = git::branches()?;
    let merged = git::merged_branches()?;

    let targets: Vec<String> = if paths.is_empty() {
        let offered = removable(&worktrees);
        if offered.is_empty() {
            bail!("no worktrees to remove — only the main tree and the one you are in");
        }
        let rows: Vec<SelectItem> = {
            let owned: Vec<Worktree> = offered.into_iter().cloned().collect();
            rm_items(&owned, &branches, &merged, ctx.home_str())
        };
        match select::select_many(rows, "Remove worktrees", &ctx.config.selector) {
            Ok(selected) => selected,
            Err(e) if e.is::<select::Cancelled>() => return Ok(()),
            Err(e) => return Err(e),
        }
    } else {
        paths.to_vec()
    };
    if targets.is_empty() {
        return Ok(());
    }
    let removals = plan(
        targets,
        &worktrees,
        &branches,
        &merged,
        with_branch,
        |path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    );

    if !confirm_removal(ctx, &removals, yes)? {
        return Ok(());
    }

    let (mut trees_failed, mut branches_failed) = (0, 0);
    for removal in &removals {
        match git::remove_worktree(Path::new(&removal.path), force) {
            Ok(()) => println!(
                "Removed {}",
                display_path(&removal.path, ctx.home_str(), false)
            ),
            // git says why on the terminal it was handed; a second sentence
            // from cid over the top of it would only be vaguer.
            Err(_) => {
                trees_failed += 1;
                continue;
            }
        }
        let Some((name, landed)) = &removal.branch else {
            continue;
        };
        match git::delete_branch(name, *landed != Landed::Merged) {
            Ok(()) => println!("Deleted {name}"),
            Err(err) => {
                branches_failed += 1;
                eprintln!("error: {name}: {err:#}");
            }
        }
    }
    if trees_failed > 0 {
        bail!(
            "{trees_failed} of {} worktrees could not be removed",
            removals.len()
        );
    }
    if branches_failed > 0 {
        bail!("{branches_failed} branches could not be deleted");
    }
    Ok(())
}

/// Selector rows for removal: the usual row, with what is known about whether
/// each tree's branch has landed ahead of the path — a tree whose branch is
/// gone upstream is usually the finished one being looked for.
fn rm_items(
    worktrees: &[Worktree],
    branches: &[git::Branch],
    merged: &HashSet<String>,
    home: &str,
) -> Vec<SelectItem> {
    let columns = Columns::of(worktrees);
    worktrees
        .iter()
        .map(|worktree| {
            let abs = worktree.path.to_string_lossy().into_owned();
            let mark = match worktree.branch.as_str() {
                "" => "",
                branch => Landed::of(branch, branches, merged).mark(),
            };
            let row = columns.row(
                worktree,
                &format!("{mark:<MARK_WIDTH$}  {}", display_path(&abs, home, false)),
            );
            let item = SelectItem::new(row, abs.clone()).preview(select::checkout_preview(&abs));
            match worktree.color() {
                Some(color) => item.color(color),
                None => item,
            }
        })
        .collect()
}

/// Show what is about to go and ask, unless `yes`.
fn confirm_removal(ctx: &Ctx, removals: &[Removal], yes: bool) -> Result<bool> {
    let color = ctx.color();
    let paths: Vec<String> = removals
        .iter()
        .map(|removal| display_path(&removal.path, ctx.home_str(), false))
        .collect();
    let width = paths.iter().map(|p| p.chars().count()).max().unwrap_or(0);

    let mut out = term::Listing::stdout();
    for (removal, path) in removals.iter().zip(&paths) {
        let line = match &removal.branch {
            Some((name, landed)) => format!(
                "{path:<width$}  {}",
                term::paint(&format!("{name}  {}", landed.mark()), landed.color(), color)
            ),
            None => path.clone(),
        };
        if !out.line(line.trim_end())? {
            return Ok(false);
        }
    }
    out.finish()?;

    let branches = removals.iter().filter(|r| r.branch.is_some()).count();
    match term::Confirm::resolve(yes) {
        term::Confirm::Assumed => Ok(true),
        term::Confirm::Ask => {
            let trees = if removals.len() == 1 {
                "worktree"
            } else {
                "worktrees"
            };
            let question = match branches {
                0 => format!("Remove {} {trees}?", removals.len()),
                1 => format!("Remove {} {trees} and 1 branch?", removals.len()),
                n => format!("Remove {} {trees} and {n} branches?", removals.len()),
            };
            let answer = term::confirm(&question)?;
            if !answer {
                println!("Nothing removed");
            }
            Ok(answer)
        }
        term::Confirm::Impossible => bail!(
            "no terminal to ask for confirmation on — pass `--yes` to remove without being asked"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::Preview;

    fn worktrees() -> Vec<Worktree> {
        git::mark_current(
            git::parse_worktrees(
                "worktree /home/u/dev/cid\n\
                 HEAD 950547ef3af47b2e60406bd23e530bdb1e226c6e\n\
                 branch refs/heads/main\n\
                 \n\
                 worktree /home/u/dev/cid/.claude/worktrees/feat\n\
                 HEAD 32bb788aa1c04d9ee4d1e5a8b0e0b8d1c2f3a4b5\n\
                 branch refs/heads/feat/x\n",
            ),
            Some(std::path::Path::new("/home/u/dev/cid")),
            std::path::Path::to_path_buf,
        )
    }

    #[test]
    fn rows_mark_the_current_tree_and_return_its_path() {
        let items = items(&worktrees(), "/home/u");
        assert!(items[0].label.starts_with("* main"), "{}", items[0].label);
        assert!(items[1].label.starts_with("  feat/x"), "{}", items[1].label);
        assert_eq!(items[0].value(), "/home/u/dev/cid");
        assert_eq!(items[0].color, Some(2), "the current tree is green");
        assert_eq!(items[1].color, None);
    }

    /// The label is what the shell would have to re-expand; the value is not.
    #[test]
    fn rows_show_a_collapsed_path_and_return_an_absolute_one() {
        let items = items(&worktrees(), "/home/u");
        assert!(items[0].label.ends_with("~/dev/cid"), "{}", items[0].label);
        assert_eq!(items[0].value(), "/home/u/dev/cid");
    }

    #[test]
    fn columns_align() {
        let items = items(&worktrees(), "/home/u");
        assert_eq!(
            items[0].label.find("~/dev/cid"),
            items[1].label.find("~/dev/cid"),
            "the path column is ragged: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>(),
        );
    }

    /// The tag column costs two spaces on every row, which the common
    /// repository — no locked or prunable tree — should not pay.
    #[test]
    fn the_tag_column_appears_only_when_a_tree_carries_a_tag() {
        let plain = worktrees();
        let untagged = Columns::of(&plain).row(&plain[0], "~/p");
        assert_eq!(untagged, "* main    ~/p", "main is padded to `feat/x`");

        let mut tagged = plain;
        tagged[1].locked = true;
        let columns = Columns::of(&tagged);
        assert_eq!(columns.row(&tagged[1], "~/q"), "  feat/x  locked  ~/q");
        assert!(
            columns.row(&tagged[0], "~/p").len() > untagged.len(),
            "the untagged row did not make room for the tag column",
        );
    }

    #[test]
    fn column_width_counts_characters_not_bytes() {
        let rows = git::parse_worktrees(
            "worktree /home/u/café\nHEAD abc\nbranch refs/heads/café\n\
             worktree /home/u/b\nHEAD def\nbranch refs/heads/b\n",
        );
        let label = &items(&rows, "/home/u")[0].label;
        assert!(
            label.starts_with("  café  ~/café"),
            "column padded past the longest name: {label:?}",
        );
    }

    #[test]
    fn a_branch_becomes_one_directory_rather_than_a_nest() {
        assert_eq!(slug("feat/x"), "feat-x");
        assert_eq!(slug("release/1.2/rc"), "release-1.2-rc");
        assert_eq!(slug("main"), "main");
    }

    #[test]
    fn a_relative_root_puts_the_tree_beside_the_checkout() {
        let path = tree_path(
            Path::new("/home/u/dev/github.com/me/cid"),
            Path::new(".worktrees"),
            "feat/x",
        );
        assert_eq!(
            path,
            PathBuf::from("/home/u/dev/github.com/me/cid/.worktrees/feat-x")
        );
    }

    /// One directory of trees for every repository would have two `main`s the
    /// moment a second repository used it.
    #[test]
    fn an_absolute_root_keeps_each_repository_apart() {
        let path = tree_path(
            Path::new("/home/u/dev/github.com/me/cid"),
            Path::new("/home/u/dev/worktrees"),
            "main",
        );
        assert_eq!(path, PathBuf::from("/home/u/dev/worktrees/cid/main"));
    }

    #[test]
    fn neither_the_main_tree_nor_the_one_you_are_in_is_offered_for_removal() {
        let mut trees = worktrees();
        trees[1].current = true;
        assert!(
            removable(&trees).is_empty(),
            "offered a tree git would refuse to remove"
        );

        let trees = worktrees();
        let offered: Vec<&str> = removable(&trees)
            .iter()
            .map(|w| w.branch.as_str())
            .collect();
        assert_eq!(offered, vec!["feat/x"], "the main tree was offered");
    }

    #[test]
    fn preview_reads_the_tree_without_taking_its_index_lock() {
        let items = items(&worktrees(), "/home/u");
        let Some(Preview::Command(cmd)) = &items[0].preview else {
            panic!("expected a command preview");
        };
        assert!(cmd.contains("-C '/home/u/dev/cid'"), "{cmd}");
        assert!(cmd.contains("--no-optional-locks"), "{cmd}");
        assert!(cmd.contains("--max-count=20"), "unbounded log: {cmd}");
    }

    fn gone_feat() -> Vec<git::Branch> {
        git::classify(&[git::RefLine {
            refname: "refs/heads/feat/x".into(),
            head: false,
            symref: String::new(),
            upstream: "origin/feat/x".into(),
            date: "1 day ago".into(),
            subject: "done".into(),
        }])
    }

    const TREE: &str = "/home/u/dev/cid/.claude/worktrees/feat";

    #[test]
    fn a_removal_takes_no_branch_unless_asked() {
        let removals = plan(
            vec![TREE.into()],
            &worktrees(),
            &gone_feat(),
            &HashSet::new(),
            false,
            Path::to_path_buf,
        );
        assert_eq!(removals[0].branch, None);
    }

    #[test]
    fn a_removal_with_its_branch_says_whether_that_branch_has_landed() {
        let removals = plan(
            vec![TREE.into()],
            &worktrees(),
            &gone_feat(),
            &HashSet::new(),
            true,
            Path::to_path_buf,
        );
        assert_eq!(
            removals[0].branch,
            Some(("feat/x".to_string(), Landed::Gone))
        );
    }

    #[test]
    fn a_tree_is_found_by_the_path_it_resolves_to() {
        let trees = worktrees();
        let resolve = |path: &Path| {
            PathBuf::from(
                path.to_string_lossy()
                    .replace("/link/", "/home/u/dev/cid/.claude/worktrees/"),
            )
        };
        let found = tree_at(&trees, Path::new("/link/feat"), resolve);
        assert_eq!(found.map(|tree| tree.branch.as_str()), Some("feat/x"));
    }

    #[test]
    fn a_path_that_is_no_tree_and_a_detached_tree_bring_no_branch() {
        let mut trees = worktrees();
        trees[1].branch.clear();
        let removals = plan(
            vec![TREE.into(), "/elsewhere".into()],
            &trees,
            &gone_feat(),
            &HashSet::new(),
            true,
            Path::to_path_buf,
        );
        assert!(removals.iter().all(|removal| removal.branch.is_none()));
    }

    #[test]
    fn removal_rows_mark_a_tree_whose_branch_is_gone() {
        let rows = rm_items(&worktrees()[1..], &gone_feat(), &HashSet::new(), "/home/u");
        assert!(rows[0].label.contains("upstream gone"), "{}", rows[0].label);
        assert_eq!(rows[0].value(), TREE);
    }
}
