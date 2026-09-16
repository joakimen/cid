//! Git branch enumeration and checkout, and the repository's worktrees.
//!
//! The decisions — which refs are local, which are remote-only, what a checkout
//! of a given name means — are pure functions over plain data
//! ([`parse_ref_lines`], [`classify`], [`resolve`], [`parse_worktrees`]).
//!
//! One `for-each-ref` over `refs/heads` and `refs/remotes` enumerates both in a
//! pass already ordered by commit recency; [`by_relevance`] regroups it without
//! asking git anything more, and the same pass carries `origin/HEAD` for
//! [`default_branch`].

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::Reported;
use crate::stats;

/// Field separator in the `for-each-ref` format. ASCII unit separator, so
/// commit subjects containing tabs or pipes cannot split a row.
const SEP: char = '\x1f';

/// Where a branch exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    /// Only in this clone, and never pushed.
    Local,
    /// Pushed once, but the remote branch it tracked has since been deleted —
    /// on GitHub, most often by merging its pull request.
    Gone,
    /// Both here and on a remote.
    Tracked,
    /// Only on a remote; checking it out creates the local branch.
    Remote,
}

impl BranchKind {
    /// ANSI 256-colour index used for this kind, in both the selector and `ls`.
    /// Standard hues so they follow the terminal theme.
    pub fn color(self) -> u8 {
        match self {
            BranchKind::Local => 3,   // yellow — here only
            BranchKind::Gone => 4,    // blue   — its remote side is gone
            BranchKind::Tracked => 2, // green  — in sync with a remote
            BranchKind::Remote => 6,  // cyan   — not here yet
        }
    }

    /// Short tag shown in `branch ls --status`, for when colour is unavailable.
    pub fn tag(self) -> &'static str {
        match self {
            BranchKind::Local => "local",
            BranchKind::Gone => "gone",
            BranchKind::Tracked => "both",
            BranchKind::Remote => "remote",
        }
    }
}

/// Which branches a listing includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Local and remote-only branches. The default.
    #[default]
    All,
    /// Branches that exist in this clone ([`BranchKind::Local`],
    /// [`BranchKind::Gone`] and [`BranchKind::Tracked`]).
    Local,
    /// Branches that exist on a remote ([`BranchKind::Tracked`] and
    /// [`BranchKind::Remote`]).
    Remote,
}

impl Filter {
    /// Resolve the mutually exclusive `--local` / `--remote` flags.
    pub fn from_flags(local: bool, remote: bool) -> Self {
        match (local, remote) {
            (true, false) => Filter::Local,
            (false, true) => Filter::Remote,
            _ => Filter::All,
        }
    }

    fn accepts(self, kind: BranchKind) -> bool {
        match self {
            Filter::All => true,
            Filter::Local => kind != BranchKind::Remote,
            Filter::Remote => matches!(kind, BranchKind::Tracked | BranchKind::Remote),
        }
    }
}

/// A branch as offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Displayed and returned name: `main` for a local branch, `origin/main`
    /// for a remote-only one, so the remote is never ambiguous.
    pub name: String,
    pub kind: BranchKind,
    /// Whether this is the currently checked-out branch.
    pub head: bool,
    /// Relative commit date, e.g. `2 days ago`.
    pub date: String,
    /// Commit subject line.
    pub subject: String,
}

/// One row of `for-each-ref` output, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLine {
    pub refname: String,
    pub head: bool,
    /// What a symbolic ref points at, in short form: `origin/main` for
    /// `refs/remotes/origin/HEAD`. Empty for every ordinary branch.
    pub symref: String,
    /// Configured upstream in short form (`origin/main`), empty when unset.
    pub upstream: String,
    pub date: String,
    pub subject: String,
}

/// What checking out a given name should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkout {
    /// Switch to an existing local branch.
    Switch(String),
    /// Create a local branch tracking `remote_ref` (`origin/main`).
    Track { remote_ref: String },
}

