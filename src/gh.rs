//! GitHub pull requests, via the `gh` CLI.
//!
//! cid does no GitHub authentication of its own: it shells out to `gh`, which
//! already holds the user's token and knows which repository the working
//! directory belongs to.
//!
//! As in [`crate::git`], the decisions are pure functions with tests; only
//! [`list`] and the process helpers touch the outside world.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::Reported;
use crate::stats;
use crate::term;

/// JSON fields requested from `gh pr list`. `body`, `statusCheckRollup` and
/// `mergeable` are fetched here so the preview pane renders from memory rather
/// than a `gh pr view` round trip per highlighted row.
const FIELDS: &str =
    "number,title,author,headRefName,isDraft,state,updatedAt,body,statusCheckRollup,mergeable";

/// A pull request, as much of it as the selector needs.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    /// Absent when the author's account is gone.
    #[serde(default)]
    pub author: Option<Author>,
    /// The PR's source branch.
    pub head_ref_name: String,
    #[serde(default)]
    pub is_draft: bool,
    /// `OPEN`, `CLOSED`, or `MERGED`.
    #[serde(default)]
    pub state: String,
    /// ISO-8601 timestamp, e.g. `2026-07-27T09:12:33Z`.
    #[serde(default)]
    pub updated_at: String,
    /// The description, as markdown. Empty when none was written.
    #[serde(default)]
    pub body: String,
    /// Every check reported against the head commit. Empty when the repository
    /// runs no CI at all.
    #[serde(default)]
    pub status_check_rollup: Vec<Check>,
    /// `MERGEABLE`, `CONFLICTING`, or `UNKNOWN`. See [`Mergeable`].
    #[serde(default)]
    pub mergeable: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

/// One entry in a pull request's status check rollup. GitHub returns two shapes
/// in the same array: `CheckRun`, which reports a `status` and a `conclusion`,
/// and the older `StatusContext`, which reports a single `state`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// A `CheckRun`'s name.
    #[serde(default)]
    pub name: String,
    /// A `StatusContext`'s name.
    #[serde(default)]
    pub context: String,
    /// The workflow a `CheckRun` belongs to, which is what tells two jobs named
    /// `build` apart.
    #[serde(default)]
    pub workflow_name: String,
    /// `CheckRun`: `QUEUED`, `IN_PROGRESS`, or `COMPLETED`.
    #[serde(default)]
    pub status: String,
    /// `CheckRun`: `SUCCESS`, `FAILURE`, `SKIPPED`, …; empty until completed.
    #[serde(default)]
    pub conclusion: String,
    /// `StatusContext`: `SUCCESS`, `PENDING`, `FAILURE`, `ERROR`, `EXPECTED`.
    #[serde(default)]
    pub state: String,
}

/// What one check says right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Pass,
    Fail,
    Pending,
}

impl CheckResult {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Pending => "pending",
        }
    }

    /// The glyph for a list row. See [`GLYPH_WIDTH`]; the colour is
    /// [`Self::color`].
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Pass => "✓",
            Self::Fail => "✗",
            Self::Pending => "⧗",
        }
    }

    /// ANSI 256-colour index matching [`Self::tag`].
    pub fn color(self) -> u8 {
        match self {
            Self::Pass => 2,    // green
            Self::Fail => 1,    // red
            Self::Pending => 3, // yellow
        }
    }
}

/// Display width of every status glyph, and of the blank that stands in for
/// one. The glyphs are all East Asian *Narrow* and absent from the emoji data,
/// so they stay one column and take the colour they are painted. Ambiguous-width
/// and emoji-presentation glyphs would not.
pub const GLYPH_WIDTH: usize = 1;

/// The blank that keeps a column aligned when there is nothing to say.
pub const NO_GLYPH: &str = " ";

impl Check {
    /// The name to show: a `CheckRun`'s job name qualified by its workflow, or
    /// a `StatusContext`'s context.
    pub fn label(&self) -> String {
        let name = if self.name.is_empty() {
            &self.context
        } else {
            &self.name
        };
        if self.workflow_name.is_empty() {
            name.clone()
        } else {
            format!("{name} ({})", self.workflow_name)
        }
    }

    pub fn result(&self) -> CheckResult {
        result_for(&self.status, &self.conclusion, &self.state)
    }
}

/// Reduce one check's fields to a verdict, whichever shape it arrived in.
/// `SKIPPED` and `NEUTRAL` count as passing, as GitHub itself treats them.
fn result_for(status: &str, conclusion: &str, state: &str) -> CheckResult {
    // A `StatusContext` says everything in one field.
    if !state.is_empty() {
        return if is(state, &["SUCCESS"]) {
            CheckResult::Pass
        } else if is(state, &["PENDING", "EXPECTED"]) {
            CheckResult::Pending
        } else {
            CheckResult::Fail // FAILURE, ERROR
        };
    }
    // A `CheckRun` that has not finished is pending whatever else it says.
    if !status.is_empty() && !status.eq_ignore_ascii_case("COMPLETED") {
        return CheckResult::Pending;
    }
    // Finished with nothing to say: not yet reported, so not yet a verdict.
    if conclusion.is_empty() {
        return CheckResult::Pending;
    }
    if is(conclusion, &["SUCCESS", "SKIPPED", "NEUTRAL"]) {
        CheckResult::Pass
    } else {
        // FAILURE, TIMED_OUT, CANCELLED, ACTION_REQUIRED, STARTUP_FAILURE.
        CheckResult::Fail
    }
}

/// Whether `value` is one of `options`, ignoring case — `gh`'s enums are
/// conventionally upper case but not guaranteed to be.
fn is(value: &str, options: &[&str]) -> bool {
    options
        .iter()
        .any(|option| value.eq_ignore_ascii_case(option))
}

/// How a pull request's checks add up: the counts, and the one-word verdict
/// derived from them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Checks {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
}

