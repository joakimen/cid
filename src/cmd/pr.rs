//! `cid pr` — list, select, check out, open, and merge GitHub pull requests.
//!
//! Everything here goes through the `gh` CLI (see [`crate::gh`]), so cid
//! inherits whatever authentication `gh auth login` set up — including SSO and
//! GitHub Enterprise hosts — and stores no credentials of its own.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use crate::Ctx;
use crate::cmd::worktree;
use crate::gh::{self, MergeMethod, PullRequest};
use crate::git;
use crate::select::{self, Preview, SelectItem};
use crate::term;

/// Fail before `gh` does when there is no repository for it to be about.
///
/// `gh` works this out from the directory it runs in, and outside one reports
/// git's own `fatal: not a git repository` — a sentence about git, from a
/// command the user asked about pull requests. `GH_REPO` names a repository
/// without a checkout, and is `gh`'s to act on, so it is left alone.
fn ensure_target(ctx: &Ctx) -> Result<()> {
    if ctx.gh_repo().is_some() || git::repo_root().is_some() {
        return Ok(());
    }
    bail!(
        "not inside a git repository — `pr` acts on the repository you are \
         standing in, or the one `GH_REPO` names"
    )
}

/// Fetch pull requests, failing with a useful message when there are none.
fn collect(ctx: &Ctx, state: &str, limit: usize) -> Result<Vec<PullRequest>> {
    let prs = {
        let _spinner = term::spinner("loading pull requests", ctx.color());
        gh::list(state, limit)?
    };
    ctx.log.info(&format!("found {} pull requests", prs.len()));
    if prs.len() == limit {
        // A full page is indistinguishable from a repository with exactly that
        // many, so the filter names itself rather than quietly hiding the rest.
        eprintln!("note: showing the first {limit} pull requests; pass --limit to raise it");
    }
    if prs.is_empty() {
        // `--state all` would otherwise read "no all pull requests found".
        let scope = if state == "all" {
            String::new()
        } else {
            format!("{state} ")
        };
        bail!("no {scope}pull requests found for this repository");
    }
    Ok(prs)
}

/// Width of the widest PR number, so the `#123` column aligns.
fn number_width(prs: &[PullRequest]) -> usize {
    prs.iter()
        .map(|pr| pr.number.to_string().len())
        .max()
        .unwrap_or(0)
}

/// Which status glyph columns this particular set of pull requests earns. A
/// column is a fixed [`gh::GLYPH_WIDTH`] and appears only when some pull
/// request in the set has something to put in it.
#[derive(Clone, Copy, Default)]
struct StatusColumns {
    checks: bool,
    merge: bool,
}

impl StatusColumns {
    fn of(prs: &[PullRequest]) -> Self {
        Self {
            checks: prs.iter().any(|pr| !pr.checks().is_empty()),
            merge: prs
                .iter()
                .any(|pr| !pr.mergeable().glyph().trim().is_empty()),
        }
    }

    /// One pull request's glyphs, followed by the usual two-space gap, ready to
    /// splice in front of the title. Empty when the set earned no columns.
    ///
    /// `back` is the colour of the row these cells sit in, so a painted glyph
    /// hands it back. `None` leaves the glyphs bare, which is what the selector
    /// needs: skim renders through ratatui and does not interpret ANSI.
    fn cells(self, pr: &PullRequest, back: Option<u8>) -> String {
        let paint = |glyph: &str, color: u8| match back {
            Some(back) => term::paint_within(glyph, color, back, true),
            None => glyph.to_string(),
        };

        let mut cells = Vec::new();
        if self.checks {
            let checks = pr.checks();
            cells.push(paint(checks.glyph(), checks.color()));
        }
        if self.merge {
            let mergeable = pr.mergeable();
            cells.push(paint(mergeable.glyph(), mergeable.color()));
        }
        if cells.is_empty() {
            return String::new();
        }
        format!("{}  ", cells.join(" "))
    }
}