/// Parse `for-each-ref` output written with the [`SEP`]-joined format. Rows
/// with too few fields are skipped rather than failing the listing.
pub fn parse_ref_lines(output: &str) -> Vec<RefLine> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut fields = line.splitn(6, SEP);
            let refname = fields.next()?;
            let head = fields.next()?;
            let symref = fields.next()?;
            let upstream = fields.next()?;
            let date = fields.next()?;
            // The subject is last and unsplit, so separators inside it survive.
            let subject = fields.next().unwrap_or_default();
            if refname.is_empty() {
                return None;
            }
            Some(RefLine {
                refname: refname.to_string(),
                head: head.trim() == "*",
                symref: symref.to_string(),
                upstream: upstream.to_string(),
                date: date.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// Split a remote ref short name (`origin/feat/x`) into remote and branch.
/// Branch names may contain `/`; remote names may not, so the first component
/// is the remote.
fn split_remote(short: &str) -> Option<(&str, &str)> {
    short
        .split_once('/')
        .filter(|(r, b)| !r.is_empty() && !b.is_empty())
}

/// Fold ref rows into the branch list shown to the user: a local branch whose
/// name also exists on a remote is [`BranchKind::Tracked`] rather than two
/// rows, and `origin/HEAD` is dropped. Input order is preserved.
///
/// A local branch whose configured upstream names a ref that no longer exists
/// is [`BranchKind::Gone`] — git's own `[gone]`, worked out from the same rows
/// rather than asked for again.
pub fn classify(lines: &[RefLine]) -> Vec<Branch> {
    let locals: HashSet<&str> = lines
        .iter()
        .filter_map(|line| line.refname.strip_prefix("refs/heads/"))
        .collect();

    // Remote refs already spoken for as some local branch's upstream, so a
    // branch tracked under a different name is not also listed on its own.
    let upstreams: HashSet<&str> = lines
        .iter()
        .filter(|line| line.refname.starts_with("refs/heads/"))
        .map(|line| line.upstream.as_str())
        .filter(|upstream| !upstream.is_empty())
        .collect();

    // Branch parts of every remote ref, across all remotes: `origin/main` and
    // `fork/main` both contribute `main`.
    let on_remote: HashSet<&str> = lines
        .iter()
        .filter_map(|line| line.refname.strip_prefix("refs/remotes/"))
        .filter(|short| !short.ends_with("/HEAD"))
        .filter_map(|short| split_remote(short).map(|(_, branch)| branch))
        .collect();

    // Every ref by the short name an upstream is written in: `origin/main` for
    // a remote branch, and `main` for a local one tracked with `branch -u main`.
    let refs: HashSet<&str> = lines
        .iter()
        .filter_map(|line| {
            line.refname
                .strip_prefix("refs/remotes/")
                .or_else(|| line.refname.strip_prefix("refs/heads/"))
        })
        .collect();

    let mut branches = Vec::new();
    for line in lines {
        if let Some(name) = line.refname.strip_prefix("refs/heads/") {
            // Prefer the configured upstream when it points somewhere else, so
            // `git branch -u origin/other` still reads as tracked.
            let counterpart = match split_remote(&line.upstream) {
                Some((_, branch)) => branch,
                None => name,
            };
            let kind = if !line.upstream.is_empty() && !refs.contains(line.upstream.as_str()) {
                BranchKind::Gone
            } else if on_remote.contains(counterpart) {
                BranchKind::Tracked
            } else {
                BranchKind::Local
            };
            branches.push(Branch {
                name: name.to_string(),
                kind,
                head: line.head,
                date: line.date.clone(),
                subject: line.subject.clone(),
            });
        } else if let Some(short) = line.refname.strip_prefix("refs/remotes/") {
            if short.ends_with("/HEAD") {
                continue;
            }
            let Some((_, branch)) = split_remote(short) else {
                continue;
            };
            // Already represented by its local counterpart.
            if locals.contains(branch) || upstreams.contains(short) {
                continue;
            }
            branches.push(Branch {
                name: short.to_string(),
                kind: BranchKind::Remote,
                head: false,
                date: line.date.clone(),
                subject: line.subject.clone(),
            });
        }
    }
    branches
}

/// The repository's default branch, as a bare branch name (`main`), from the
/// same `for-each-ref` pass the branch list comes from.
///
/// `refs/remotes/<remote>/HEAD` is the answer where it exists, but only `git
/// clone` writes it: a repository started with `git init` and pushed has none
/// until someone runs `git remote set-head`. The fallback is the name, since a
/// repository whose default is neither `main` nor `master` and that is also
/// missing that ref has nothing left to ask.
pub fn default_branch(lines: &[RefLine]) -> Option<String> {
    let from_head = lines
        .iter()
        .filter(|line| line.refname.starts_with("refs/remotes/") && line.refname.ends_with("/HEAD"))
        .find_map(|line| split_remote(&line.symref));
    if let Some((_, branch)) = from_head {
        return Some(branch.to_string());
    }

    let known: HashSet<&str> = lines
        .iter()
        .filter_map(|line| match line.refname.strip_prefix("refs/heads/") {
            Some(name) => Some(name),
            None => line
                .refname
                .strip_prefix("refs/remotes/")
                .and_then(split_remote)
                .map(|(_, branch)| branch),
        })
        .collect();
    ["main", "master"]
        .into_iter()
        .find(|name| known.contains(name))
        .map(str::to_string)
}

/// Whether `branch` is the branch `default` names. A remote-only row carries
/// its remote in the name, so `origin/main` is the default where `main` is,
/// while a local `feature/main` is not.
fn is_default(branch: &Branch, default: &str) -> bool {
    match branch.kind {
        BranchKind::Remote => split_remote(&branch.name).is_some_and(|(_, name)| name == default),
        _ => branch.name == default,
    }
}

/// Which block of the listing a branch belongs in: the default branch, then the
/// current one, then what is already in this clone, then what is only on a
/// remote.
fn tier(branch: &Branch, default: Option<&str>) -> u8 {
    if default.is_some_and(|default| is_default(branch, default)) {
        return 0;
    }
    match (branch.head, branch.kind) {
        (true, _) => 1,
        (_, BranchKind::Remote) => 3,
        _ => 2,
    }
}

/// Order branches by how likely one is to be the one wanted: the default
/// branch, then the current branch, then local branches, then remote-only ones,
/// each block most recently committed first. The recency half comes from git's
/// own `--sort`, so this only regroups it.
///
/// The default branch leads because the list is mostly read on the way off a
/// feature branch, and the row above it — the branch already checked out —
/// would be a selection that does nothing. Standing on the default branch
/// leaves one row in both blocks and the order unchanged.
pub fn by_relevance(mut branches: Vec<Branch>, default: Option<&str>) -> Vec<Branch> {
    // Stable, so committer-date order survives inside each block.
    branches.sort_by_key(|branch| tier(branch, default));
    branches
}

/// Apply a [`Filter`] to a branch list.
pub fn filtered(branches: Vec<Branch>, filter: Filter) -> Vec<Branch> {
    branches
        .into_iter()
        .filter(|branch| filter.accepts(branch.kind))
        .collect()
}

/// Decide what checking out `input` means, given the known branches.
///
/// Rules, in order:
/// - A name matching a listed branch uses that branch's kind: local branches
///   are switched to, remote-only ones get a tracking branch created.
/// - `origin/foo` where a local `foo` already exists switches to the local
///   branch, rather than detaching HEAD at the remote ref.
/// - Anything else is handed to git verbatim, so its own resolution (and its
///   error message) still applies.
pub fn resolve(branches: &[Branch], input: &str) -> Checkout {
    if let Some(branch) = branches.iter().find(|b| b.name == input) {
        return match branch.kind {
            BranchKind::Remote => Checkout::Track {
                remote_ref: branch.name.clone(),
            },
            _ => Checkout::Switch(branch.name.clone()),
        };
    }

    // `origin/foo` when `foo` is checked out locally: the user means `foo`.
    if let Some((_, tail)) = split_remote(input)
        && branches
            .iter()
            .any(|b| b.kind != BranchKind::Remote && b.name == tail)
    {
        return Checkout::Switch(tail.to_string());
    }

    Checkout::Switch(input.to_string())
}

/// What a name given to `worktree add` means: the three ways a new tree can
/// come by the branch it checks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeSource {
    /// Check out a local branch that already exists.
    Branch(String),
    /// Create `branch` from `remote_ref` and track it, as [`Checkout::Track`]
    /// does for the current tree.
    Track { remote_ref: String, branch: String },
    /// Create a branch here, from what is checked out now.
    New(String),
}

impl TreeSource {
    /// The local branch the new tree will be on, whichever way it got there.
    pub fn branch(&self) -> &str {
        match self {
            Self::Branch(name) | Self::New(name) => name,
            Self::Track { branch, .. } => branch,
        }
    }
}

/// Decide where a new tree's branch comes from, given the known branches.
///
/// [`resolve`]'s rules, and then two of its own, because `git worktree add`
/// does not guess the way `git switch` does:
///
/// - A bare name only a remote has creates the local branch tracking it. This
///   is git's own DWIM, which `switch` performs and `worktree add` does not —
///   left to git, `worktree add -b feat/x` would build a *new* branch from
///   HEAD that shares a name with the remote one and nothing else. Ambiguous
///   across remotes, it is left alone, as a branch to create.
/// - A name matching nothing at all is a branch to create rather than an error.
///   Adding a tree is how a piece of work starts, and its branch usually does
///   not exist yet.
pub fn tree_source(branches: &[Branch], input: &str) -> TreeSource {
    let tracking = |remote_ref: &str| TreeSource::Track {
        branch: split_remote(remote_ref)
            .map(|(_, branch)| branch.to_string())
            .unwrap_or_else(|| remote_ref.to_string()),
        remote_ref: remote_ref.to_string(),
    };

    match resolve(branches, input) {
        Checkout::Track { remote_ref } => tracking(&remote_ref),
        Checkout::Switch(name) if branches.iter().any(|b| b.name == name) => {
            TreeSource::Branch(name)
        }
        Checkout::Switch(name) => {
            let mut remotes = branches
                .iter()
                .filter(|b| b.kind == BranchKind::Remote)
                .filter(|b| split_remote(&b.name).is_some_and(|(_, tail)| tail == name));
            match (remotes.next(), remotes.next()) {
                (Some(only), None) => tracking(&only.name),
                _ => TreeSource::New(name),
            }
        }
    }
}

/// A working tree of the repository: the main one, or one added with
/// `git worktree add`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worktree {
    /// Absolute path to the tree, as git reports it.
    pub path: PathBuf,
    /// Short branch name, empty when HEAD is detached or the tree is bare.
    pub branch: String,
    /// Full HEAD commit, empty for a bare tree — which has no HEAD.
    pub head: String,
    /// A bare repository: nothing checked out, and nowhere to work.
    pub bare: bool,
    /// Held by `git worktree lock`, so pruning leaves it alone.
    pub locked: bool,
    /// git can no longer use it — most often because its directory is gone.
    pub prunable: bool,
    /// Whether this is the tree the working directory is in.
    pub current: bool,
}