impl Checks {
    /// Tally `checks` into passed/failed/pending.
    pub fn of(checks: &[Check]) -> Self {
        let mut out = Self::default();
        for check in checks {
            match check.result() {
                CheckResult::Pass => out.passed += 1,
                CheckResult::Fail => out.failed += 1,
                CheckResult::Pending => out.pending += 1,
            }
        }
        out
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.pending
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// The verdict, worst-first. Empty when there are no checks, so a
    /// repository without CI does not look like one whose checks have not
    /// started.
    pub fn tag(&self) -> &'static str {
        if self.is_empty() {
            ""
        } else if self.failed > 0 {
            "fail"
        } else if self.pending > 0 {
            "pending"
        } else {
            "pass"
        }
    }

    /// The verdict as a single [`CheckResult`], or `None` when there are no
    /// checks to roll up.
    pub fn result(&self) -> Option<CheckResult> {
        if self.is_empty() {
            None
        } else if self.failed > 0 {
            Some(CheckResult::Fail)
        } else if self.pending > 0 {
            Some(CheckResult::Pending)
        } else {
            Some(CheckResult::Pass)
        }
    }

    /// The glyph for a list row: one column of [`GLYPH_WIDTH`], blank when the
    /// repository reports no checks at all.
    pub fn glyph(&self) -> &'static str {
        self.result().map_or(NO_GLYPH, CheckResult::glyph)
    }

    /// ANSI 256-colour index matching [`Self::tag`], grey when there is nothing
    /// to report.
    pub fn color(&self) -> u8 {
        self.result().map_or(8, CheckResult::color)
    }

    /// A human count, e.g. `2 passed, 1 failed`. Empty when there are no checks.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.pending > 0 {
            parts.push(format!("{} pending", self.pending));
        }
        if self.passed > 0 {
            parts.push(format!("{} passed", self.passed));
        }
        parts.join(", ")
    }
}

/// Whether GitHub thinks a pull request can be merged into its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Clean,
    Conflicting,
    /// GitHub computes mergeability lazily and reports `UNKNOWN` until that
    /// job has run. Rendered as nothing rather than guessed at.
    Unknown,
}

impl Mergeable {
    pub fn parse(raw: &str) -> Self {
        if is(raw, &["MERGEABLE"]) {
            Self::Clean
        } else if is(raw, &["CONFLICTING"]) {
            Self::Conflicting
        } else {
            Self::Unknown
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Conflicting => "conflict",
            Self::Unknown => "",
        }
    }

    /// The glyph for a list row — one column of [`GLYPH_WIDTH`], and only for a
    /// conflict, which is the answer that changes what happens next. The
    /// preview names both.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Conflicting => "⊘",
            _ => NO_GLYPH,
        }
    }

    /// ANSI 256-colour index matching [`Self::tag`].
    pub fn color(self) -> u8 {
        match self {
            Self::Clean => 2,       // green
            Self::Conflicting => 1, // red
            Self::Unknown => 8,     // bright black
        }
    }
}

/// Whether a pull request looks mergeable right now, from what a single
/// `gh pr list` already knows. Not the same as [`PullRequest::state`], which
/// colours a list of open pull requests all one shade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Nothing in the way: open, not a draft, no failing check, no conflict.
    Ready,
    /// Checks are still running.
    Waiting,
    /// A failing check or a conflict with the base branch.
    Blocked,
    /// Not a candidate at all — a draft, or already merged or closed.
    Unavailable,
}

impl Readiness {
    /// ANSI 256-colour index: the traffic light this describes.
    pub fn color(self) -> u8 {
        match self {
            Self::Ready => 2,       // green
            Self::Waiting => 3,     // yellow
            Self::Blocked => 1,     // red
            Self::Unavailable => 8, // bright black
        }
    }
}

impl PullRequest {
    pub fn author_login(&self) -> &str {
        self.author.as_ref().map_or("unknown", |a| a.login.as_str())
    }

    pub fn checks(&self) -> Checks {
        Checks::of(&self.status_check_rollup)
    }

    pub fn mergeable(&self) -> Mergeable {
        Mergeable::parse(&self.mergeable)
    }

    /// Whether this looks mergeable — see [`Readiness`]. A draft counts as
    /// unavailable rather than blocked.
    pub fn readiness(&self) -> Readiness {
        if self.is_draft || !self.state.eq_ignore_ascii_case("OPEN") {
            return Readiness::Unavailable;
        }
        let checks = self.checks();
        if checks.failed > 0 || self.mergeable() == Mergeable::Conflicting {
            Readiness::Blocked
        } else if checks.pending > 0 {
            Readiness::Waiting
        } else {
            Readiness::Ready
        }
    }

    /// The checks that are not passing, failures first, each half keeping the
    /// rollup's own order.
    pub fn failing_checks(&self) -> Vec<&Check> {
        let mut failed = Vec::new();
        let mut pending = Vec::new();
        for check in &self.status_check_rollup {
            match check.result() {
                CheckResult::Pass => {}
                CheckResult::Fail => failed.push(check),
                CheckResult::Pending => pending.push(check),
            }
        }
        failed.append(&mut pending);
        failed
    }

    /// Just the date part of [`Self::updated_at`].
    pub fn updated_date(&self) -> &str {
        self.updated_at.split('T').next().unwrap_or_default()
    }

    /// ANSI 256-colour index for this PR, used in both the selector and `ls`.
    pub fn color(&self) -> u8 {
        color_for(self.is_draft, &self.state)
    }

    pub fn tag(&self) -> &'static str {
        tag_for(self.is_draft, &self.state)
    }
}

/// Colour a PR by what can be done with it: a draft is not ready to review, a
/// merged or closed one is history.
fn color_for(is_draft: bool, state: &str) -> u8 {
    if is(state, &["MERGED"]) {
        5 // magenta
    } else if is(state, &["CLOSED"]) {
        1 // red
    } else if is_draft {
        8 // bright black — open but draft
    } else {
        2 // green — open and ready
    }
}

fn tag_for(is_draft: bool, state: &str) -> &'static str {
    if is(state, &["MERGED"]) {
        "merged"
    } else if is(state, &["CLOSED"]) {
        "closed"
    } else if is_draft {
        "draft"
    } else {
        "open"
    }
}