/// `cid pr ls` — print one pull request per line. The check and conflict
/// glyphs are in the plain listing; `--status` adds the state tag, the source
/// branch and the last-updated date.
///
/// The glyphs are shapes rather than colours, so a piped listing still says
/// everything a coloured one does.
pub fn ls(ctx: &Ctx, state: &str, limit: usize, status: bool) -> Result<()> {
    ensure_target(ctx)?;
    let prs = collect(ctx, state, limit)?;
    let color = ctx.color();
    let width = number_width(&prs);
    let columns = StatusColumns::of(&prs);
    let mut out = term::Listing::stdout();

    for pr in &prs {
        let cells = columns.cells(pr, color.then(|| pr.color()));
        let row = if status {
            format!(
                "#{number:<width$}  {tag:<6}  {cells}{title}  @{author}  [{head}]  {updated}",
                number = pr.number,
                tag = pr.tag(),
                title = pr.title,
                author = pr.author_login(),
                head = pr.head_ref_name,
                updated = pr.updated_date(),
            )
        } else {
            format!(
                "#{number:<width$}  {cells}{title}  @{author}",
                number = pr.number,
                title = pr.title,
                author = pr.author_login(),
            )
        };
        if !out.line(&term::paint(&row, pr.color(), color))? {
            break;
        }
    }
    out.finish()?;
    Ok(())
}

/// How many failing or running checks the preview names before it stops.
/// A matrix build can report a hundred; the pane is not a check log.
const PREVIEW_CHECKS: usize = 10;

/// The checks-and-mergeability block, or nothing when there is neither. Green
/// checks are counted, not listed; what is failing or running gets named.
fn check_lines(pr: &PullRequest) -> String {
    let checks = pr.checks();
    let mergeable = pr.mergeable();
    let mut parts = Vec::new();
    if !checks.is_empty() {
        parts.push(format!(
            "checks {}",
            term::paint(&checks.summary(), checks.color(), true)
        ));
    }
    if !mergeable.tag().is_empty() {
        parts.push(format!(
            "mergeable {}",
            term::paint(mergeable.tag(), mergeable.color(), true)
        ));
    }
    if parts.is_empty() {
        return String::new();
    }

    let mut out = parts.join("   ");
    out.push('\n');
    let failing = pr.failing_checks();
    for check in failing.iter().take(PREVIEW_CHECKS) {
        let result = check.result();
        out.push_str(&format!(
            "  {glyph}  {name}\n",
            glyph = term::paint(result.glyph(), result.color(), true),
            name = check.label(),
        ));
    }
    if failing.len() > PREVIEW_CHECKS {
        out.push_str(&format!(
            "  … and {} more\n",
            failing.len() - PREVIEW_CHECKS
        ));
    }
    out
}

/// The preview for a pull request: its heading, check rollup, and description,
/// rendered from data already in memory. Deliberately *not* `gh pr view`, which
/// would be a ~2s network round trip per highlighted row.
fn preview(pr: &PullRequest) -> Preview {
    let mut out = term::paint(
        &format!("#{number}  {title}", number = pr.number, title = pr.title),
        pr.color(),
        true,
    );
    out.push('\n');
    out.push_str(&format!(
        "{tag}  @{author}  [{head}]  updated {updated}\n",
        tag = pr.tag(),
        author = pr.author_login(),
        head = pr.head_ref_name,
        updated = pr.updated_date(),
    ));
    out.push_str(&check_lines(pr));
    out.push('\n');

    let body = pr.body.trim();
    if body.is_empty() {
        out.push_str("(no description)");
    } else {
        // GitHub stores CRLF; the pane renders the stray CRs as artefacts.
        out.push_str(&body.replace("\r\n", "\n"));
    }
    Preview::Text(out)
}

/// What tints a selector row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tint {
    /// By pull request state — for choosing *which* pull request.
    State,
    /// By readiness to merge — for whether it *can* be merged, where a state
    /// colouring would paint a list of open pull requests one uniform green.
    Readiness,
}

impl Tint {
    fn color(self, pr: &PullRequest) -> u8 {
        match self {
            Self::State => pr.color(),
            Self::Readiness => pr.readiness().color(),
        }
    }
}