/// How much of a commit to show where a branch name would be. Fixed rather than
/// git's `core.abbrev`, which grows with the repository and would make the
/// column's width depend on which one is being listed; 7 is what git itself
/// shows in a repository of ordinary size.
const SHORT_COMMIT: usize = 7;

impl Worktree {
    /// What identifies the tree beside its path: the branch it has checked out,
    /// the commit a detached HEAD sits on, or that the repository is bare.
    /// Parenthesised where it is not a branch name, so the two never read alike.
    pub fn head_label(&self) -> String {
        if self.bare {
            return "(bare)".to_string();
        }
        if !self.branch.is_empty() {
            return self.branch.clone();
        }
        let short: String = self.head.chars().take(SHORT_COMMIT).collect();
        format!("({short})")
    }

    /// Short tags for the states worth flagging, for when colour is
    /// unavailable. An ordinary tree has none.
    pub fn tags(&self) -> Vec<&'static str> {
        let mut tags = Vec::new();
        if self.locked {
            tags.push("locked");
        }
        if self.prunable {
            tags.push("prunable");
        }
        tags
    }

    /// ANSI 256-colour index for the row, or `None` for an ordinary tree, which
    /// keeps the terminal's foreground. Standard hues, so they follow the theme.
    pub fn color(&self) -> Option<u8> {
        // Checked before `current`: a tree git cannot use is not one to enter,
        // and that outranks where the shell happens to be standing.
        if self.prunable {
            return Some(8); // grey — nothing to switch to
        }
        if self.current {
            return Some(2); // green — where you already are
        }
        if self.branch.is_empty() {
            return Some(3); // yellow — detached or bare, so no branch to land on
        }
        None
    }
}

/// Parse `git worktree list --porcelain`.
///
/// Each record opens with a `worktree <path>` line and is followed by its
/// attributes, one per line, valueless where the attribute is a flag. A record
/// is closed by the next `worktree` line rather than by the blank line between
/// them, so an attribute git adds later cannot be mistaken for a new record.
pub fn parse_worktrees(output: &str) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<Worktree> = None;

    for line in output.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            worktrees.extend(current.take());
            if !value.is_empty() {
                current = Some(Worktree {
                    path: PathBuf::from(value),
                    ..Worktree::default()
                });
            }
            continue;
        }
        // Attributes before the first `worktree` line belong to nothing.
        let Some(worktree) = current.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => worktree.head = value.to_string(),
            "branch" => worktree.branch = value.strip_prefix("refs/heads/").unwrap_or(value).into(),
            "bare" => worktree.bare = true,
            // Both carry an optional reason, which the listing has no room for.
            "locked" => worktree.locked = true,
            "prunable" => worktree.prunable = true,
            _ => {}
        }
    }

    worktrees.extend(current);
    worktrees
}