/// Parse the array `gh pr list --json …` prints.
pub fn parse_prs(data: &str) -> Result<Vec<PullRequest>> {
    // `gh` prints nothing at all in some error paths; treat that as empty.
    if data.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut prs: Vec<PullRequest> =
        serde_json::from_str(data).context("parsing `gh pr list` output")?;
    // Foreign text, made safe once here so no row downstream has to remember.
    for pr in &mut prs {
        pr.title = term::one_row(&pr.title);
        pr.head_ref_name = term::one_row(&pr.head_ref_name);
        pr.body = term::block(&pr.body);
        if let Some(author) = &mut pr.author {
            author.login = term::one_row(&author.login);
        }
        for check in &mut pr.status_check_rollup {
            check.name = term::one_row(&check.name);
            check.context = term::one_row(&check.context);
            check.workflow_name = term::one_row(&check.workflow_name);
        }
    }
    Ok(prs)
}

/// Pull requests for the repository the working directory belongs to.
///
/// `state` is passed through to `gh` (`open`, `closed`, `merged`, `all`).
pub fn list(state: &str, limit: usize) -> Result<Vec<PullRequest>> {
    let limit = limit.to_string();
    let out = capture(&[
        "pr", "list", "--json", FIELDS, "--state", state, "--limit", &limit,
    ])?;
    parse_prs(&out)
}

/// Check out a pull request's branch. `gh` creates the local branch and sets
/// its upstream, including for PRs from forks.
pub fn checkout(number: u64) -> Result<()> {
    run(&["pr", "checkout", &number.to_string()])
}

/// Check out a pull request's branch in the tree at `dir`, as [`checkout`]
/// does in the current one.
///
/// `gh` narrates the checkout on stdout, which is where the caller prints the
/// tree's path for a shell to `cd` into, so it is handed stderr in its place.
pub fn checkout_in(dir: &Path, number: u64) -> Result<()> {
    use std::os::fd::AsFd;

    let stderr = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating stderr")?;
    let _child = stats::in_child();
    let status = Command::new("gh")
        .args(["pr", "checkout", &number.to_string()])
        .current_dir(dir)
        .stdout(Stdio::from(stderr))
        .status()
        .map_err(spawn_error)?;
    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

/// The branch a pull request was opened from, as GitHub names it.
pub fn head_branch(number: u64) -> Result<String> {
    let out = capture(&[
        "pr",
        "view",
        &number.to_string(),
        "--json",
        "headRefName",
        "--jq",
        ".headRefName",
    ])?;
    let branch = term::one_row(out.trim());
    if branch.is_empty() {
        bail!("gh named no branch for pull request #{number}");
    }
    Ok(branch)
}

/// Open a pull request in the browser. `gh` already knows the host, so GitHub
/// Enterprise works, and defers to `$BROWSER`.
///
/// `gh browse <number>` and not `gh pr view --web <number>`, which reaches the
/// same page: the latter asks GraphQL for the pull request before handing the
/// browser a URL it had already worked out, and waits for the answer. `browse`
/// builds the URL from the git remotes alone. It spells the number as an issue,
/// which GitHub redirects to the pull request — the redirect every `#123`
/// reference on the site relies on.
pub fn view_web(number: u64) -> Result<()> {
    run(&["browse", &number.to_string()])
}

/// Open the repository's pull request list in the browser.
pub fn list_web() -> Result<()> {
    run(&["pr", "list", "--web"])
}

/// The most recent pull request opened from `branch`, in any state, or `None`
/// when GitHub has none.
///
/// State is deliberately unfiltered: a branch whose pull request has merged
/// still has one, and it is the page a reader asking about that branch wants.
///
/// The number is asked for rather than derived from `gh pr view`'s own notion
/// of the current branch, so one query decides both whether a pull request
/// exists and which it is — two answers that must not be able to disagree.
pub fn pr_for_branch(branch: &str) -> Result<Option<u64>> {
    let out = capture(&[
        "pr",
        "list",
        "--head",
        branch,
        "--state",
        "all",
        "--limit",
        "1",
        "--json",
        "number",
        "--jq",
        ".[0].number",
    ])?;
    let out = out.trim();
    if out.is_empty() {
        return Ok(None);
    }
    out.parse()
        .map(Some)
        .with_context(|| format!("gh returned `{out}` as the pull request number for {branch}"))
}

/// How to merge a pull request. `None` at the call site leaves the choice to
/// `gh`, which asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    fn flag(self) -> &'static str {
        match self {
            Self::Merge => "--merge",
            Self::Squash => "--squash",
            Self::Rebase => "--rebase",
        }
    }
}

/// Merge a pull request. With no `method`, `gh` prompts for one, which is why
/// this inherits stdio rather than capturing it.
pub fn merge(
    number: u64,
    method: Option<MergeMethod>,
    delete_branch: bool,
    auto: bool,
) -> Result<()> {
    let number = number.to_string();
    let mut args = vec!["pr", "merge", &number];
    if let Some(method) = method {
        args.push(method.flag());
    }
    if delete_branch {
        args.push("--delete-branch");
    }
    if auto {
        args.push("--auto");
    }
    run(&args)
}

/// A repository on GitHub, as much of it as the clone selector needs.
///
/// Named as the REST API names things, since that is what fills it in.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct Repo {
    /// `owner/name`.
    pub full_name: String,
    /// Null for a repository with no description, which is why it is an option
    /// rather than a defaulted string.
    #[serde(default)]
    pub description: Option<String>,
    /// GitHub's own word: `public`, `private` or `internal`. Read through
    /// [`Repo::visibility`].
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
    /// The language GitHub counts most of, null for a repository with none.
    #[serde(default)]
    pub language: Option<String>,
    /// ISO-8601 timestamp of the last push.
    #[serde(default)]
    pub pushed_at: String,
}

/// Who can see a repository. `Internal` exists only under an enterprise, where
/// it means every member of that enterprise rather than the wider internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Internal,
}

impl Visibility {
    /// Anything GitHub did not say is public: the safe direction to be wrong in
    /// is the one that leaves a private repository looking unremarkable rather
    /// than announcing a private repository that is not one.
    pub fn parse(raw: &str) -> Self {
        if is(raw, &["PRIVATE"]) {
            Self::Private
        } else if is(raw, &["INTERNAL"]) {
            Self::Internal
        } else {
            Self::Public
        }
    }