/// Build selector rows, with the glyphs and source branch part of the label.
/// The glyphs go in bare: skim tints a row from [`SelectItem::color`] and does
/// no ANSI parsing along the way.
fn items(prs: &[PullRequest], tint: Tint) -> Vec<SelectItem> {
    let width = number_width(prs);
    let columns = StatusColumns::of(prs);
    prs.iter()
        .map(|pr| {
            let label = format!(
                "#{number:<width$}  {cells}{title}  @{author}  [{head}]",
                number = pr.number,
                cells = columns.cells(pr, None),
                title = pr.title,
                author = pr.author_login(),
                head = pr.head_ref_name,
            );
            SelectItem::new(label, pr.number.to_string())
                .color(tint.color(pr))
                .preview(preview(pr))
        })
        .collect()
}

/// Open the highlighted pull request in the browser, from any of these
/// selectors. f2 is what opens one from the prompt in fish, and means the same
/// thing here.
const OPEN: select::Action = select::Action::new("f2", "open");

/// Check the highlighted pull request out, from any of these selectors — f7 at
/// the prompt, and f7 here.
const CHECKOUT: select::Action = select::Action::new("f7", "check out");

/// Check the highlighted pull request out in a worktree of its own — ctrl-f7 at
/// the prompt, and ctrl-f7 here. Only the prompt's binding can `cd` into it;
/// from here the path is printed.
const WORKTREE: select::Action = select::Action::new("ctrl-f7", "worktree");

/// A pull request settled on, and the key that settled it when it was not
/// enter.
struct Chosen {
    number: u64,
    /// The branch it was opened from, where the list it was chosen from said so.
    /// A number given outright comes without one.
    head: Option<String>,
    action: Option<&'static str>,
}

/// Fuzzy-select one pull request and return its number, with
/// [`REFRESH_KEY`](select::REFRESH_KEY) asking `gh` again in place. A failed
/// reload leaves the rows as they were and says so once the selector closes.
///
/// `actions` are the verbs this selector offers beside the one it was opened
/// for; the key that was pressed comes back with the number.
fn select(
    ctx: &Ctx,
    state: &str,
    limit: usize,
    prompt: &str,
    tint: Tint,
    actions: &'static [select::Action],
) -> Result<Chosen> {
    let prs = collect(ctx, state, limit)?;
    // Built before the shared list exists: a `known.lock()` written inline in
    // the `select_one_reloading` call below would hold its guard for the whole
    // statement, and the reload would block on it forever.
    let rows = items(&prs, tint);
    let known = Arc::new(Mutex::new(prs));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let reload = {
        let (known, failure, state) = (Arc::clone(&known), Arc::clone(&failure), state.to_string());
        Box::new(move |_view| {
            // Asked for outside the lock: `gh` is a network round trip, and
            // holding the list for it would make a second ctrl-r wait on the
            // first rather than on GitHub.
            let fresh = gh::list(&state, limit);
            let mut known = known.lock().expect("pull request list poisoned");
            match fresh {
                Ok(fresh) if !fresh.is_empty() => *known = fresh,
                // An empty answer is not worth blanking the list for: the
                // pull request you were looking at was there a moment ago.
                Ok(_) => {}
                Err(err) => {
                    *failure.lock().expect("failure slot poisoned") = Some(format!("{err:#}"));
                }
            }
            items(&known, tint)
        })
    };

    let chosen = select::select_one_reloading(rows, prompt, &ctx.config.selector, reload, actions)?;

    if let Some(err) = failure.lock().expect("failure slot poisoned").take() {
        eprintln!("warning: could not refresh pull requests: {err}");
    }
    let number = chosen
        .value
        .parse()
        .map_err(|_| anyhow::anyhow!("unexpected selector result: {}", chosen.value))?;
    let head = known
        .lock()
        .expect("pull request list poisoned")
        .iter()
        .find(|pr| pr.number == number)
        .map(|pr| pr.head_ref_name.clone());
    Ok(Chosen {
        number,
        head,
        action: chosen.action,
    })
}