/// Mark the tree `here` is inside, so a listing can point at it.
///
/// `resolve` is `canonicalize`: git prints the real path of every worktree
/// while the working directory may have reached one through a symlink, and only
/// the resolved forms compare equal. What is displayed stays as git gave it.
pub fn mark_current(
    worktrees: Vec<Worktree>,
    here: Option<&Path>,
    resolve: impl Fn(&Path) -> PathBuf,
) -> Vec<Worktree> {
    let Some(here) = here.map(&resolve) else {
        return worktrees;
    };
    worktrees
        .into_iter()
        .map(|worktree| Worktree {
            current: resolve(&worktree.path) == here,
            ..worktree
        })
        .collect()
}

/// A missing `git` is worth explaining; anything else is the spawn's own
/// failure. Shared by every helper below, so one wording covers all of them.
fn spawn_error(err: std::io::Error) -> anyhow::Error {
    match err.kind() {
        ErrorKind::NotFound => anyhow!("`git` was not found on PATH"),
        _ => anyhow!(err).context("running git"),
    }
}

/// Run git with `args`, capturing stdout.
fn capture(args: &[&str]) -> Result<String> {
    let _child = stats::in_child();
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        bail!(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr.to_string()
        });
    }
    // Reuse the buffer; the copy is paid only on malformed output.
    Ok(String::from_utf8(output.stdout)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Run git with `args`, letting it write straight to the terminal. A failure is
/// [`Reported`], since git has already said why.
fn passthrough(args: &[&str]) -> Result<()> {
    let _child = stats::in_child();
    let status = Command::new("git")
        .args(args)
        .status()
        .map_err(spawn_error)?;
    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

/// [`passthrough`], with the child's stdout sent to cid's stderr.
///
/// `git worktree add` narrates its checkout on stdout — `HEAD is now at …`, and
/// the line saying an upstream was set — while stdout is also where cid puts
/// the path a shell reads to `cd` into the new tree. Both are worth keeping, so
/// git keeps its voice and gives up the channel.
fn passthrough_onto_stderr(args: &[&str]) -> Result<()> {
    use std::os::fd::AsFd;

    let stderr = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .context("duplicating stderr")?;
    let _child = stats::in_child();
    let status = Command::new("git")
        .args(args)
        .stdout(Stdio::from(stderr))
        .status()
        .map_err(spawn_error)?;
    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

/// Fail early, and with a better message than git's, when the working
/// directory is not inside a repository.
pub fn ensure_repo() -> Result<()> {
    let _child = stats::in_child();
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(spawn_error)?;
    if !inside.success() {
        bail!("not inside a git repository");
    }
    Ok(())
}

/// The root of the repository the working directory is in, if it is in one.
/// Absence is an answer rather than an error.
pub fn repo_root() -> Option<PathBuf> {
    require_repo_root().ok()
}

/// [`repo_root`] where being outside a repository is a failure, with the same
/// message [`ensure_repo`] gives.
///
/// One `rev-parse` answers both questions, so a command that needs the root
/// does not also spawn git to be told it is in a repository at all.
pub fn require_repo_root() -> Result<PathBuf> {
    let _child = stats::in_child();
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || root.is_empty() {
        bail!("not inside a git repository");
    }
    Ok(PathBuf::from(root))
}

/// The branch the working directory has checked out.
///
/// `None` on a detached HEAD, where `rev-parse` answers with the literal
/// `HEAD`: there is no branch, and so nothing a pull request could be opened
/// from. Absence is an answer rather than an error, as in [`repo_root`].
pub fn current_branch() -> Option<String> {
    let _child = stats::in_child();
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Every branch in the repository, ordered by [`by_relevance`]: the default
/// branch, then the current one, then local branches, then remote-only ones,
/// newest first within each.
pub fn branches() -> Result<Vec<Branch>> {
    let format = format!(
        "--format=%(refname){SEP}%(HEAD){SEP}%(symref:short){SEP}%(upstream:short){SEP}%(committerdate:relative){SEP}%(subject)"
    );
    let out = capture(&[
        "for-each-ref",
        "--sort=-committerdate",
        &format,
        "refs/heads",
        "refs/remotes",
    ])
    .context("listing branches")?;
    let lines = parse_ref_lines(&out);
    let default = default_branch(&lines);
    Ok(by_relevance(classify(&lines), default.as_deref()))
}

/// Every working tree of the repository, in git's own order: the main tree
/// first, then the linked ones as they were added. The tree the working
/// directory is in is marked rather than moved, since where you already are is
/// rarely where you are going.
///
/// `here` is the root of the tree the working directory is in — the caller has
/// it from [`require_repo_root`], which is also what established there is a
/// repository to list.
pub fn worktrees(here: &Path) -> Result<Vec<Worktree>> {
    let out = capture(&["worktree", "list", "--porcelain"]).context("listing worktrees")?;
    let worktrees = parse_worktrees(&out);
    Ok(mark_current(worktrees, Some(here), |path| {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }))
}

/// Refresh remote-tracking refs, dropping ones deleted upstream.
///
/// The one place cid silences git on purpose: `git fetch --all` narrates
/// itself onto the stdout `branch sel` writes a branch name to, and over the
/// spinner on stderr. A failure still speaks, as [`capture`] returns git's
/// stderr as the error.
pub fn fetch() -> Result<()> {
    capture(&["fetch", "--all", "--prune"])
        .context("fetching from remotes")
        .map(|_| ())
}

/// Perform a resolved checkout. `switch --track origin/foo` creates local `foo`
/// with its upstream already set.
pub fn checkout(action: &Checkout) -> Result<()> {
    match action {
        Checkout::Switch(name) => passthrough(&["switch", "--", name]),
        Checkout::Track { remote_ref } => passthrough(&["switch", "--track", remote_ref]),
    }
}

/// Create a working tree at `path` from `source`.
///
/// git creates the parent directories itself, so there is nothing to prepare,
/// and everything it says lands on stderr — see [`passthrough_onto_stderr`].
pub fn add_worktree(path: &Path, source: &TreeSource) -> Result<()> {
    let path = path.to_string_lossy();
    match source {
        TreeSource::Branch(name) => passthrough_onto_stderr(&["worktree", "add", &path, name]),
        TreeSource::New(name) => passthrough_onto_stderr(&["worktree", "add", "-b", name, &path]),
        TreeSource::Track { remote_ref, branch } => passthrough_onto_stderr(&[
            "worktree", "add", "--track", "-b", branch, &path, remote_ref,
        ]),
    }
}

/// Create a working tree at `path` with nothing checked out but the current
/// commit, for a caller that puts a branch in it afterwards. Narrates onto
/// stderr, as [`add_worktree`] does.
pub fn add_detached_worktree(path: &Path) -> Result<()> {
    let path = path.to_string_lossy();
    passthrough_onto_stderr(&["worktree", "add", "--detach", &path])
}

/// Whether git already ignores `path`, by any of the rules it consults.
fn is_ignored(path: &Path) -> bool {
    let _child = stats::in_child();
    Command::new("git")
        .args(["check-ignore", "--quiet", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Have this clone ignore `pattern`, if nothing already does.
///
/// A directory of working trees inside the repository is untracked files as far
/// as everything else is concerned: `git status` lists them, and every walker
/// that honours `.gitignore` — `cid edit`'s included — offers the whole tree a
/// second time under it. The rule belongs to this clone rather than to the
/// project, so it goes in `info/exclude` and is never committed.
///
/// Returns whether a line was written. A failure is not one: the tree is
/// already there and usable, and being untidy about it is not worth undoing it.
pub fn ignore_locally(root: &Path, pattern: &str) -> Result<bool> {
    if is_ignored(&root.join(pattern)) {
        return Ok(false);
    }

    // The *common* directory: a linked worktree's own `.git` is a file
    // pointing into the main one, and that is where `info/exclude` lives.
    let common = capture(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .context("locating the git directory")?;
    let exclude = Path::new(common.trim()).join("info").join("exclude");

    let mut existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(&format!("{pattern}/\n"));

    if let Some(dir) = exclude.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&exclude, existing).with_context(|| format!("writing {}", exclude.display()))?;
    Ok(true)
}

/// Remove the working tree at `path`. `force` is git's own, and is what a tree
/// with uncommitted changes in it needs.
pub fn remove_worktree(path: &Path, force: bool) -> Result<()> {
    let path = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    passthrough(&args)
}

/// The local branches already contained in the current HEAD, which are the ones
/// git will delete without being forced.
///
/// A squash merge produces no such containment — the branch's commits never
/// become ancestors of what merged them — so an empty answer means "git cannot
/// tell", not "nothing has landed".
pub fn merged_branches() -> Result<HashSet<String>> {
    // `HEAD` spelled out: `--merged` takes an optional commit, so the format
    // argument after a bare one is read as the commit to compare against.
    let out = capture(&["branch", "--merged", "HEAD", "--format=%(refname:short)"])
        .context("listing merged branches")?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// Delete a local branch. `force` is git's `-D`: it deletes a branch whose
/// commits are in no other, which `-d` refuses.
///
/// Captured rather than passed through, so a refusal is one line the caller can
/// report against the branch it belongs to — several branches are deleted in
/// one run, and git's own wording says which only by quoting the name.
pub fn delete_branch(name: &str, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    capture(&["branch", flag, "--", name])
        .map(|_| ())
        .map_err(|err| anyhow!("{}", unprefixed(&format!("{err:#}"))))
}

/// git's own `error:` or `fatal:` opener, removed. The caller is reporting the
/// line inside one of its own, and two of the word in a row says nothing twice.
fn unprefixed(message: &str) -> &str {
    let message = message.trim();
    ["error: ", "fatal: "]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .unwrap_or(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(refname: &str, head: bool, upstream: &str) -> RefLine {
        RefLine {
            refname: refname.to_string(),
            head,
            symref: String::new(),
            upstream: upstream.to_string(),
            date: "2 days ago".to_string(),
            subject: "some commit".to_string(),
        }
    }

    fn render(rows: &[(&str, &str, &str, &str, &str, &str)]) -> String {
        rows.iter()
            .map(|(r, h, y, u, d, s)| format!("{r}{SEP}{h}{SEP}{y}{SEP}{u}{SEP}{d}{SEP}{s}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parses_ref_rows() {
        let out = render(&[
            (
                "refs/heads/main",
                "*",
                "",
                "origin/main",
                "2 hours ago",
                "init",
            ),
            (
                "refs/remotes/origin/HEAD",
                " ",
                "origin/main",
                "",
                "2 hours ago",
                "init",
            ),
            (
                "refs/remotes/origin/main",
                " ",
                "",
                "",
                "2 hours ago",
                "init",
            ),
        ]);
        let got = parse_ref_lines(&out);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].refname, "refs/heads/main");
        assert!(got[0].head);
        assert_eq!(got[0].symref, "");
        assert_eq!(got[0].upstream, "origin/main");
        assert_eq!(got[0].date, "2 hours ago");
        assert_eq!(got[1].symref, "origin/main");
        assert_eq!(got[2].refname, "refs/remotes/origin/main");
        assert!(!got[2].head);
    }

    #[test]
    fn subject_keeps_trailing_separators() {
        let out = format!("refs/heads/main{SEP} {SEP}{SEP}{SEP}now{SEP}fix: a{SEP}b");
        let got = parse_ref_lines(&out);
        assert_eq!(got[0].subject, format!("fix: a{SEP}b"));
    }

    #[test]
    fn skips_malformed_rows() {
        let out = format!("refs/heads/main{SEP}*\n\nrefs/heads/ok{SEP} {SEP}{SEP}{SEP}now{SEP}s");
        let got = parse_ref_lines(&out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].refname, "refs/heads/ok");
    }

    #[test]
    fn local_with_remote_counterpart_is_tracked() {
        let got = classify(&[
            line("refs/heads/main", true, "origin/main"),
            line("refs/remotes/origin/main", false, ""),
        ]);
        assert_eq!(got.len(), 1, "the remote ref must not add a second row");
        assert_eq!(got[0].kind, BranchKind::Tracked);
        assert!(got[0].head);
    }

    #[test]
    fn local_without_remote_is_local_only() {
        let got = classify(&[line("refs/heads/scratch", false, "")]);
        assert_eq!(got[0].kind, BranchKind::Local);
    }

    #[test]
    fn an_upstream_that_no_longer_exists_is_gone() {
        let got = classify(&[line("refs/heads/gone", false, "origin/gone")]);
        assert_eq!(got[0].kind, BranchKind::Gone);
    }

    /// git's `[gone]` is about the ref the branch tracks, not about whether a
    /// branch of the same name exists somewhere else.
    #[test]
    fn a_namesake_on_another_remote_does_not_bring_a_gone_upstream_back() {
        let got = classify(&[
            line("refs/heads/feat", false, "origin/feat"),
            line("refs/remotes/fork/feat", false, ""),
        ]);
        assert_eq!(got[0].kind, BranchKind::Gone);
    }

    #[test]
    fn a_local_upstream_that_exists_is_not_gone() {
        let got = classify(&[
            line("refs/heads/topic", false, "main"),
            line("refs/heads/main", true, ""),
        ]);
        assert_eq!(got[0].kind, BranchKind::Local);
    }

    #[test]
    fn a_gone_branch_is_local_to_the_filters() {
        assert!(Filter::Local.accepts(BranchKind::Gone));
        assert!(!Filter::Remote.accepts(BranchKind::Gone));
        assert!(Filter::All.accepts(BranchKind::Gone));
    }

    #[test]
    fn upstream_under_another_name_still_counts() {
        let got = classify(&[
            line("refs/heads/local-name", false, "origin/remote-name"),
            line("refs/remotes/origin/remote-name", false, ""),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, BranchKind::Tracked);
    }

    #[test]
    fn remote_only_keeps_its_remote_prefix() {
        let got = classify(&[line("refs/remotes/origin/feat/x", false, "")]);
        assert_eq!(got[0].kind, BranchKind::Remote);
        assert_eq!(got[0].name, "origin/feat/x");
        assert_eq!(
            resolve(&got, "origin/feat/x"),
            Checkout::Track {
                remote_ref: "origin/feat/x".to_string()
            }
        );
    }

    #[test]
    fn drops_remote_head_pointer() {
        let got = classify(&[
            line("refs/remotes/origin/HEAD", false, ""),
            line("refs/remotes/origin/main", false, ""),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "origin/main");
    }

    #[test]
    fn same_branch_on_two_remotes_lists_both() {
        let got = classify(&[
            line("refs/remotes/origin/shared", false, ""),
            line("refs/remotes/fork/shared", false, ""),
        ]);
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["origin/shared", "fork/shared"]
        );
    }

    #[test]
    fn preserves_input_order() {
        let got = classify(&[
            line("refs/heads/b", false, ""),
            line("refs/heads/a", false, ""),
        ]);
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn relevance_groups_current_then_local_then_remote() {
        let got = by_relevance(
            classify(&[
                line("refs/remotes/origin/hot", false, ""),
                line("refs/heads/scratch", false, ""),
                line("refs/heads/main", true, "origin/main"),
                line("refs/remotes/origin/main", false, ""),
            ]),
            None,
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "scratch", "origin/hot"]
        );
    }

    #[test]
    fn relevance_keeps_commit_order_inside_each_group() {
        let got = by_relevance(
            classify(&[
                line("refs/heads/newest", false, ""),
                line("refs/remotes/origin/newest-remote", false, ""),
                line("refs/heads/older", false, ""),
                line("refs/remotes/origin/oldest-remote", false, ""),
            ]),
            None,
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec![
                "newest",
                "older",
                "origin/newest-remote",
                "origin/oldest-remote"
            ]
        );
    }

    #[test]
    fn relevance_handles_no_current_branch() {
        let got = by_relevance(
            classify(&[
                line("refs/remotes/origin/feature", false, ""),
                line("refs/heads/scratch", false, ""),
            ]),
            None,
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["scratch", "origin/feature"]
        );
    }

    /// The list is read on the way off a feature branch, so the branch being
    /// left is not the row the cursor opens on.
    #[test]
    fn relevance_leads_with_the_default_branch_when_it_is_not_checked_out() {
        let got = by_relevance(
            classify(&[
                line("refs/heads/feat/api", true, "origin/feat/api"),
                line("refs/remotes/origin/feat/api", false, ""),
                line("refs/heads/scratch", false, ""),
                line("refs/heads/main", false, "origin/main"),
                line("refs/remotes/origin/main", false, ""),
                line("refs/remotes/origin/hot", false, ""),
            ]),
            Some("main"),
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "feat/api", "scratch", "origin/hot"]
        );
    }

    #[test]
    fn relevance_leaves_the_order_alone_on_the_default_branch() {
        let got = by_relevance(
            classify(&[
                line("refs/heads/main", true, "origin/main"),
                line("refs/remotes/origin/main", false, ""),
                line("refs/heads/scratch", false, ""),
            ]),
            Some("main"),
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "scratch"]
        );
    }

    /// A clone that was never on the default branch has it only as a remote
    /// ref, which is still the row to lead with.
    #[test]
    fn relevance_leads_with_a_default_branch_that_is_only_on_the_remote() {
        let got = by_relevance(
            classify(&[
                line("refs/heads/feat/api", true, ""),
                line("refs/remotes/origin/main", false, ""),
            ]),
            Some("main"),
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["origin/main", "feat/api"]
        );
    }

    /// `feature/main` shares its last component with the default branch and is
    /// not it.
    #[test]
    fn relevance_matches_the_whole_name_of_a_local_branch() {
        let got = by_relevance(
            classify(&[
                line("refs/heads/feature/main", false, ""),
                line("refs/heads/main", false, ""),
            ]),
            Some("main"),
        );
        assert_eq!(
            got.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "feature/main"]
        );
    }

    #[test]
    fn the_default_branch_comes_from_the_remote_head_ref() {
        let mut head = line("refs/remotes/origin/HEAD", false, "");
        head.symref = "origin/trunk".to_string();
        let lines = vec![
            head,
            line("refs/heads/main", false, ""),
            line("refs/remotes/origin/trunk", false, ""),
        ];
        assert_eq!(default_branch(&lines).as_deref(), Some("trunk"));
    }

    /// `git init` and a push leave no `origin/HEAD` to read.
    #[test]
    fn the_default_branch_falls_back_to_the_name() {
        let lines = vec![
            line("refs/heads/feat/api", true, ""),
            line("refs/heads/master", false, ""),
        ];
        assert_eq!(default_branch(&lines).as_deref(), Some("master"));

        let lines = vec![
            line("refs/heads/feat/api", true, ""),
            line("refs/remotes/origin/main", false, ""),
            line("refs/heads/master", false, ""),
        ];
        assert_eq!(
            default_branch(&lines).as_deref(),
            Some("main"),
            "master won over main"
        );
    }

    #[test]
    fn a_repository_with_no_recognisable_default_branch_has_none() {
        let lines = vec![
            line("refs/heads/feat/api", true, ""),
            line("refs/heads/scratch", false, ""),
        ];
        assert_eq!(default_branch(&lines), None);
    }

    fn sample() -> Vec<Branch> {
        classify(&[
            line("refs/heads/main", true, "origin/main"),
            line("refs/remotes/origin/main", false, ""),
            line("refs/heads/scratch", false, ""),
            line("refs/remotes/origin/feature", false, ""),
        ])
    }

    #[test]
    fn filters_by_side() {
        let names = |f| {
            filtered(sample(), f)
                .into_iter()
                .map(|b| b.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Filter::All), ["main", "scratch", "origin/feature"]);
        assert_eq!(names(Filter::Local), ["main", "scratch"]);
        assert_eq!(names(Filter::Remote), ["main", "origin/feature"]);
    }

    #[test]
    fn filter_flags_default_to_all() {
        assert_eq!(Filter::from_flags(false, false), Filter::All);
        assert_eq!(Filter::from_flags(true, false), Filter::Local);
        assert_eq!(Filter::from_flags(false, true), Filter::Remote);
    }

    #[test]
    fn local_branch_is_switched_to() {
        assert_eq!(
            resolve(&sample(), "main"),
            Checkout::Switch("main".to_string())
        );
    }

    #[test]
    fn remote_only_branch_creates_a_tracking_branch() {
        assert_eq!(
            resolve(&sample(), "origin/feature"),
            Checkout::Track {
                remote_ref: "origin/feature".to_string()
            }
        );
    }

    #[test]
    fn remote_form_of_a_local_branch_switches_locally() {
        assert_eq!(
            resolve(&sample(), "origin/main"),
            Checkout::Switch("main".to_string())
        );
    }

    #[test]
    fn unknown_name_is_left_to_git() {
        assert_eq!(
            resolve(&sample(), "nope"),
            Checkout::Switch("nope".to_string())
        );
    }

    #[test]
    fn gits_own_error_opener_is_not_repeated_inside_the_callers() {
        assert_eq!(
            unprefixed("error: cannot delete branch 'x' used by worktree at '/y'"),
            "cannot delete branch 'x' used by worktree at '/y'"
        );
        assert_eq!(unprefixed("fatal: not a valid ref"), "not a valid ref");
        assert_eq!(unprefixed("  plain trouble  "), "plain trouble");
    }

    /// A tree is where work starts, and its branch usually does not exist yet —
    /// the one place an unknown name means "create it" rather than "ask git".
    #[test]
    fn an_unknown_name_is_a_branch_to_create() {
        assert_eq!(
            tree_source(&sample(), "feat/new"),
            TreeSource::New("feat/new".to_string())
        );
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        assert_eq!(
            tree_source(&sample(), "scratch"),
            TreeSource::Branch("scratch".to_string())
        );
    }

    #[test]
    fn a_remote_only_branch_arrives_tracking_it() {
        assert_eq!(
            tree_source(&sample(), "origin/feature"),
            TreeSource::Track {
                remote_ref: "origin/feature".to_string(),
                branch: "feature".to_string(),
            }
        );
        // The remote name is not part of the local branch, whichever form the
        // tree was asked for by.
        assert_eq!(tree_source(&sample(), "origin/feature").branch(), "feature");
    }

    /// `git switch feature` would create the tracking branch by itself.
    /// `git worktree add -b feature` would not: it would build a new branch
    /// from HEAD sharing a name with the remote one and nothing else.
    #[test]
    fn a_bare_name_only_a_remote_has_still_tracks_it() {
        assert_eq!(
            tree_source(&sample(), "feature"),
            TreeSource::Track {
                remote_ref: "origin/feature".to_string(),
                branch: "feature".to_string(),
            }
        );
    }

    /// git refuses to guess between two remotes, and neither does this.
    #[test]
    fn a_name_two_remotes_share_is_left_as_a_branch_to_create() {
        let mut branches = sample();
        branches.push(Branch {
            name: "fork/feature".to_string(),
            kind: BranchKind::Remote,
            head: false,
            date: "1 day ago".to_string(),
            subject: "theirs".to_string(),
        });
        assert_eq!(
            tree_source(&branches, "feature"),
            TreeSource::New("feature".to_string())
        );
    }

    /// Exactly what `git worktree list --porcelain` writes: a main tree, a
    /// linked one, a detached one, a locked one and a tree whose directory has
    /// been deleted.
    const WORKTREE_LIST: &str = "\
worktree /home/u/dev/cid
HEAD 950547ef3af47b2e60406bd23e530bdb1e226c6e
branch refs/heads/main

worktree /home/u/dev/cid/.claude/worktrees/feat
HEAD 32bb788aa1c04d9ee4d1e5a8b0e0b8d1c2f3a4b5
branch refs/heads/feat/x

worktree /home/u/dev/cid/.claude/worktrees/spike
HEAD cd7eff2bbb1c04d9ee4d1e5a8b0e0b8d1c2f3a4b
detached

worktree /home/u/dev/cid/.claude/worktrees/held
HEAD ec4b8dfccc1c04d9ee4d1e5a8b0e0b8d1c2f3a4b
branch refs/heads/held
locked waiting on review

worktree /home/u/dev/cid/.claude/worktrees/gone
HEAD 88a2dedddd1c04d9ee4d1e5a8b0e0b8d1c2f3a4b
branch refs/heads/gone
prunable gitdir file points to non-existent location
";

    #[test]
    fn parses_every_worktree_record() {
        let got = parse_worktrees(WORKTREE_LIST);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].path, PathBuf::from("/home/u/dev/cid"));
        assert_eq!(got[0].branch, "main", "the ref prefix survived");
        assert_eq!(got[1].branch, "feat/x", "a branch name may contain a slash");
        assert!(got[2].branch.is_empty(), "a detached HEAD has no branch");
        assert!(got[3].locked);
        assert!(got[4].prunable);
    }

    /// A reason on `locked` or `prunable` is words, not a value to be parsed.
    #[test]
    fn a_state_with_a_reason_is_still_that_state() {
        let got = parse_worktrees(WORKTREE_LIST);
        assert_eq!(got[3].tags(), vec!["locked"]);
        assert_eq!(got[4].tags(), vec!["prunable"]);
        assert!(got[0].tags().is_empty(), "an ordinary tree carries no tag");
    }

    #[test]
    fn a_bare_repository_has_no_head_to_report() {
        let got = parse_worktrees("worktree /home/u/dev/mirror.git\nbare\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].bare);
        assert_eq!(got[0].head_label(), "(bare)");
    }

    /// The column reads as a branch name only when it is one.
    #[test]
    fn a_detached_head_shows_the_commit_it_sits_on() {
        let got = parse_worktrees(WORKTREE_LIST);
        assert_eq!(got[0].head_label(), "main");
        assert_eq!(got[2].head_label(), "(cd7eff2)");
    }

    #[test]
    fn attributes_belong_to_the_record_that_opened() {
        // No blank line between the records, and an attribute git might add
        // after the ones known here.
        let got = parse_worktrees(
            "worktree /a\nHEAD abc\nbranch refs/heads/a\nsomething-new value\nworktree /b\nHEAD def\ndetached\n",
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].branch, "a");
        assert_eq!(got[1].path, PathBuf::from("/b"));
        assert!(got[1].branch.is_empty());
    }

    #[test]
    fn nothing_is_parsed_from_nothing() {
        assert!(parse_worktrees("").is_empty());
        // Attributes with no record to attach to are dropped, not panicked on.
        assert!(parse_worktrees("HEAD abc\nbranch refs/heads/a\n").is_empty());
    }

    #[test]
    fn the_current_tree_is_the_one_the_shell_is_in() {
        let here = Path::new("/home/u/dev/cid/.claude/worktrees/feat");
        let got = mark_current(
            parse_worktrees(WORKTREE_LIST),
            Some(here),
            Path::to_path_buf,
        );
        assert_eq!(
            got.iter().filter(|w| w.current).count(),
            1,
            "exactly one tree is the current one",
        );
        assert!(got[1].current);
        assert_eq!(got[1].color(), Some(2), "the current tree is green");
    }

    /// git prints the real path of a worktree; the working directory may have
    /// reached it through a symlink, as `/tmp` is on macOS.
    #[test]
    fn the_current_tree_is_found_through_a_symlink() {
        let got = mark_current(
            parse_worktrees("worktree /private/tmp/wt\nHEAD abc\nbranch refs/heads/main\n"),
            Some(Path::new("/tmp/wt")),
            |path| match path.strip_prefix("/tmp") {
                Ok(rest) => Path::new("/private/tmp").join(rest),
                Err(_) => path.to_path_buf(),
            },
        );
        assert!(got[0].current, "the symlinked path did not match");
        assert_eq!(
            got[0].path,
            PathBuf::from("/private/tmp/wt"),
            "resolving must not rewrite what is displayed",
        );
    }

    #[test]
    fn outside_a_repository_no_tree_is_current() {
        let got = mark_current(parse_worktrees(WORKTREE_LIST), None, Path::to_path_buf);
        assert!(got.iter().all(|w| !w.current));
    }

    /// A tree git cannot use is not one to switch to, whichever one the shell
    /// is standing in.
    #[test]
    fn an_unusable_tree_is_greyed_even_when_it_is_the_current_one() {
        let got = mark_current(
            parse_worktrees(WORKTREE_LIST),
            Some(Path::new("/home/u/dev/cid/.claude/worktrees/gone")),
            Path::to_path_buf,
        );
        assert_eq!(got[4].color(), Some(8));
        assert_eq!(got[2].color(), Some(3), "a detached tree is yellow");
        assert_eq!(got[0].color(), None, "an ordinary tree keeps the default");
    }
}