    /// The tag for a list row, empty for the case that says nothing.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Public => "",
            Self::Private => "private",
            Self::Internal => "internal",
        }
    }

    /// ANSI 256-colour index, or `None` for public — the ordinary case, which
    /// keeps the terminal's own foreground so the restricted ones stand out.
    pub fn color(self) -> Option<u8> {
        match self {
            Self::Public => None,
            Self::Private => Some(3),  // yellow — yours alone
            Self::Internal => Some(5), // magenta — your enterprise's
        }
    }
}

impl Repo {
    /// The repository name without its owner — the directory it clones into.
    pub fn name(&self) -> &str {
        self.full_name
            .split_once('/')
            .map_or(self.full_name.as_str(), |(_, name)| name)
    }

    pub fn owner(&self) -> &str {
        self.full_name
            .split_once('/')
            .map_or("", |(owner, _)| owner)
    }

    /// What `owner/name` is written as everywhere a slug is wanted.
    pub fn name_with_owner(&self) -> &str {
        &self.full_name
    }

    pub fn description(&self) -> &str {
        self.description.as_deref().unwrap_or_default()
    }

    pub fn language(&self) -> &str {
        self.language.as_deref().unwrap_or_default()
    }

    /// Just the date part of [`Self::pushed_at`].
    pub fn pushed_date(&self) -> &str {
        self.pushed_at.split('T').next().unwrap_or_default()
    }

    pub fn visibility(&self) -> Visibility {
        Visibility::parse(&self.visibility)
    }

    /// The one-word tags worth showing: what makes this repository unusual.
    /// A public, unarchived, non-fork repository is the default and says
    /// nothing.
    pub fn tags(&self) -> Vec<&'static str> {
        let mut tags = Vec::new();
        if self.archived {
            tags.push("archived");
        }
        let visibility = self.visibility().tag();
        if !visibility.is_empty() {
            tags.push(visibility);
        }
        if self.fork {
            tags.push("fork");
        }
        tags
    }
}

/// Parse one page of the repository listing.
pub fn parse_repos(data: &str) -> Result<Vec<Repo>> {
    if data.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut repos: Vec<Repo> =
        serde_json::from_str(data).context("parsing the repository listing")?;
    // `full_name` is deliberately left alone: it is not display text but the
    // slug handed to `gh repo clone`, guarded by [`valid_slug`].
    for repo in &mut repos {
        repo.description = repo.description.as_deref().map(term::one_row);
        repo.language = repo.language.as_deref().map(term::one_row);
    }
    Ok(repos)
}

/// Whether `slug` is a GitHub `owner` or `owner/repo` cid will act on. It is
/// a positional argument to `gh` and a component of the clone path: a leading
/// `-` reads as a flag, and `..` or an extra `/` escapes the root.
pub fn valid_slug(slug: &str) -> bool {
    let parts: Vec<&str> = slug.split('/').collect();
    (1..=2).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                && *part != ".."
                && *part != "."
        })
}

/// Rows per request. GitHub's maximum, so a listing is as few round trips as
/// it can be.
const PAGE_SIZE: usize = 100;

/// How many pages are asked for at once.
///
/// The REST listing numbers its pages, so they can be fetched together — which
/// is the whole reason this is not the GraphQL `gh repo list`, whose cursors
/// only ever hand out the next one. Eight is well inside GitHub's limit on
/// concurrent requests and already turns a listing into about one round trip's
/// wait.
const PAGE_WORKERS: usize = 8;

/// Where an owner's repositories are listed. GitHub keeps orgs, other people
/// and the authenticated user in three different places, and only the last of
/// them shows the private repositories the token's owner has.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Org(String),
    Person(String),
    /// The authenticated user themselves. `users/{login}/repos` answers for
    /// them with their public repositories alone, which would hide every
    /// private repository they have from their own clone listing.
    Me,
}

impl Source {
    /// The path one page of this listing is read from.
    fn page(&self, page: usize) -> String {
        let query = format!("per_page={PAGE_SIZE}&page={page}");
        match self {
            Self::Org(owner) => format!("orgs/{owner}/repos?{query}"),
            Self::Person(owner) => format!("users/{owner}/repos?{query}"),
            Self::Me => format!("user/repos?affiliation=owner&{query}"),
        }
    }
}

/// One page as it came back: the response headers — read only from the first
/// page, empty on the rest — and the body.
type Page = Result<(String, String)>;

/// The last page of a listing, read from the `Link` header GitHub sends with
/// the first — `<…page=4>; rel="last"`. One page when there is no such link,
/// which is what a listing that fits in one says.
fn last_page(headers: &str) -> usize {
    headers
        .lines()
        .find(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("link"))
        })
        .and_then(|link| {
            link.split(',')
                .find(|part| part.contains("rel=\"last\""))
                // On the separator, not on the name: `per_page=100` ends in one
                // too, and matching that reads the page size as the page count.
                .and_then(|part| {
                    part.split_once("&page=")
                        .or_else(|| part.split_once("?page="))
                })
                .and_then(|(_, rest)| {
                    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                    digits.parse().ok()
                })
        })
        .unwrap_or(1)
}

/// Split what `gh api --include` prints into its headers and its body. A
/// response with no blank line in it is all body, which is what an error page
/// cid should try to parse anyway looks like.
fn split_response(raw: &str) -> (&str, &str) {
    raw.split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .unwrap_or(("", raw))
}

/// Whether a failed `gh api` call failed because there is no such thing, as
/// opposed to a network or an authentication failure. Only that one is worth
/// trying somewhere else.
fn is_not_found(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("HTTP 404") || message.contains("Not Found")
}

/// How many pages are asked for before anything is known about how many there
/// are.
///
/// The page count arrives in the first page's own headers, so reading it first
/// would make every listing two round trips. Asking for four at once instead
/// costs a small owner three requests that come back empty — in parallel, so
/// they cost no time — and covers every owner up to four hundred repositories
/// in one.
const SPECULATIVE_PAGES: usize = 4;