/// Do what the key the selector closed on asked for, or `Ok(false)` when it was
/// enter and the command it was opened for is what should happen.
fn acted_on(ctx: &Ctx, chosen: &Chosen) -> Result<bool> {
    match chosen.action {
        Some(key) if key == OPEN.key => gh::view_web(chosen.number).map(|()| true),
        Some(key) if key == CHECKOUT.key => gh::checkout(chosen.number).map(|()| true),
        Some(key) if key == WORKTREE.key => checkout_worktree(ctx, chosen).map(|()| true),
        _ => Ok(false),
    }
}

/// `cid pr sel` — fuzzy-select a pull request and print its number, so it
/// composes with `gh`: `gh pr view (cid pr sel)`.
pub fn sel(ctx: &Ctx, state: &str, limit: usize) -> Result<()> {
    ensure_target(ctx)?;
    let chosen = select(
        ctx,
        state,
        limit,
        "Select a pull request",
        Tint::State,
        &[OPEN, CHECKOUT, WORKTREE],
    )?;
    if acted_on(ctx, &chosen)? {
        // The number is not printed: nothing is waiting for it, and a caller
        // substituting this command asked for a number rather than for the
        // browser to open.
        return Ok(());
    }
    println!("{}", chosen.number);
    Ok(())
}

/// Resolve the pull request to act on: the number given, or one the user
/// selects — in which case they may have asked for something else on the way,
/// which the caller learns from the returned key.
fn resolve(
    ctx: &Ctx,
    number: Option<u64>,
    state: &str,
    limit: usize,
    prompt: &str,
    tint: Tint,
    actions: &'static [select::Action],
) -> Result<Chosen> {
    match number {
        Some(number) => Ok(Chosen {
            number,
            head: None,
            action: None,
        }),
        None => select(ctx, state, limit, prompt, tint, actions),
    }
}

/// `cid pr checkout [number]` — check out a pull request's branch, selecting
/// one when no number is given. The checkout itself is `gh pr checkout`, which
/// handles fork PRs and sets the upstream.
///
/// With `worktree`, the branch goes in a tree of its own instead, and the
/// tree's path is what is printed — see [`checkout_worktree`].
pub fn checkout(
    ctx: &Ctx,
    number: Option<u64>,
    worktree: bool,
    state: &str,
    limit: usize,
) -> Result<()> {
    ensure_target(ctx)?;
    // Beside it, the verbs enter does not already perform.
    let (prompt, actions): (&str, &'static [select::Action]) = if worktree {
        ("Check out a pull request in a worktree", &[OPEN, CHECKOUT])
    } else {
        ("Check out a pull request", &[OPEN, WORKTREE])
    };
    let chosen = resolve(ctx, number, state, limit, prompt, Tint::State, actions)?;
    if acted_on(ctx, &chosen)? {
        return Ok(());
    }
    if worktree {
        checkout_worktree(ctx, &chosen)
    } else {
        gh::checkout(chosen.number)
    }
}

/// Check a pull request out in a working tree of its own and print the tree's
/// path, for a shell to `cd` into.
///
/// The tree goes where `worktree add` would put one for the pull request's
/// branch. It is created detached and `gh pr checkout` runs inside it, so a
/// pull request from a fork gets the same remote and upstream it would in the
/// main tree. A tree that already has the branch checked out is where the user
/// is sent instead, since git will not check one branch out twice.
fn checkout_worktree(ctx: &Ctx, chosen: &Chosen) -> Result<()> {
    let repo_root = git::require_repo_root()?;
    let branch = match &chosen.head {
        Some(head) => head.clone(),
        None => {
            let _spinner = term::spinner("looking up the pull request's branch", ctx.color());
            gh::head_branch(chosen.number)?
        }
    };

    let trees = git::worktrees(&repo_root)?;
    if let Some(tree) = tree_with_branch(&trees, &branch) {
        eprintln!("note: `{branch}` is already checked out in a worktree");
        println!("{}", tree.path.display());
        return Ok(());
    }

    let path = worktree::new_tree_path(ctx, &repo_root, &branch)?;
    git::add_detached_worktree(&path)?;
    if let Err(err) = gh::checkout_in(&path, chosen.number) {
        // A tree with no branch in it is not the half of this worth keeping.
        if let Err(cleanup) = git::remove_worktree(&path, true) {
            ctx.log
                .warn(&format!("could not remove {}: {cleanup:#}", path.display()));
        }
        return Err(err);
    }
    worktree::exclude_root(ctx, &repo_root);

    println!("{}", path.display());
    Ok(())
}

/// The tree that has `branch` checked out, if one does.
fn tree_with_branch<'a>(trees: &'a [git::Worktree], branch: &str) -> Option<&'a git::Worktree> {
    trees
        .iter()
        .find(|tree| !tree.prunable && tree.branch == branch)
}

/// `cid pr open [number]` — open a pull request in the browser, selecting one
/// when no number is given.
pub fn open(
    ctx: &Ctx,
    number: Option<u64>,
    current: bool,
    state: &str,
    limit: usize,
) -> Result<()> {
    ensure_target(ctx)?;
    if current {
        return open_current(ctx);
    }
    let chosen = resolve(
        ctx,
        number,
        state,
        limit,
        "Open a pull request",
        Tint::State,
        &[CHECKOUT, WORKTREE],
    )?;
    if acted_on(ctx, &chosen)? {
        return Ok(());
    }
    gh::view_web(chosen.number)
}

/// `cid pr open --current` — open the pull request for the checked-out
/// branch, falling back to the repository's pull request list when it has none.
///
/// The fallback is the point of the flag: a branch either has a pull request or
/// is a branch you are about to open one from, and both answers are a page in
/// the browser rather than a question to answer. A detached HEAD has no branch
/// at all, and takes the same fallback.
fn open_current(ctx: &Ctx) -> Result<()> {
    git::ensure_repo()?;

    let branch = git::current_branch();
    let number = match &branch {
        Some(branch) => {
            let _spinner = term::spinner("looking for this branch's pull request", ctx.color());
            gh::pr_for_branch(branch)?
        }
        None => None,
    };

    match number {
        Some(number) => {
            ctx.log
                .info(&format!("pull request #{number} for this branch"));
            gh::view_web(number)
        }
        None => {
            // Said out loud: the browser opens either way, and without this the
            // list page looks like the pull request the binding was asked for.
            match &branch {
                Some(branch) => {
                    eprintln!("note: no pull request from `{branch}`; opening the list")
                }
                None => eprintln!("note: HEAD is detached; opening the pull request list"),
            }
            gh::list_web()
        }
    }
}