/// Where `owner` keeps repositories, once GitHub has been asked.
///
/// An org is tried first: it is the common case, and the request doubles as the
/// first page. Only when GitHub says there is no such org does this ask who the
/// token belongs to, which is what tells a person from the person running
/// cid.
fn person_or_me(owner: &str) -> Source {
    if login().is_ok_and(|login| login.eq_ignore_ascii_case(owner)) {
        Source::Me
    } else {
        Source::Person(owner.to_string())
    }
}

/// Every repository belonging to `owner`, archived ones only when `archived`,
/// and at most `limit` of them.
pub fn list_repos(owner: &str, limit: usize, archived: bool) -> Result<Vec<Repo>> {
    let speculative = limit.div_ceil(PAGE_SIZE).clamp(1, SPECULATIVE_PAGES);

    let org = Source::Org(owner.to_string());
    let (source, bodies, last) = match first_pages(&org, speculative) {
        Ok((bodies, last)) => (org, bodies, last),
        Err(err) if is_not_found(&err) => {
            let source = person_or_me(owner);
            let (bodies, last) = first_pages(&source, speculative)?;
            (source, bodies, last)
        }
        Err(err) => return Err(err),
    };

    let mut repos = Vec::new();
    extend_repos(&mut repos, &bodies, archived)?;

    let mut fetched = speculative;
    while let Some(pages) = next_pages(fetched, last, repos.len(), limit) {
        fetched = *pages.end();
        extend_repos(&mut repos, &fetch_pages(&source, pages)?, archived)?;
    }
    repos.truncate(limit);
    Ok(repos)
}

/// Append the repositories in `bodies` to `repos`, archived ones only when
/// `archived`.
fn extend_repos(repos: &mut Vec<Repo>, bodies: &[String], archived: bool) -> Result<()> {
    for body in bodies {
        let page = parse_repos(body)?;
        repos.extend(page.into_iter().filter(|repo| archived || !repo.archived));
    }
    Ok(())
}

/// The pages to fetch after the first `fetched`, when they yielded `kept` of
/// the `limit` rows wanted and the listing ends at page `last`; `None` once
/// either is reached.
///
/// Archived rows are dropped after they arrive — GitHub's listing has no filter
/// for them — so how many pages hold `limit` rows is not known up front. Each
/// batch covers at least the shortfall, and never fewer than [`PAGE_WORKERS`]
/// pages, since a smaller batch costs the same one round trip.
fn next_pages(
    fetched: usize,
    last: usize,
    kept: usize,
    limit: usize,
) -> Option<std::ops::RangeInclusive<usize>> {
    if kept >= limit || fetched >= last {
        return None;
    }
    let short = (limit - kept).div_ceil(PAGE_SIZE).max(PAGE_WORKERS);
    Some(fetched + 1..=(fetched + short).min(last))
}

/// The first `pages` pages of `source`, and the page the listing actually ends
/// at. A page past the end is an empty array rather than a failure, which is
/// what makes asking for pages nobody may need safe.
///
/// An error on the first page is the listing's error — there is no such owner,
/// or no way to reach GitHub — and is returned rather than joined with the
/// others, since it is the one the caller can act on.
fn first_pages(source: &Source, pages: usize) -> Result<(Vec<String>, usize)> {
    let mut fetched = fetch(source, 1..=pages, true);
    let first = fetched.remove(0)?;
    let last = last_page(&first.0);

    let mut bodies = vec![first.1];
    for page in fetched {
        bodies.push(page?.1);
    }
    Ok((bodies, last))
}

/// Bodies of `pages`, in page order.
fn fetch_pages(source: &Source, pages: std::ops::RangeInclusive<usize>) -> Result<Vec<String>> {
    fetch(source, pages, false)
        .into_iter()
        .map(|page| page.map(|(_, body)| body))
        .collect()
}

/// Fetch `pages` of `source`, [`PAGE_WORKERS`] at a time, and hand them back in
/// page order. `headers` asks for the response headers as well, which only the
/// first page's are read from.
fn fetch(source: &Source, pages: std::ops::RangeInclusive<usize>, headers: bool) -> Vec<Page> {
    let (first, last) = (*pages.start(), *pages.end());
    if first > last {
        return Vec::new();
    }
    let count = last - first + 1;

    let next = AtomicUsize::new(first);
    let fetched: Mutex<Vec<Option<Page>>> = Mutex::new((0..count).map(|_| None).collect());

    std::thread::scope(|scope| {
        for _ in 0..PAGE_WORKERS.min(count) {
            scope.spawn(|| {
                loop {
                    let page = next.fetch_add(1, Ordering::Relaxed);
                    if page > last {
                        return;
                    }
                    let path = source.page(page);
                    let result = if headers && page == first {
                        capture(&["api", "--include", &path]).map(|raw| {
                            let (headers, body) = split_response(&raw);
                            (headers.to_string(), body.to_string())
                        })
                    } else {
                        capture(&["api", &path]).map(|body| (String::new(), body))
                    };
                    if let Ok(mut fetched) = fetched.lock() {
                        fetched[page - first] = Some(result);
                    }
                }
            });
        }
    });

    fetched
        .into_inner()
        .unwrap_or_default()
        .into_iter()
        .map(|page| page.unwrap_or_else(|| Ok((String::new(), String::new()))))
        .collect()
}

/// Clone `owner/repo` into `dest`. Output is captured because clones run
/// concurrently, and returned on failure so the caller can attribute it.
pub fn clone(name_with_owner: &str, dest: &Path) -> Result<()> {
    let _child = stats::in_child();
    let output = Command::new("gh")
        .args(["repo", "clone", name_with_owner, &dest.to_string_lossy()])
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(if stderr.is_empty() {
            format!("cloning {name_with_owner} failed")
        } else {
            stderr.to_string()
        });
    }
    Ok(())
}

/// Open the GitHub page of the repository checked out at `dir`. Which
/// repository that is comes from `dir`'s git remotes, not its path — a renamed
/// clone or a fork makes the two disagree.
///
/// `gh browse` and not `gh repo view --web`, which reaches the same page: the
/// latter fetches the repository's metadata over the API first and waits for
/// it, so opening a browser tab costs a network round trip nothing here reads.
pub fn view_repo_web(dir: &Path) -> Result<()> {
    run_at(Some(dir), &["browse"])
}

/// Who the token belongs to. Asked of GitHub rather than of the config, since
/// it is the answer that decides whether an owner's private repositories are
/// the user's own to list.
pub fn login() -> Result<String> {
    capture(&["api", "user", "--jq", ".login"]).map(|out| out.trim().to_string())
}

/// The authenticated user's login, and the organisations they belong to — the
/// owners to suggest on a machine with nothing cloned yet. Failure is returned
/// rather than swallowed, since a missing `gh` is worth saying.
pub fn owners() -> Result<Vec<String>> {
    // Two independent round trips to GitHub, and both stand between the user
    // and the owner selector. Run together they cost the slower one rather than
    // the sum.
    let (login, orgs) = std::thread::scope(|scope| {
        let orgs = scope.spawn(|| capture(&["api", "user/orgs", "--jq", ".[].login"]));
        let login = login();
        (login, orgs.join())
    });

    let mut out = Vec::new();
    out.extend(login?.split_whitespace().map(str::to_string));
    let orgs = orgs.unwrap_or_else(|e| std::panic::resume_unwind(e))?;
    out.extend(orgs.split_whitespace().map(str::to_string));
    Ok(out)
}

/// Whether `gh` can act as someone on GitHub: `gh auth status` tests the stored
/// token against the host, so this catches an expired or revoked one as well as
/// no login at all.
///
/// A network round trip, and therefore for `doctor` alone — nothing on a
/// keystroke path may ask this.
pub fn authenticated() -> bool {
    let _child = stats::in_child();
    Command::new("gh")
        .args(["auth", "status", "--active"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Run `gh` with the terminal attached, in the working directory cid was
/// invoked from. `gh` writes its own diagnostics, so a failure is [`Reported`].
fn run(args: &[&str]) -> Result<()> {
    run_at(None, args)
}

/// [`run`], optionally somewhere else. Most `gh` subcommands resolve which
/// repository they are about from the directory they run in.
fn run_at(dir: Option<&Path>, args: &[&str]) -> Result<()> {
    let _child = stats::in_child();
    let mut cmd = Command::new("gh");
    cmd.args(args);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let status = cmd.status().map_err(spawn_error)?;
    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

fn capture(args: &[&str]) -> Result<String> {
    let _child = stats::in_child();
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(if stderr.is_empty() {
            format!("gh {} failed", args.join(" "))
        } else {
            stderr.to_string()
        });
    }
    Ok(into_string(output.stdout))
}

/// Take ownership of a child's output as a `String`, reusing the buffer when it
/// is already valid UTF-8 — `gh repo list` over a large org is megabytes.
fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// A missing `gh` is the one failure worth explaining, since PR support is the
/// only thing that depends on it.
fn spawn_error(err: std::io::Error) -> anyhow::Error {
    match err.kind() {
        ErrorKind::NotFound => anyhow!(
            "`gh` was not found on PATH — pull request commands need the GitHub CLI (https://cli.github.com), authenticated with `gh auth login`"
        ),
        _ => anyhow!(err).context("running gh"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
        {"number":12,"title":"Add branch selector","author":{"login":"joakimen"},
         "headRefName":"feat/branches","isDraft":false,"state":"OPEN",
         "updatedAt":"2026-07-27T09:12:33Z"},
        {"number":9,"title":"WIP","author":null,"headRefName":"wip",
         "isDraft":true,"state":"OPEN","updatedAt":"2026-07-20T11:00:00Z"}
    ]"#;

    #[test]
    fn a_pull_request_cannot_carry_an_escape_into_a_listing() {
        let prs = parse_prs(
            r#"[{"number":1,"title":"fix\u001b[2K\u001b[32m all checks pass",
                 "author":{"login":"a\u001b[31mb"},"headRefName":"x\u001b[0m",
                 "body":"para\u001b[2Kone\n\npara two","state":"OPEN",
                 "statusCheckRollup":[{"name":"bu\u001b[2Kild","status":"COMPLETED",
                 "conclusion":"FAILURE"}]}]"#,
        )
        .unwrap();
        let pr = &prs[0];
        for text in [&pr.title, &pr.body, &pr.head_ref_name] {
            assert!(!text.contains('\x1b'), "{text:?}");
        }
        assert!(!pr.author_login().contains('\x1b'));
        assert!(!pr.status_check_rollup[0].name.contains('\x1b'));
        assert_eq!(pr.checks().failed, 1);
    }

    #[test]
    fn a_repository_description_cannot_carry_an_escape_either() {
        let repos = parse_repos(
            r#"[{"full_name":"acme/api","description":"a\u001b[2Kb",
                 "language":"R\u001b[31must"}]"#,
        )
        .unwrap();
        assert_eq!(repos[0].description(), "a[2Kb");
        assert_eq!(repos[0].language(), "R[31must");
        // Not display text: rewriting it would clone something else.
        assert_eq!(repos[0].name_with_owner(), "acme/api");
    }

    /// A repository with neither description nor language comes back with
    /// nulls rather than empty strings.
    #[test]
    fn a_repository_with_nothing_written_about_it_still_parses() {
        let repos = parse_repos(r#"[{"full_name":"acme/api","description":null,"language":null}]"#)
            .unwrap();
        assert_eq!(repos[0].description(), "");
        assert_eq!(repos[0].language(), "");
        assert_eq!(repos[0].name(), "api");
        assert_eq!(repos[0].owner(), "acme");
    }

    #[test]
    fn only_names_github_could_have_issued_are_valid_slugs() {
        for good in ["joakimen", "acme/api", "a-b_c.d", "acme/my.repo"] {
            assert!(valid_slug(good), "{good} rejected");
        }
        for bad in [
            "-L",              // read by gh as a flag
            "--json",          //
            "..",              // walks out of the root
            "../../etc",       //
            "acme/../../etc",  //
            "a/b/c",           // a third component the layout has no place for
            "",                //
            "acme/",           // an empty component
            "/api",            //
            "acme/api;whoami", // shell metacharacters, if one ever reaches a shell
            "acme api",
        ] {
            assert!(!valid_slug(bad), "{bad} accepted");
        }
    }

    #[test]
    fn parses_gh_output() {
        let prs = parse_prs(SAMPLE).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 12);
        assert_eq!(prs[0].head_ref_name, "feat/branches");
        assert_eq!(prs[0].author_login(), "joakimen");
        assert_eq!(prs[0].updated_date(), "2026-07-27");
        assert!(prs[1].is_draft);
    }

    /// A deleted account leaves `author: null`; the listing must still render.
    #[test]
    fn missing_author_falls_back() {
        let prs = parse_prs(SAMPLE).unwrap();
        assert_eq!(prs[1].author_login(), "unknown");
    }

    #[test]
    fn empty_output_is_no_prs() {
        assert!(parse_prs("").unwrap().is_empty());
        assert!(parse_prs("[]").unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(parse_prs("{oops").is_err());
    }

    #[test]
    fn colours_by_state() {
        assert_eq!(color_for(false, "OPEN"), 2);
        assert_eq!(color_for(true, "OPEN"), 8);
        assert_eq!(color_for(false, "MERGED"), 5);
        assert_eq!(color_for(false, "CLOSED"), 1);
        // A draft that was merged or closed is history first.
        assert_eq!(color_for(true, "MERGED"), 5);
    }

    #[test]
    fn tags_match_colours() {
        assert_eq!(tag_for(false, "OPEN"), "open");
        assert_eq!(tag_for(true, "OPEN"), "draft");
        assert_eq!(tag_for(false, "MERGED"), "merged");
        assert_eq!(tag_for(false, "CLOSED"), "closed");
    }

    const CHECKS: &str = r#"[
        {"number":1,"title":"Mixed","headRefName":"m","state":"OPEN",
         "mergeable":"MERGEABLE","statusCheckRollup":[
            {"__typename":"CheckRun","name":"build","workflowName":"ci",
             "status":"COMPLETED","conclusion":"SUCCESS"},
            {"__typename":"CheckRun","name":"lint","workflowName":"ci",
             "status":"COMPLETED","conclusion":"FAILURE"},
            {"__typename":"CheckRun","name":"e2e","workflowName":"ci",
             "status":"IN_PROGRESS","conclusion":""},
            {"__typename":"CheckRun","name":"docs","workflowName":"ci",
             "status":"COMPLETED","conclusion":"SKIPPED"},
            {"__typename":"StatusContext","context":"ci/legacy","state":"SUCCESS"},
            {"__typename":"StatusContext","context":"ci/flaky","state":"ERROR"}
         ]}
    ]"#;

    fn mixed() -> PullRequest {
        parse_prs(CHECKS).unwrap().pop().unwrap()
    }

    #[test]
    fn counts_both_check_shapes() {
        let checks = mixed().checks();
        // SUCCESS + SKIPPED + StatusContext SUCCESS.
        assert_eq!(checks.passed, 3);
        // CheckRun FAILURE + StatusContext ERROR.
        assert_eq!(checks.failed, 2);
        assert_eq!(checks.pending, 1);
        assert_eq!(checks.total(), 6);
    }

    #[test]
    fn one_failure_makes_the_set_failing() {
        assert_eq!(mixed().checks().tag(), "fail");
        assert_eq!(
            Checks {
                passed: 9,
                failed: 0,
                pending: 1
            }
            .tag(),
            "pending"
        );
        assert_eq!(
            Checks {
                passed: 9,
                failed: 0,
                pending: 0
            }
            .tag(),
            "pass"
        );
    }

    #[test]
    fn no_checks_is_blank_not_pending() {
        let checks = Checks::default();
        assert!(checks.is_empty());
        assert_eq!(checks.tag(), "");
        assert_eq!(checks.summary(), "");
    }

    #[test]
    fn skipped_and_neutral_pass() {
        assert_eq!(result_for("COMPLETED", "SKIPPED", ""), CheckResult::Pass);
        assert_eq!(result_for("COMPLETED", "NEUTRAL", ""), CheckResult::Pass);
        assert_eq!(result_for("COMPLETED", "CANCELLED", ""), CheckResult::Fail);
        assert_eq!(result_for("COMPLETED", "TIMED_OUT", ""), CheckResult::Fail);
        assert_eq!(result_for("QUEUED", "", ""), CheckResult::Pending);
        // A StatusContext's `state` decides on its own.
        assert_eq!(result_for("", "", "PENDING"), CheckResult::Pending);
        assert_eq!(result_for("", "", "FAILURE"), CheckResult::Fail);
    }

    #[test]
    fn summary_leads_with_the_bad_news() {
        assert_eq!(mixed().checks().summary(), "2 failed, 1 pending, 3 passed");
    }

    #[test]
    fn failing_checks_are_listed_failures_first() {
        let pr = mixed();
        let names: Vec<String> = pr.failing_checks().iter().map(|c| c.label()).collect();
        assert_eq!(names, ["lint (ci)", "ci/flaky", "e2e (ci)"]);
    }

    #[test]
    fn mergeable_states_parse() {
        assert_eq!(Mergeable::parse("MERGEABLE"), Mergeable::Clean);
        assert_eq!(Mergeable::parse("CONFLICTING"), Mergeable::Conflicting);
        assert_eq!(Mergeable::parse("UNKNOWN"), Mergeable::Unknown);
        assert_eq!(mixed().mergeable(), Mergeable::Clean);
    }

    #[test]
    fn unknown_mergeability_renders_as_nothing() {
        assert_eq!(Mergeable::parse("UNKNOWN").tag(), "");
        assert_eq!(Mergeable::parse("").tag(), "");
    }

    #[test]
    fn check_fields_are_optional() {
        let prs = parse_prs(SAMPLE).unwrap();
        assert!(prs[0].checks().is_empty());
        assert_eq!(prs[0].mergeable(), Mergeable::Unknown);
    }

    #[test]
    fn every_status_glyph_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        let glyphs = [
            CheckResult::Pass.glyph(),
            CheckResult::Fail.glyph(),
            CheckResult::Pending.glyph(),
            Mergeable::Conflicting.glyph(),
            Mergeable::Clean.glyph(),
            Mergeable::Unknown.glyph(),
            Checks::default().glyph(),
            NO_GLYPH,
        ];
        for glyph in glyphs {
            assert_eq!(
                glyph.width(),
                GLYPH_WIDTH,
                "{glyph:?} is not {GLYPH_WIDTH} columns wide"
            );
        }
    }

    #[test]
    fn readiness_is_not_state() {
        let ready = mixed();
        // `mixed` has a failing check.
        assert_eq!(ready.readiness(), Readiness::Blocked);

        let prs = parse_prs(SAMPLE).unwrap();
        // Open, no checks, mergeability unknown: nothing is in the way.
        assert_eq!(prs[0].readiness(), Readiness::Ready);
        // A draft is not blocked, it is simply not on offer.
        assert_eq!(prs[1].readiness(), Readiness::Unavailable);
    }

    #[test]
    fn merged_is_never_ready() {
        let pr = parse_prs(
            r#"[{"number":1,"title":"t","headRefName":"h","state":"MERGED",
                 "statusCheckRollup":[
                   {"name":"build","status":"COMPLETED","conclusion":"SUCCESS"}]}]"#,
        )
        .unwrap()
        .pop()
        .unwrap();
        assert_eq!(pr.checks().tag(), "pass");
        assert_eq!(pr.readiness(), Readiness::Unavailable);
    }

    #[test]
    fn a_conflict_blocks_a_green_pull_request() {
        let pr = parse_prs(
            r#"[{"number":1,"title":"t","headRefName":"h","state":"OPEN",
                 "mergeable":"CONFLICTING","statusCheckRollup":[
                   {"name":"build","status":"COMPLETED","conclusion":"SUCCESS"}]}]"#,
        )
        .unwrap()
        .pop()
        .unwrap();
        assert_eq!(pr.checks().tag(), "pass");
        assert_eq!(pr.readiness(), Readiness::Blocked);
    }

    #[test]
    fn merge_methods_map_to_gh_flags() {
        assert_eq!(MergeMethod::Merge.flag(), "--merge");
        assert_eq!(MergeMethod::Squash.flag(), "--squash");
        assert_eq!(MergeMethod::Rebase.flag(), "--rebase");
    }

    #[test]
    fn visibility_parses_what_github_says() {
        assert_eq!(Visibility::parse("PRIVATE"), Visibility::Private);
        assert_eq!(Visibility::parse("internal"), Visibility::Internal);
        assert_eq!(Visibility::parse("PUBLIC"), Visibility::Public);
        // A field GitHub did not send must not read as restricted.
        assert_eq!(Visibility::parse(""), Visibility::Public);
    }

    #[test]
    fn each_visibility_reads_differently() {
        let colors = [
            Visibility::Public.color(),
            Visibility::Private.color(),
            Visibility::Internal.color(),
        ];
        let mut distinct = colors.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), colors.len(), "two visibilities look alike");
        assert_eq!(
            Visibility::Public.color(),
            None,
            "the ordinary case is bare"
        );
    }

    /// The three places GitHub keeps repositories, and the one that is not
    /// interchangeable with the others: `users/{me}/repos` answers for the
    /// token's owner with their public repositories alone.
    #[test]
    fn each_kind_of_owner_is_listed_where_github_keeps_it() {
        assert_eq!(
            Source::Org("acme".into()).page(2),
            "orgs/acme/repos?per_page=100&page=2"
        );
        assert_eq!(
            Source::Person("torvalds".into()).page(1),
            "users/torvalds/repos?per_page=100&page=1"
        );
        assert_eq!(
            Source::Me.page(1),
            "user/repos?affiliation=owner&per_page=100&page=1"
        );
    }

    /// How many round trips the listing is comes from the `Link` header of the
    /// first, which is the only thing that makes the rest of them parallel.
    #[test]
    fn the_page_count_comes_from_the_link_header() {
        let header = "Link: <https://api.github.com/organizations/1/repos?per_page=100&page=2>; \
             rel=\"next\", <https://api.github.com/organizations/1/repos?per_page=100&page=4>; \
             rel=\"last\"";
        assert_eq!(last_page(header), 4);

        // A listing that fits in one page is sent without the header at all.
        assert_eq!(last_page("Date: today\r\nServer: github.com"), 1);
        assert_eq!(last_page(""), 1);
        // A `next` with no `last` is the final page of a cursor-style listing.
        assert_eq!(last_page("link: <https://x/?page=9>; rel=\"next\""), 1);
    }

    #[test]
    fn archived_rows_do_not_count_toward_the_limit() {
        // Ten pages read, half of every one archived: 500 kept of 1000 wanted.
        assert_eq!(next_pages(10, 20, 500, 1000), Some(11..=18));
    }

    #[test]
    fn a_batch_is_never_smaller_than_the_workers_that_fetch_it() {
        // One page short, but eight cost the same wait as one.
        assert_eq!(next_pages(4, 30, 390, 400), Some(5..=12));
        assert_eq!(next_pages(4, 30, 100, 1000), Some(5..=13));
    }

    #[test]
    fn fetching_stops_at_the_limit_or_the_last_page() {
        assert_eq!(next_pages(4, 30, 400, 400), None);
        assert_eq!(next_pages(4, 30, 450, 400), None);
        assert_eq!(next_pages(4, 4, 10, 1000), None);
        assert_eq!(next_pages(4, 6, 10, 1000), Some(5..=6));
    }

    #[test]
    fn a_response_is_split_where_its_headers_end() {
        let (headers, body) = split_response("HTTP/2 200\r\nLink: x\r\n\r\n[{\"a\":1}]");
        assert_eq!(headers, "HTTP/2 200\r\nLink: x");
        assert_eq!(body, "[{\"a\":1}]");

        // Nothing but a body is a body, rather than a body read as headers.
        let (headers, body) = split_response("[]");
        assert_eq!(headers, "");
        assert_eq!(body, "[]");
    }
}