/// `cid pr merge [number]` — merge a pull request, selecting one when no
/// number is given.
///
/// The one selector tinted by [`Tint::Readiness`] rather than by state, since a
/// list of open pull requests is one shade of green under a state colouring.
/// `gh pr merge` prompts for the method when none of
/// `--merge`/`--squash`/`--rebase` is given.
pub fn merge(
    ctx: &Ctx,
    number: Option<u64>,
    state: &str,
    limit: usize,
    method: Option<MergeMethod>,
    delete_branch: bool,
    auto: bool,
) -> Result<()> {
    ensure_target(ctx)?;
    // `open` above all: reading a pull request before merging it is the thing
    // most likely to be wanted between choosing one and merging it.
    let chosen = resolve(
        ctx,
        number,
        state,
        limit,
        "Merge a pull request",
        Tint::Readiness,
        &[OPEN],
    )?;
    if acted_on(ctx, &chosen)? {
        return Ok(());
    }
    gh::merge(chosen.number, method, delete_branch, auto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prs() -> Vec<PullRequest> {
        gh::parse_prs(
            r#"[
            {"number":7,"title":"Add branch selector","author":{"login":"joakimen"},
             "headRefName":"feat/branches","isDraft":false,"state":"OPEN",
             "updatedAt":"2026-07-27T09:12:33Z","mergeable":"MERGEABLE",
             "statusCheckRollup":[
                {"name":"build","workflowName":"ci","status":"COMPLETED","conclusion":"SUCCESS"}
             ],
             "body":"Adds a selector.\r\n\r\nSecond paragraph."},
            {"number":123,"title":"WIP","author":{"login":"someone"},
             "headRefName":"wip","isDraft":true,"state":"OPEN",
             "updatedAt":"2026-07-20T11:00:00Z","mergeable":"CONFLICTING",
             "statusCheckRollup":[
                {"name":"build","workflowName":"ci","status":"COMPLETED","conclusion":"FAILURE"},
                {"name":"test","workflowName":"ci","status":"IN_PROGRESS","conclusion":""}
             ],
             "body":""}
        ]"#,
        )
        .unwrap()
    }

    fn bare_prs() -> Vec<PullRequest> {
        gh::parse_prs(
            r#"[
            {"number":1,"title":"Only one","author":{"login":"ada"},
             "headRefName":"only","isDraft":false,"state":"OPEN",
             "updatedAt":"2026-07-27T09:12:33Z","body":""}
        ]"#,
        )
        .unwrap()
    }

    fn tree(branch: &str, prunable: bool) -> git::Worktree {
        git::Worktree {
            path: format!("/trees/{branch}").into(),
            branch: branch.to_string(),
            prunable,
            ..git::Worktree::default()
        }
    }

    #[test]
    fn a_branch_already_in_a_tree_is_found_there() {
        let trees = [tree("main", false), tree("feat/x", false)];
        let found = tree_with_branch(&trees, "feat/x").map(|tree| tree.path.clone());
        assert_eq!(found, Some("/trees/feat/x".into()));
        assert!(tree_with_branch(&trees, "feat").is_none());
    }

    /// A tree whose directory is gone is nowhere to send a shell.
    #[test]
    fn a_prunable_tree_does_not_count_as_holding_its_branch() {
        let trees = [tree("feat/x", true)];
        assert!(tree_with_branch(&trees, "feat/x").is_none());
    }

    #[test]
    fn the_worktree_key_is_not_one_another_action_uses() {
        let keys = [OPEN.key, CHECKOUT.key, WORKTREE.key];
        for (index, key) in keys.iter().enumerate() {
            assert!(!keys[index + 1..].contains(key), "{key} is bound twice");
            assert!(
                skim::binds::parse_key(key).is_ok(),
                "skim cannot read {key}"
            );
        }
    }

    #[test]
    fn rows_return_the_pr_number() {
        let items = items(&prs(), Tint::State);
        assert_eq!(items[0].value(), "7");
        assert_eq!(items[1].value(), "123");
    }

    #[test]
    fn rows_show_title_author_and_branch() {
        let label = &items(&prs(), Tint::State)[0].label;
        assert!(label.contains("#7"));
        assert!(label.contains("Add branch selector"));
        assert!(label.contains("@joakimen"));
        assert!(label.contains("[feat/branches]"), "{label}");
    }

    #[test]
    fn drafts_are_tinted_differently() {
        let items = items(&prs(), Tint::State);
        assert_ne!(items[0].color, items[1].color);
    }

    #[test]
    fn preview_is_rendered_in_memory() {
        let Preview::Text(text) = preview(&prs()[0]) else {
            panic!("a PR preview must not spawn a command");
        };
        assert!(text.contains("#7"), "{text}");
        assert!(text.contains("Add branch selector"), "{text}");
        assert!(text.contains("@joakimen"), "{text}");
        assert!(text.contains("[feat/branches]"), "{text}");
        assert!(text.contains("Adds a selector"), "the body is the preview");
    }

    #[test]
    fn preview_handles_a_missing_description() {
        let Preview::Text(text) = preview(&prs()[1]) else {
            panic!("expected text");
        };
        assert!(text.contains("(no description)"), "{text}");
    }

    #[test]
    fn preview_normalises_line_endings() {
        let Preview::Text(text) = preview(&prs()[0]) else {
            panic!("expected text");
        };
        assert!(!text.contains('\r'), "CRLF leaked into the preview");
    }

    /// The column a title starts in, as the terminal counts it — not
    /// `str::find`, whose byte offsets differ between rows that line up.
    fn title_column(label: &str, title: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        let start = label.find(title).expect("title in label");
        label[..start].width()
    }

    #[test]
    fn number_column_aligns() {
        let items = items(&prs(), Tint::State);
        assert_eq!(
            title_column(&items[0].label, "Add branch selector"),
            title_column(&items[1].label, "WIP"),
        );
    }

    #[test]
    fn rows_carry_the_check_rollup() {
        let items = items(&prs(), Tint::State);
        assert!(items[0].label.contains('✓'), "{}", items[0].label);
        assert!(items[1].label.contains('✗'), "{}", items[1].label);
        assert!(items[1].label.contains('⊘'), "{}", items[1].label);
    }

    #[test]
    fn status_columns_vanish_when_there_is_nothing_to_show() {
        let columns = StatusColumns::of(&bare_prs());
        assert_eq!(columns.cells(&bare_prs()[0], None), "");
        let label = &items(&bare_prs(), Tint::State)[0].label;
        assert_eq!(label, "#1  Only one  @ada  [only]");
    }

    #[test]
    fn status_columns_align() {
        use unicode_width::UnicodeWidthStr;
        let items = items(&prs(), Tint::State);
        assert_eq!(
            title_column(&items[0].label, "Add branch selector"),
            title_column(&items[1].label, "WIP"),
        );
        let columns = StatusColumns::of(&prs());
        let widths: Vec<usize> = prs()
            .iter()
            .map(|pr| columns.cells(pr, None).width())
            .collect();
        // Two glyph columns, the space between them, and the gap before the
        // title.
        assert_eq!(widths, [gh::GLYPH_WIDTH * 2 + 3; 2]);
    }

    #[test]
    fn the_conflict_column_appears_only_when_one_exists() {
        use unicode_width::UnicodeWidthStr;
        let clean = &prs()[..1];
        let columns = StatusColumns::of(clean);
        assert_eq!(columns.cells(&clean[0], None).width(), gh::GLYPH_WIDTH + 2);
    }

    #[test]
    fn the_bare_listing_carries_the_glyphs() {
        // `ls` builds its bare row from the same cells the selector uses.
        let columns = StatusColumns::of(&prs());
        assert!(columns.cells(&prs()[0], None).contains('✓'));
        assert!(columns.cells(&prs()[1], None).contains('✗'));
    }

    #[test]
    fn the_merge_selector_tints_by_readiness() {
        let prs = prs();
        let state = items(&prs, Tint::State);
        let readiness = items(&prs, Tint::Readiness);
        // #7 is open with green checks; #123 is a draft with a failing check.
        assert_eq!(state[0].color, Some(prs[0].color()));
        assert_eq!(readiness[0].color, Some(gh::Readiness::Ready.color()));
        assert_eq!(
            readiness[1].color,
            Some(gh::Readiness::Unavailable.color()),
            "a draft is not a merge candidate",
        );
    }

    #[test]
    fn preview_names_the_failing_checks() {
        let Preview::Text(text) = preview(&prs()[1]) else {
            panic!("expected text");
        };
        assert!(text.contains("1 failed, 1 pending"), "{text}");
        assert!(text.contains("build (ci)"), "{text}");
        assert!(text.contains("test (ci)"), "{text}");
        assert!(text.contains("conflict"), "{text}");
    }

    #[test]
    fn preview_marks_each_named_check_with_its_glyph() {
        let text = strip_ansi(&check_lines(&prs()[1]));
        let rows: Vec<&str> = text.lines().filter(|line| line.starts_with("  ")).collect();
        assert_eq!(rows, ["  ✗  build (ci)", "  ⧗  test (ci)"]);
    }

    /// Drop ANSI colour sequences, so a test can measure what the pane shows.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn preview_counts_passing_checks_without_listing_them() {
        let Preview::Text(text) = preview(&prs()[0]) else {
            panic!("expected text");
        };
        assert!(text.contains("1 passed"), "{text}");
        assert!(!text.contains("build (ci)"), "a passing check was listed");
    }

    #[test]
    fn preview_omits_the_status_line_when_empty() {
        assert_eq!(check_lines(&bare_prs()[0]), "");
        let Preview::Text(text) = preview(&bare_prs()[0]) else {
            panic!("expected text");
        };
        assert!(!text.contains("checks"), "{text}");
        assert!(!text.contains("mergeable"), "{text}");
    }
}
