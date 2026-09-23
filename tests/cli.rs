//! End-to-end tests of the `cid` binary.
//!
//! These cover what the unit tests cannot: that a flag reaches the function it
//! names, that an error leaves the right exit status behind, and that stdout
//! carries what a shell is expected to read.
//!
//! **Every run is sealed off from the machine it runs on.** `HOME`,
//! `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `PWD` point at a temporary directory
//! and the inherited environment is otherwise wiped.
//!
//! Nothing here opens a selector: a test that allocated a pty would be testing
//! skim. The commands exercised are the ones a script can call.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// The binary under test, as cargo built it for this run.
const BIN: &str = env!("CARGO_BIN_EXE_cid");

/// A sealed-off cid installation: its own home, config file and known-files
/// list, none of which outlive the test.
struct Sandbox {
    home: TempDir,
}

/// What one run of the binary came back with.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn ok(&self) -> &Self {
        assert_eq!(
            self.code,
            Some(0),
            "expected success\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    fn code(&self, expected: i32) -> &Self {
        assert_eq!(
            self.code,
            Some(expected),
            "wrong exit status\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
        self
    }

    /// The lines of stdout, with the trailing blank one dropped.
    fn lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temp home"),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn config_path(&self) -> PathBuf {
        self.home().join(".config/cid/config.toml")
    }

    /// The log every run appends itself to.
    fn stats_path(&self) -> PathBuf {
        self.home().join(".local/share/cid/stats")
    }

    /// The known-files list, which lives beside the config file.
    fn files_path(&self) -> PathBuf {
        self.home().join(".config/cid/files")
    }

    fn write_config(&self, toml: &str) -> PathBuf {
        let path = self.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml).unwrap();
        path
    }

    /// Run `cid` with `args`, from `cwd`. `env_clear` is the whole point: an
    /// inherited `CID_CONFIG` or `HOME` would change what is being tested.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> Run {
        self.run_full(cwd, args, &[])
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_full(self.home(), args, &[])
    }

    /// [`Sandbox::run`] with `env` set on top — for the variables cid is
    /// meant to react to, such as `CID_NO_COLOR`.
    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        self.run_full(self.home(), args, env)
    }

    /// [`Sandbox::run`] with `HOME` taken away — the one variable cid cannot
    /// resolve a configuration path without.
    fn run_without_home(&self, args: &[&str]) -> Run {
        let mut cmd = self.command(self.home(), args);
        cmd.env_remove("HOME");
        finish(cmd)
    }

    fn run_full(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = self.command(cwd, args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        finish(cmd)
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", self.home())
            .env("PWD", cwd)
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("XDG_DATA_HOME", self.home().join(".local/share"))
            // `gh`, `git` and the editor are looked up on PATH.
            .env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd
    }
}

fn finish(mut cmd: Command) -> Run {
    let out = cmd.output().expect("running cid");
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Create `<root>/<owner>/<name>` as something discovery will call a
/// repository.
fn mk_repo(root: &Path, owner: &str, name: &str) -> PathBuf {
    let path = root.join(owner).join(name);
    std::fs::create_dir_all(path.join(".git")).unwrap();
    path
}

// --- the argument surface ---------------------------------------------------

#[test]
fn help_lists_every_top_level_command() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["--help"]);
    run.ok();
    for command in [
        "repo", "file", "edit", "branch", "worktree", "pr", "ps", "history", "project", "config",
        "init",
    ] {
        assert!(
            run.stdout.contains(command),
            "`{command}` missing from --help:\n{}",
            run.stdout
        );
    }
    assert!(run.stdout.contains("Examples:"), "{}", run.stdout);
}

/// The shape is asserted rather than the exact string: both forms are correct
/// depending on where the build happened, and demanding one would fail in CI
/// or on a release runner.
#[test]
fn version_reports_the_crate_version_and_flags_a_development_build() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["--version"]);
    run.ok();

    let version = run
        .stdout
        .trim()
        .strip_prefix("cid ")
        .unwrap_or_else(|| panic!("unexpected --version output: {}", run.stdout));
    let crate_version = env!("CARGO_PKG_VERSION");

    let Some(dev) = version.strip_prefix(crate_version) else {
        panic!("`{version}` does not start with the crate version {crate_version}");
    };
    assert!(
        dev.is_empty() || dev.starts_with("-dev."),
        "`{version}` is neither the release version nor a development build",
    );
    if let Some(rest) = dev.strip_prefix("-dev.") {
        let sha = rest.strip_suffix(".dirty").unwrap_or(rest);
        assert!(
            !sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "`{version}` names no commit",
        );
    }
}

#[test]
fn an_unknown_command_is_a_usage_error() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["reop", "ls"]);
    run.code(2);
    assert!(run.stdout.is_empty(), "usage errors belong on stderr");
}

#[test]
fn help_names_the_aliases_the_binary_accepts() {
    let sandbox = Sandbox::new();
    let top = sandbox.run(&["--help"]);
    top.ok();
    for alias in ["r", "f", "e", "b", "w", "h", "c"] {
        assert!(
            top.stdout.contains(&format!("[aliases: {alias}]")),
            "`{alias}` is accepted but unmentioned:\n{}",
            top.stdout
        );
    }

    let branch = sandbox.run(&["branch", "--help"]);
    branch.ok();
    assert!(
        branch.stdout.contains("[aliases: co, switch]"),
        "{}",
        branch.stdout
    );
}

#[test]
fn the_help_examples_stay_at_three() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["--help"]);
    run.ok();
    let examples = run
        .stdout
        .split_once("Examples:")
        .expect("no examples block")
        .1;
    let lines = examples.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines, 3, "the examples block has grown:{examples}");
}

#[test]
fn the_documented_abbreviations_resolve() {
    let sandbox = Sandbox::new();
    for args in [
        &["c", "--help"][..],
        &["r", "--help"][..],
        &["e", "--help"][..],
        &["b", "--help"][..],
        &["w", "--help"][..],
        &["f", "--help"][..],
        &["h", "--help"][..],
        &["n", "--help"][..],
        &["pj", "--help"][..],
        &["ps", "--help"][..],
        &["branch", "co", "--help"][..],
        &["repo", "list", "--help"][..],
    ] {
        sandbox.run(args).ok();
    }
}

/// `$EDITOR` is `echo` here, so what it was asked to open comes back on
/// stdout. The `--` is part of the contract: to vim, `-c` is an Ex command.
#[test]
fn edit_defaults_to_the_file_subcommand() {
    let sandbox = Sandbox::new();
    let bare = sandbox.run_with_env(&["edit", "notes.md"], &[("EDITOR", "echo")]);
    bare.ok();
    let explicit = sandbox.run_with_env(&["edit", "file", "notes.md"], &[("EDITOR", "echo")]);
    explicit.ok();
    assert_eq!(bare.stdout, explicit.stdout, "the two spellings disagree");
    assert_eq!(bare.stdout.trim(), "-- notes.md");
}

#[test]
fn edit_dir_opens_the_directories_it_is_given() {
    let sandbox = Sandbox::new();
    let run = sandbox.run_with_env(&["edit", "dir", "src", "tests"], &[("EDITOR", "echo")]);
    run.ok();
    assert_eq!(run.stdout.trim(), "-- src tests");
}

/// The branch's pull request is a different question from "which pull
/// request?", so the two spellings must not both be answerable at once.
#[test]
fn pr_open_current_and_a_number_are_different_questions() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["pr", "open", "--current", "42"]);
    run.code(2);
    assert!(run.stderr.contains("cannot be used with"), "{}", run.stderr);
}

/// `--state` and `--limit` narrow a list this flag never asks for.
#[test]
fn pr_open_current_refuses_the_flags_it_would_ignore() {
    let sandbox = Sandbox::new();
    for scope in [&["--state", "all"][..], &["--limit", "5"][..]] {
        let mut args = vec!["pr", "open", "--current"];
        args.extend_from_slice(scope);
        sandbox.run(&args).code(2);
    }
}

#[test]
fn pr_open_current_outside_a_repository_says_so() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["pr", "open", "--current"]);
    run.code(1);
    assert!(
        run.stderr.contains("not inside a git repository"),
        "{}",
        run.stderr
    );
}

/// Every `pr` verb needs a repository to be about, and left to `gh` the answer
/// is git's `fatal: not a git repository` — a sentence about git, from a
/// command that was asked about pull requests.
#[test]
fn pr_outside_a_repository_says_so_before_gh_does() {
    let sandbox = Sandbox::new();
    for verb in [&["ls"][..], &["sel"][..], &["checkout", "1"][..]] {
        let mut args = vec!["pr"];
        args.extend_from_slice(verb);
        let run = sandbox.run(&args);
        run.code(1);
        assert!(
            run.stderr.contains("not inside a git repository"),
            "`pr {}`: {}",
            verb.join(" "),
            run.stderr
        );
        assert!(
            run.stderr.contains("GH_REPO"),
            "the way to run this anyway went unmentioned: {}",
            run.stderr
        );
        assert!(
            !run.stderr.contains("fatal:"),
            "gh was reached after all: {}",
            run.stderr
        );
    }
}

#[test]
fn every_registry_exposes_sel() {
    let sandbox = Sandbox::new();
    for group in [
        "repo", "file", "note", "branch", "worktree", "pr", "ps", "history",
    ] {
        sandbox.run(&[group, "sel", "--help"]).ok();
    }
}

/// The run gets no terminal, so it fails at the selector rather than at
/// parsing: "needs a terminal" is the proof the argument was accepted.
#[test]
fn a_history_query_may_begin_with_a_dash() {
    let sandbox = Sandbox::new();
    let history = sandbox.home().join(".local/share/fish/fish_history");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, "- cmd: git status\n  when: 1700000000\n").unwrap();

    let run = sandbox.run(&["history", "sel", "--query", "--version"]);
    assert!(
        !run.stderr.contains("a value is required"),
        "the query was read as a flag: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("needs a terminal"),
        "expected to get as far as the selector: {}",
        run.stderr
    );
}

#[test]
fn repo_clone_refuses_a_slug_github_could_not_have_issued() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&format!(
        "[repo]\nroot = {:?}\n",
        sandbox.home().join("dev").to_str().unwrap()
    ));
    for slug in ["../../etc/passwd", "acme/../../etc"] {
        let run = sandbox.run(&["repo", "clone", slug]);
        run.code(1);
        assert!(
            run.stderr.contains("not a GitHub owner"),
            "{slug} was accepted: {}",
            run.stderr
        );
    }
}

// --- config -----------------------------------------------------------------

#[test]
fn config_init_writes_a_config_the_next_command_accepts() {
    let sandbox = Sandbox::new();
    let init = sandbox.run(&["config", "init"]);
    init.ok();
    assert!(sandbox.config_path().exists(), "{}", init.stdout);

    let print = sandbox.run(&["config", "print"]);
    print.ok();
    assert!(print.stdout.contains("[repo]"), "{}", print.stdout);
    assert!(
        print.stdout.contains("~/dev/github.com"),
        "the root the starter config sets is not in the report: {}",
        print.stdout
    );
}

/// The report is what the settings actually are, so every table the file can
/// hold is in it whether the file writes that table or not.
#[test]
fn config_print_covers_every_table_and_the_keys_a_shell_binds() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        "[repo]\nroot = \"~/dev/github.com\"\n\n[shell.bindings]\nctrl-o = \"repo-cd\"\n",
    );

    let run = sandbox.run(&["config", "print"]);
    run.ok();
    for table in ["[repo]", "[worktree]", "[note]", "[history]", "[selector]"] {
        assert!(run.stdout.contains(table), "no {table} in:\n{}", run.stdout);
    }
    // The keys a shell binds are configuration, and this is where they are read
    // back: the key, what it runs, and what that does.
    assert!(run.stdout.contains("ctrl-o"), "{}", run.stdout);
    assert!(run.stdout.contains("repo-cd"), "{}", run.stdout);
    assert!(
        run.stdout.contains("Select a repository and cd into it"),
        "{}",
        run.stdout
    );
}

/// A config with no `[shell]` section binds nothing, and the report says so
/// rather than listing keys nobody asked for.
#[test]
fn config_print_says_when_no_key_is_bound() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/dev/github.com\"\n");

    let run = sandbox.run(&["config", "print"]);
    run.ok();
    assert!(run.stdout.contains("nothing is bound"), "{}", run.stdout);
    assert!(!run.stdout.contains("ctrl-o"), "{}", run.stdout);
}

/// Every action is listed, bound or not, with the keys and names the config
/// gives it.
#[test]
fn config_actions_lists_every_action_with_what_is_bound_to_it() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        "[shell.bindings]\nctrl-o = \"repo-cd\"\n\n[shell.aliases]\nkl = \"proc-kill\"\n",
    );

    let run = sandbox.run(&["config", "actions"]);
    run.ok();
    let columns = |id: &str| -> Vec<String> {
        run.lines()
            .into_iter()
            .find(|line| line.split_whitespace().next() == Some(id))
            .unwrap_or_else(|| panic!("no {id} row in:\n{}", run.stdout))
            .split_whitespace()
            .take(3)
            .map(str::to_string)
            .collect()
    };
    assert_eq!(columns("repo-cd"), ["repo-cd", "ctrl-o", "-"]);
    assert_eq!(columns("proc-kill"), ["proc-kill", "-", "kl"]);
    assert_eq!(columns("worktree-cd"), ["worktree-cd", "-", "-"]);
    assert!(run.stdout.contains("cid ps kill --force"), "{}", run.stdout);
}

#[test]
fn config_init_refuses_to_clobber_without_force() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/mine\"\n");

    let again = sandbox.run(&["config", "init"]);
    again.code(1);
    assert!(again.stderr.contains("already exists"), "{}", again.stderr);
    // Untouched.
    let kept = std::fs::read_to_string(sandbox.config_path()).unwrap();
    assert!(kept.contains("~/mine"), "the edited config was overwritten");

    sandbox.run(&["config", "init", "--force"]).ok();
    let replaced = std::fs::read_to_string(sandbox.config_path()).unwrap();
    assert!(!replaced.contains("~/mine"), "--force did not overwrite");
}

#[test]
fn config_path_prints_one_bare_path() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["config", "path"]);
    run.ok();
    assert_eq!(run.lines(), vec![sandbox.config_path().to_str().unwrap()]);
}

/// Every path cid resolves hangs off the home directory, and `$HOME` is the
/// only place it is read from. Without it the run has to stop and say so,
/// rather than carry on against a guess from the passwd file.
#[test]
fn no_home_fails_by_name() {
    let sandbox = Sandbox::new();
    let run = sandbox.run_without_home(&["config", "path"]);
    run.code(1);
    assert!(run.stderr.contains("HOME"), "{}", run.stderr);
}

#[test]
fn the_config_flag_points_at_another_file() {
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.home().join("elsewhere.toml");
    std::fs::write(&elsewhere, "[repo]\nroot = \"~/somewhere-else\"\n").unwrap();

    let run = sandbox.run(&["--config", elsewhere.to_str().unwrap(), "config", "print"]);
    run.ok();
    assert!(run.stdout.contains("somewhere-else"), "{}", run.stdout);
}

#[test]
fn a_legacy_config_fails_with_the_replacement_written_out() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[[paths]]\npath = \"~/dev/github.com/acme\"\ndepth = 1\n");

    let run = sandbox.run(&["config", "print"]);
    run.code(1);
    assert!(run.stderr.contains("old `paths` format"), "{}", run.stderr);
    assert!(run.stderr.contains("[repo]"), "{}", run.stderr);
    assert!(
        run.stderr.contains("root = \"~/dev/github.com\""),
        "{}",
        run.stderr
    );
}

#[test]
fn a_broken_config_names_the_file() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_config("[repo\nroot =");

    let run = sandbox.run(&["config", "print"]);
    run.code(1);
    assert!(
        run.stderr.contains(path.to_str().unwrap()),
        "the error did not say which file: {}",
        run.stderr
    );
}

#[test]
fn config_check_passes_on_a_sound_setup() {
    let sandbox = Sandbox::new();
    mk_repo(&sandbox.home().join("dev/github.com"), "acme", "billing");
    sandbox.write_config("[repo]\nroot = \"~/dev/github.com\"\n");

    let run = sandbox.run(&["config", "check"]);
    run.ok();
    assert!(run.stdout.contains("repo root"), "{}", run.stdout);
    // Discovery is really run: the count is what answers "is my root right".
    assert!(run.stdout.contains("1 found"), "{}", run.stdout);
    assert!(!run.stdout.contains('✗'), "{}", run.stdout);
}

#[test]
fn config_check_fails_on_a_missing_root() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/not/here\"\n");

    let run = sandbox.run(&["config", "check"]);
    run.code(1);
    assert!(
        run.stdout
            .lines()
            .any(|l| l.trim_start().starts_with('✗') && l.contains("repo root")),
        "{}",
        run.stdout
    );
    assert!(run.stderr.contains("checks failed"), "{}", run.stderr);
    assert!(!run.stdout.contains('\x1b'), "colour through a pipe");
}

/// `config check` and `--color` were written in parallel, each green on its
/// own branch, and the merge did not compile. Nothing tied the two together.
#[test]
fn config_check_honours_the_color_flag() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/not/here\"\n");

    let forced = sandbox.run(&["--color", "always", "config", "check"]);
    forced.code(1);
    assert!(
        forced.stdout.contains('\x1b'),
        "the report ignored --color always: {:?}",
        forced.stdout
    );
}

#[test]
fn config_check_does_not_report_one_problem_twice() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/not/here\"\n");

    let run = sandbox.run(&["config", "check"]);
    run.code(1);
    assert_eq!(
        run.stdout
            .lines()
            .filter(|l| l.trim_start().starts_with('✗'))
            .count(),
        1,
        "{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("repositories"),
        "the repository count repeated the root failure: {}",
        run.stdout
    );
}

// --- repo discovery ---------------------------------------------------------

/// The two ways to have nowhere to look need opposite advice. Someone who has
/// just run `config init` and not yet filled in the root must not be sent back
/// to `config init`, which refuses to overwrite the file it just wrote.
#[test]
fn a_missing_root_is_answered_by_where_the_user_actually_is() {
    let sandbox = Sandbox::new();

    let fresh = sandbox.run(&["repo", "ls"]);
    fresh.code(1);
    assert!(fresh.stderr.contains("cid config init"), "{}", fresh.stderr);

    let path = sandbox.write_config("[repo]\nignore = [\"target\"]\n");
    let written = sandbox.run(&["repo", "ls"]);
    written.code(1);
    assert!(
        written.stderr.contains(path.to_str().unwrap()),
        "the error did not say which file to edit: {}",
        written.stderr
    );
    assert!(written.stderr.contains("[repo] root"), "{}", written.stderr);
    assert!(
        !written.stderr.contains("config init"),
        "sent back to a command that will refuse: {}",
        written.stderr
    );
}

#[test]
fn repo_ls_finds_repositories_under_the_root() {
    let sandbox = Sandbox::new();
    let root = sandbox.home().join("dev/github.com");
    mk_repo(&root, "acme", "billing");
    mk_repo(&root, "acme", "auth");
    mk_repo(&root, "me", "dotfiles");
    // Neither of these is a repository: no `.git`, and too deep respectively.
    std::fs::create_dir_all(root.join("acme/not-a-repo")).unwrap();
    mk_repo(&root, "acme/billing", "vendored");

    sandbox.write_config("[repo]\nroot = \"~/dev/github.com\"\n");
    let run = sandbox.run(&["repo", "ls"]);
    run.ok();

    assert_eq!(
        run.lines(),
        vec![
            "~/dev/github.com/acme/auth",
            "~/dev/github.com/acme/billing",
            "~/dev/github.com/me/dotfiles",
        ],
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn repo_ls_absolute_paths_are_absolute() {
    let sandbox = Sandbox::new();
    let root = sandbox.home().join("dev/github.com");
    mk_repo(&root, "acme", "billing");
    sandbox.write_config("[repo]\nroot = \"~/dev/github.com\"\n");

    let run = sandbox.run(&["repo", "ls", "-A"]);
    run.ok();
    assert_eq!(run.lines().len(), 1);
    assert!(
        Path::new(run.lines()[0]).is_absolute(),
        "{}",
        run.lines()[0]
    );
    assert!(!run.lines()[0].starts_with('~'));
}

#[test]
fn repo_ls_skips_ignored_directories() {
    let sandbox = Sandbox::new();
    let root = sandbox.home().join("dev/github.com");
    mk_repo(&root, "acme", "billing");
    mk_repo(&root, "node_modules", "vendored");
    sandbox.write_config("[repo]\nroot = \"~/dev/github.com\"\nignore = [\"node_modules\"]\n");

    let run = sandbox.run(&["repo", "ls"]);
    run.ok();
    assert_eq!(run.lines(), vec!["~/dev/github.com/acme/billing"]);
}

#[test]
fn repo_ls_reports_a_missing_root() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/not/here\"\n");

    let run = sandbox.run(&["repo", "ls"]);
    run.code(1);
    assert!(run.stderr.contains("not/here"), "{}", run.stderr);
}

// --- worktrees --------------------------------------------------------------

/// A real repository with one linked worktree, since `cid worktree` reads
/// git rather than the filesystem.
///
/// `git worktree add` needs a commit to point the new tree at, and the sandbox
/// has no git identity to make one with — hence the author and committer in
/// the environment, and the two `GIT_CONFIG_*` variables that keep the machine
/// running the test out of the result.
fn mk_worktree_repo(root: &Path) -> (PathBuf, PathBuf) {
    let main = root.join("cid");
    std::fs::create_dir_all(&main).unwrap();

    for args in [
        &["init", "-b", "main"][..],
        &["commit", "--allow-empty", "-m", "init"][..],
        &["worktree", "add", "-b", "feat", "../feat"][..],
    ] {
        git_in(&main, root, args);
    }

    (main, root.join("feat"))
}

/// Run git in `dir`, sealed off the way [`Sandbox`] seals cid off.
///
/// The sandbox has no git identity to commit with — hence the author and
/// committer in the environment, and the two `GIT_CONFIG_*` variables that keep
/// the machine running the test out of the result.
fn git_in(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "cid tests")
        .env("GIT_AUTHOR_EMAIL", "tests@example.invalid")
        .env("GIT_COMMITTER_NAME", "cid tests")
        .env("GIT_COMMITTER_EMAIL", "tests@example.invalid")
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// git reports the real path of a worktree, and a temporary directory on macOS
/// is reached through a symlink.
fn real(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

#[test]
fn worktree_ls_lists_the_main_tree_and_the_linked_one() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());

    let run = sandbox.run_in(&main, &["worktree", "ls", "--absolute-paths"]);
    run.ok();
    assert_eq!(run.lines(), vec![real(&main), real(&feat)]);
}

/// Which tree is current is the whole question the selector answers, and it is
/// the one the shell is standing in — not the one git happens to list first.
#[test]
fn worktree_ls_status_marks_the_tree_the_shell_is_in() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());

    let from_linked = sandbox.run_in(&feat, &["worktree", "ls", "--status", "-A"]);
    from_linked.ok();
    let lines = from_linked.lines();
    assert!(lines[0].starts_with("  main "), "{lines:?}");
    assert!(lines[1].starts_with("* feat "), "{lines:?}");
    assert!(lines[1].ends_with(&real(&feat)), "{lines:?}");

    let from_main = sandbox.run_in(&main, &["worktree", "ls", "--status", "-A"]);
    from_main.ok();
    assert!(
        from_main.lines()[0].starts_with("* main "),
        "{:?}",
        from_main.lines()
    );
}

/// `~` is for reading, `-A` is for piping. `HOME` is overridden with its
/// resolved form because git reports resolved paths, and a `~` is only
/// recognisable in one that shares its prefix.
#[test]
fn worktree_ls_collapses_home_unless_asked_for_absolute_paths() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());
    let home = sandbox.home().canonicalize().unwrap();
    let env = [("HOME", home.to_str().unwrap())];

    let collapsed = sandbox.run_full(&main, &["worktree", "ls"], &env);
    collapsed.ok();
    assert_eq!(collapsed.lines(), vec!["~/cid", "~/feat"]);

    let absolute = sandbox.run_full(&main, &["worktree", "ls", "-A"], &env);
    absolute.ok();
    assert_eq!(absolute.lines(), vec![real(&main), real(&feat)]);
}

#[test]
fn worktree_ls_outside_a_repository_says_so() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["worktree", "ls"]);
    run.code(1);
    assert!(
        run.stderr.contains("not inside a git repository"),
        "{}",
        run.stderr
    );
}

#[test]
fn worktree_add_creates_the_tree_beside_the_checkout_and_prints_its_path() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());

    let run = sandbox.run_in(&main, &["worktree", "add", "work/x"]);
    run.ok();

    let path = main.join(".worktrees/work-x");
    assert!(
        path.is_dir(),
        "no tree at {}: {}",
        path.display(),
        run.stderr
    );
    // The path is stdout and git's narration is not, so `cd (cid worktree
    // add …)` lands in the tree rather than in a sentence about it.
    assert_eq!(run.lines(), vec![real(&path)]);
    assert_eq!(
        git_in(
            &path,
            sandbox.home(),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        )
        .trim(),
        "work/x",
        "the tree is not on the branch it was asked for",
    );
}

/// Untracked trees inside the repository are files `git status` reports and
/// every `.gitignore`-honouring walker offers a second time.
#[test]
fn a_tree_inside_the_repository_is_excluded_from_it() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());

    let run = sandbox.run_in(&main, &["worktree", "add", "work/x"]);
    run.ok();
    assert!(run.stderr.contains("info/exclude"), "{}", run.stderr);

    let status = git_in(&main, sandbox.home(), &["status", "--short"]);
    assert!(
        status.trim().is_empty(),
        "the tree shows as untracked: {status}"
    );

    // Written once: a second tree finds the rule already there.
    let again = sandbox.run_in(&main, &["worktree", "add", "work/y"]);
    again.ok();
    let exclude = std::fs::read_to_string(main.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.matches(".worktrees/").count(), 1, "{exclude}");
}

#[test]
fn an_absolute_worktree_root_keeps_each_repository_apart() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    sandbox.write_config("[worktree]\nroot = \"~/trees\"\n");

    let run = sandbox.run_in(&main, &["worktree", "add", "work/x"]);
    run.ok();

    let path = sandbox.home().join("trees/cid/work-x");
    assert!(
        path.is_dir(),
        "no tree at {}: {}",
        path.display(),
        run.stderr
    );
    assert!(
        !run.stderr.contains("info/exclude"),
        "a tree outside the repository has nothing to hide from it: {}",
        run.stderr
    );
}

#[test]
fn worktree_add_refuses_a_path_that_is_already_a_tree() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    sandbox.run_in(&main, &["worktree", "add", "work/x"]).ok();

    let again = sandbox.run_in(&main, &["worktree", "add", "work/x"]);
    again.code(1);
    assert!(again.stderr.contains("already exists"), "{}", again.stderr);
}

// --- pull requests in worktrees ---------------------------------------------

/// A `gh` that knows two pull requests: #7 from `fix/login`, whose checkout
/// narrates on stdout the way the real one does, and #9, whose checkout fails.
/// Returns the directory to put first on `PATH`.
fn stub_gh(home: &Path) -> PathBuf {
    let bin = home.join("stub-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
case "$1 $2 $3" in
    "pr view 7") echo fix/login ;;
    "pr view 9") echo fix/broken ;;
    "pr checkout 7") git checkout -q -b fix/login && echo "Switched to branch 'fix/login'" ;;
    "pr checkout 9") echo "could not check out #9" >&2; exit 4 ;;
    *) echo "stub gh: unexpected: $*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// `PATH` with the stub `gh` ahead of everything else.
fn path_with(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn pr_checkout_worktree_puts_the_branch_in_a_tree_and_prints_only_its_path() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    let path = path_with(&stub_gh(sandbox.home()));

    let run = sandbox.run_full(
        &main,
        &["pr", "checkout", "7", "--worktree"],
        &[("PATH", &path)],
    );
    run.ok();

    let tree = main.join(".worktrees/fix-login");
    assert_eq!(run.lines(), vec![real(&tree)], "stderr: {}", run.stderr);
    assert!(run.stderr.contains("Switched to branch"), "{}", run.stderr);
    assert_eq!(
        git_in(
            &tree,
            sandbox.home(),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        )
        .trim(),
        "fix/login",
    );
    assert_eq!(
        git_in(
            &main,
            sandbox.home(),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        )
        .trim(),
        "main",
        "the tree the shell was in was checked out instead",
    );
}

/// git will not check one branch out in two trees, and the tree that already
/// has it is where the user was headed.
#[test]
fn pr_checkout_worktree_sends_you_to_the_tree_that_already_has_the_branch() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());
    git_in(&feat, sandbox.home(), &["switch", "-c", "fix/login"]);
    let path = path_with(&stub_gh(sandbox.home()));

    let run = sandbox.run_full(&main, &["pr", "checkout", "-w", "7"], &[("PATH", &path)]);
    run.ok();
    assert_eq!(run.lines(), vec![real(&feat)]);
    assert!(run.stderr.contains("already checked out"), "{}", run.stderr);
}

#[test]
fn a_failed_pr_checkout_leaves_no_tree_behind() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    let path = path_with(&stub_gh(sandbox.home()));

    let run = sandbox.run_full(
        &main,
        &["pr", "checkout", "9", "--worktree"],
        &[("PATH", &path)],
    );
    run.code(4);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
    let trees = git_in(&main, sandbox.home(), &["worktree", "list", "--porcelain"]);
    assert_eq!(trees.matches("worktree ").count(), 2, "{trees}");
    assert!(!main.join(".worktrees/fix-broken").exists());
}

#[test]
fn worktree_rm_removes_the_tree_it_is_given() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    sandbox.run_in(&main, &["worktree", "add", "work/x"]).ok();
    let path = main.join(".worktrees/work-x");

    let run = sandbox.run_in(&main, &["worktree", "rm", path.to_str().unwrap(), "--yes"]);
    run.ok();
    assert!(
        !path.exists(),
        "{} survived: {}",
        path.display(),
        run.stderr
    );
}

#[test]
fn worktree_rm_leaves_the_branch_unless_asked() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());

    sandbox
        .run_in(&main, &["worktree", "rm", feat.to_str().unwrap(), "--yes"])
        .ok();
    assert!(!feat.exists());
    assert_eq!(local_branches(&main, sandbox.home()), vec!["feat", "main"]);
}

#[test]
fn worktree_rm_branch_removes_the_tree_and_the_branch_it_had_checked_out() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());
    let tree = sandbox.home().join("done-tree");
    git_in(
        &main,
        sandbox.home(),
        &["worktree", "add", "-q", "../done-tree", "done"],
    );

    let run = sandbox.run_in(
        &main,
        &[
            "worktree",
            "rm",
            tree.to_str().unwrap(),
            "--branch",
            "--yes",
        ],
    );
    run.ok();
    let listed = run.stdout.find("done  upstream gone").expect(&run.stdout);
    let deleted = run.stdout.find("Deleted done").expect(&run.stdout);
    assert!(listed < deleted, "deleted before listing: {}", run.stdout);

    assert!(!tree.exists(), "the tree survived: {}", run.stderr);
    assert_eq!(local_branches(&main, sandbox.home()), vec!["main", "spike"]);
}

/// The question cannot be asked without a terminal, and a removal is not the
/// kind of thing to answer on the user's behalf.
#[test]
fn worktree_rm_without_a_terminal_names_the_flag_that_skips_the_question() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    sandbox.run_in(&main, &["worktree", "add", "work/x"]).ok();
    let path = main.join(".worktrees/work-x");

    let run = sandbox.run_in(&main, &["worktree", "rm", path.to_str().unwrap()]);
    run.code(1);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);
    assert!(path.exists(), "the tree went without being confirmed");
}

// --- branch listing ---------------------------------------------------------

/// The list is mostly read on the way off a feature branch, so the default
/// branch leads it — here without an `origin/HEAD` to read, since `git init`
/// writes none.
#[test]
fn branch_ls_leads_with_the_default_branch_from_a_feature_branch() {
    let sandbox = Sandbox::new();
    let (main, feat) = mk_worktree_repo(sandbox.home());
    git_in(&main, sandbox.home(), &["branch", "spike"]);

    let run = sandbox.run_in(&feat, &["branch", "ls"]);
    run.ok();
    let listed: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(listed, vec!["main", "feat", "spike"], "{listed:?}");
}

// --- branch deletion --------------------------------------------------------

#[test]
fn branch_rm_deletes_the_branches_it_is_given_and_says_what_had_landed() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    git_in(&main, sandbox.home(), &["branch", "landed"]);
    git_in(&main, sandbox.home(), &["branch", "other"]);

    let run = sandbox.run_in(&main, &["branch", "rm", "landed", "other", "--yes"]);
    run.ok();

    // Both point at HEAD, so git can see both have landed.
    assert!(run.stdout.contains("landed  merged"), "{}", run.stdout);
    assert!(run.stdout.contains("Deleted landed"), "{}", run.stdout);

    let left = git_in(
        &main,
        sandbox.home(),
        &["branch", "--format=%(refname:short)"],
    );
    let left: Vec<&str> = left.lines().collect();
    assert_eq!(left, vec!["feat", "main"], "{left:?}");
}

/// A branch git cannot see has landed is still deletable — a repository that
/// squashes its merges has no other kind — but the list says so first.
#[test]
fn an_unmerged_branch_is_marked_rather_than_refused() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    git_in(&main, sandbox.home(), &["checkout", "-b", "work"]);
    git_in(
        &main,
        sandbox.home(),
        &["commit", "--allow-empty", "-m", "wip"],
    );
    git_in(&main, sandbox.home(), &["checkout", "main"]);

    let run = sandbox.run_in(&main, &["branch", "rm", "work", "--yes"]);
    run.ok();
    assert!(run.stdout.contains("work  not merged"), "{}", run.stdout);
    assert!(run.stdout.contains("Deleted work"), "{}", run.stdout);
}

#[test]
fn branch_rm_without_a_terminal_names_the_flag_that_skips_the_question() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());
    git_in(&main, sandbox.home(), &["branch", "keep"]);

    let run = sandbox.run_in(&main, &["branch", "rm", "keep"]);
    run.code(1);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);

    let left = git_in(
        &main,
        sandbox.home(),
        &["branch", "--format=%(refname:short)"],
    );
    assert!(left.contains("keep"), "the branch went unconfirmed: {left}");
}

/// A clone of a bare `remote.git` holding `main`, `done` — pushed, then
/// deleted on the remote, the way merging its pull request deletes it — and
/// `spike`, which was never pushed. `done` is deleted through the clone, so
/// its remote-tracking ref is already gone and no fetch is needed to see it.
fn mk_repo_with_a_gone_branch(root: &Path) -> PathBuf {
    let home = root;
    git_in(
        root,
        home,
        &["init", "-q", "--bare", "-b", "main", "remote.git"],
    );
    let main = root.join("cid");
    std::fs::create_dir_all(&main).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
        &["remote", "add", "origin", "../remote.git"][..],
        &["push", "-q", "-u", "origin", "main"][..],
        &["checkout", "-q", "-b", "done"][..],
        &["commit", "-q", "--allow-empty", "-m", "finished work"][..],
        &["push", "-q", "-u", "origin", "done"][..],
        &["checkout", "-q", "-b", "spike", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "never pushed"][..],
        &["checkout", "-q", "main"][..],
        &["push", "-q", "origin", "--delete", "done"][..],
    ] {
        git_in(&main, root, args);
    }
    main
}

fn local_branches(repo: &Path, home: &Path) -> Vec<String> {
    git_in(repo, home, &["branch", "--format=%(refname:short)"])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn branch_ls_status_tags_a_branch_whose_remote_branch_is_gone() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());

    let run = sandbox.run_in(&main, &["branch", "ls", "--status"]);
    run.ok();
    let row = |name: &str| {
        run.lines()
            .into_iter()
            .find(|line| line.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("no row for {name}: {}", run.stdout))
            .to_string()
    };
    assert!(row("done").contains(" gone "), "{}", row("done"));
    assert!(row("spike").contains(" local "), "{}", row("spike"));

    let remote = sandbox.run_in(&main, &["branch", "ls", "--remote"]);
    remote.ok();
    assert!(!remote.lines().contains(&"done"), "{}", remote.stdout);
}

#[test]
fn branch_prune_deletes_the_gone_branches_and_nothing_else() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());

    let run = sandbox.run_in(&main, &["branch", "prune", "--yes"]);
    run.ok();
    let listed = run.stdout.find("done  upstream gone").expect(&run.stdout);
    let deleted = run.stdout.find("Deleted done").expect(&run.stdout);
    assert!(listed < deleted, "deleted before listing: {}", run.stdout);

    assert_eq!(local_branches(&main, sandbox.home()), vec!["main", "spike"]);
}

/// A branch is only seen to be gone once a fetch has pruned its tracking ref,
/// which is what `--fetch` is for when it was deleted from somewhere else.
#[test]
fn branch_prune_fetch_sees_a_branch_deleted_elsewhere() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());
    git_in(
        &main,
        sandbox.home(),
        &["push", "-q", "-u", "origin", "spike"],
    );
    git_in(
        &sandbox.home().join("remote.git"),
        sandbox.home(),
        &["branch", "-D", "spike"],
    );

    let before = sandbox.run_in(&main, &["branch", "prune", "--yes"]);
    before.ok();
    assert!(
        local_branches(&main, sandbox.home()).contains(&"spike".to_string()),
        "pruned without having fetched: {}",
        before.stdout
    );

    sandbox
        .run_in(&main, &["branch", "prune", "--fetch", "--yes"])
        .ok();
    assert_eq!(local_branches(&main, sandbox.home()), vec!["main"]);
}

#[test]
fn branch_prune_keeps_a_branch_a_worktree_has_checked_out() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());
    git_in(
        &main,
        sandbox.home(),
        &["worktree", "add", "-q", "../done-tree", "done"],
    );

    let run = sandbox.run_in(&main, &["branch", "prune", "--yes"]);
    run.ok();
    assert!(run.stderr.contains("keeping done"), "{}", run.stderr);
    assert!(run.stdout.contains("Nothing to prune"), "{}", run.stdout);
    assert!(local_branches(&main, sandbox.home()).contains(&"done".to_string()));
}

#[test]
fn branch_prune_without_a_terminal_names_the_flag_that_skips_the_question() {
    let sandbox = Sandbox::new();
    let main = mk_repo_with_a_gone_branch(sandbox.home());

    let run = sandbox.run_in(&main, &["branch", "prune"]);
    run.code(1);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);
    assert!(local_branches(&main, sandbox.home()).contains(&"done".to_string()));
}

#[test]
fn branch_prune_with_nothing_gone_points_at_fetch() {
    let sandbox = Sandbox::new();
    let (main, _) = mk_worktree_repo(sandbox.home());

    let run = sandbox.run_in(&main, &["branch", "prune"]);
    run.ok();
    assert!(run.stdout.contains("Nothing to prune"), "{}", run.stdout);
    assert!(run.stdout.contains("--fetch"), "{}", run.stdout);
}

// --- the known-files list ---------------------------------------------------

#[test]
fn file_add_ls_and_remove_round_trip() {
    let sandbox = Sandbox::new();
    let note = sandbox.home().join("notes.md");
    std::fs::write(&note, "hello").unwrap();

    sandbox.run(&["file", "add", note.to_str().unwrap()]).ok();
    // Stored `~`-collapsed, so the list survives a different machine.
    let stored = std::fs::read_to_string(sandbox.files_path()).unwrap();
    assert_eq!(stored.trim(), "~/notes.md");

    // Listed expanded, because a listing is for handing to another command.
    let ls = sandbox.run(&["file", "ls"]);
    ls.ok();
    assert_eq!(ls.lines(), vec![note.to_str().unwrap()]);

    // A newline would come back as two entries `file rm` can never match.
    let bad = sandbox.run(&["file", "add", "one\ntwo.md"]);
    bad.code(1);
    assert!(bad.stderr.contains("one path per line"), "{}", bad.stderr);
    assert_eq!(
        sandbox.run(&["file", "ls"]).ok().lines(),
        vec![note.to_str().unwrap()],
        "the rejected path reached the list anyway"
    );

    let removed = sandbox.run(&["file", "rm", note.to_str().unwrap()]);
    removed.ok();
    assert!(removed.stdout.contains("Removed"), "{}", removed.stdout);
    assert!(sandbox.run(&["file", "ls"]).ok().lines().is_empty());
}

#[test]
fn file_add_refuses_a_duplicate() {
    let sandbox = Sandbox::new();
    let note = sandbox.home().join("notes.md");
    std::fs::write(&note, "hello").unwrap();

    sandbox.run(&["file", "add", note.to_str().unwrap()]).ok();
    let again = sandbox.run(&["file", "add", note.to_str().unwrap()]);
    again.code(1);
    assert!(again.stderr.contains("already exists"), "{}", again.stderr);
}

#[test]
fn file_ls_separates_present_from_missing() {
    let sandbox = Sandbox::new();
    let here = sandbox.home().join("here.md");
    std::fs::write(&here, "").unwrap();
    let gone = sandbox.home().join("gone.md");

    sandbox.run(&["file", "add", here.to_str().unwrap()]).ok();
    // Adding something absent warns but records it: the file may come back.
    let added = sandbox.run(&["file", "add", gone.to_str().unwrap()]);
    added.ok();
    assert!(added.stderr.contains("does not exist"), "{}", added.stderr);

    let missing = sandbox.run(&["file", "ls", "--missing"]);
    missing.ok();
    assert_eq!(missing.lines(), vec![gone.to_str().unwrap()]);

    let exists = sandbox.run(&["file", "ls", "--exists"]);
    exists.ok();
    assert_eq!(exists.lines(), vec![here.to_str().unwrap()]);

    let status = sandbox.run(&["file", "ls", "--status"]);
    status.ok();
    assert!(status.stdout.contains("✓ "), "{}", status.stdout);
    assert!(status.stdout.contains("✗ "), "{}", status.stdout);
}

#[test]
fn file_prune_drops_the_entries_whose_files_are_gone() {
    let sandbox = Sandbox::new();
    let here = sandbox.home().join("here.md");
    std::fs::write(&here, "").unwrap();
    let gone = sandbox.home().join("gone.md");
    std::fs::write(&gone, "").unwrap();

    sandbox.run(&["file", "add", here.to_str().unwrap()]).ok();
    sandbox.run(&["file", "add", gone.to_str().unwrap()]).ok();
    std::fs::remove_file(&gone).unwrap();

    let run = sandbox.run(&["file", "prune", "--yes"]);
    run.ok();
    assert!(run.stdout.contains("Removed"), "{}", run.stdout);
    assert!(run.stdout.contains("gone.md"), "{}", run.stdout);

    let left = sandbox.run(&["file", "ls"]);
    left.ok();
    assert_eq!(left.lines(), vec![here.to_str().unwrap()]);
}

#[test]
fn file_prune_refuses_to_assume_an_answer_it_could_not_ask_for() {
    let sandbox = Sandbox::new();
    let gone = sandbox.home().join("gone.md");
    std::fs::write(&gone, "").unwrap();
    sandbox.run(&["file", "add", gone.to_str().unwrap()]).ok();
    std::fs::remove_file(&gone).unwrap();

    // The test harness gives the child no terminal, which is exactly the case.
    let run = sandbox.run(&["file", "prune"]);
    run.code(1);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);

    // And the list is untouched.
    assert_eq!(sandbox.run(&["file", "ls"]).ok().lines().len(), 1);
}

#[test]
fn file_prune_says_so_when_there_is_nothing_to_do() {
    let sandbox = Sandbox::new();
    let here = sandbox.home().join("here.md");
    std::fs::write(&here, "").unwrap();
    sandbox.run(&["file", "add", here.to_str().unwrap()]).ok();

    // No `--yes`, and no terminal: reaching the confirmation at all would fail.
    let run = sandbox.run(&["file", "prune"]);
    run.ok();
    assert!(run.stdout.contains("Nothing to prune"), "{}", run.stdout);
    assert_eq!(sandbox.run(&["file", "ls"]).ok().lines().len(), 1);
}

#[test]
fn file_prune_shows_the_entries_before_removing_them() {
    let sandbox = Sandbox::new();
    let gone = sandbox.home().join("gone.md");
    std::fs::write(&gone, "").unwrap();
    sandbox.run(&["file", "add", gone.to_str().unwrap()]).ok();
    std::fs::remove_file(&gone).unwrap();

    let run = sandbox.run(&["file", "prune", "--yes"]);
    run.ok();
    let shown = run.stdout.find("✗ ").expect("no listing of what would go");
    let removed = run
        .stdout
        .find("Removed")
        .expect("nothing reported removed");
    assert!(shown < removed, "listed the entries after removing them");
}

#[test]
fn file_add_resolves_a_relative_path_against_the_working_directory() {
    let sandbox = Sandbox::new();
    let project = sandbox.home().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.rs"), "").unwrap();

    sandbox.run_in(&project, &["file", "add", "main.rs"]).ok();
    let stored = std::fs::read_to_string(sandbox.files_path()).unwrap();
    assert_eq!(stored.trim(), "~/project/main.rs");
}

// --- colour -----------------------------------------------------------------

/// A sandbox with one present and one missing tracked file, so `file ls
/// --status` has both a green row and a red one to colour.
fn coloured_listing(sandbox: &Sandbox) {
    let here = sandbox.home().join("here.md");
    std::fs::write(&here, "").unwrap();
    sandbox.run(&["file", "add", here.to_str().unwrap()]).ok();
    sandbox
        .run(&[
            "file",
            "add",
            sandbox.home().join("gone.md").to_str().unwrap(),
        ])
        .ok();
}

#[test]
fn color_always_colours_a_pipe_and_never_does_not() {
    let sandbox = Sandbox::new();
    coloured_listing(&sandbox);

    let always = sandbox.run(&["--color", "always", "file", "ls", "--status"]);
    always.ok();
    assert!(
        always.stdout.contains('\x1b'),
        "no colour through a pipe: {:?}",
        always.stdout
    );

    for args in [
        &["--color", "never", "file", "ls", "--status"][..],
        // The default through a pipe: unchanged from before there was a flag.
        &["file", "ls", "--status"][..],
    ] {
        let run = sandbox.run(args);
        run.ok();
        assert!(
            !run.stdout.contains('\x1b'),
            "{args:?} coloured: {:?}",
            run.stdout
        );
        // The glyphs are shapes, so the listing says as much either way.
        assert!(run.stdout.contains("✓ "), "{:?}", run.stdout);
        assert!(run.stdout.contains("✗ "), "{:?}", run.stdout);
    }
}

#[test]
fn cid_no_color_turns_colour_off() {
    let sandbox = Sandbox::new();
    coloured_listing(&sandbox);

    let run = sandbox.run_with_env(
        &["--color", "auto", "file", "ls", "--status"],
        &[("CID_NO_COLOR", "1")],
    );
    run.ok();
    assert!(!run.stdout.contains('\x1b'), "{:?}", run.stdout);

    // Set but empty is not set, as every variable of this shape works.
    let empty = sandbox.run_with_env(
        &["--color", "always", "file", "ls", "--status"],
        &[("CID_NO_COLOR", "")],
    );
    empty.ok();
    assert!(empty.stdout.contains('\x1b'), "{:?}", empty.stdout);
}

#[test]
fn an_explicit_color_choice_outranks_no_color() {
    let sandbox = Sandbox::new();
    coloured_listing(&sandbox);

    let forced = sandbox.run_with_env(
        &["--color", "always", "file", "ls", "--status"],
        &[("CID_NO_COLOR", "1")],
    );
    forced.ok();
    assert!(
        forced.stdout.contains('\x1b'),
        "CID_NO_COLOR beat `--color always`: {:?}",
        forced.stdout
    );
}

#[test]
fn color_takes_only_the_three_conventional_values() {
    let sandbox = Sandbox::new();
    sandbox.run(&["--color", "sometimes", "file", "ls"]).code(2);
}

// --- processes --------------------------------------------------------------

#[test]
fn ps_ls_leads_every_row_with_a_pid() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["ps", "ls"]);
    run.ok();
    let lines = run.lines();
    assert!(!lines.is_empty(), "listed no processes at all");
    for line in &lines {
        let (pid, command) = line.split_once(' ').unwrap_or_else(|| panic!("{line:?}"));
        pid.parse::<i32>()
            .unwrap_or_else(|_| panic!("not a pid: {line:?}"));
        assert!(!command.is_empty(), "{line:?}");
    }
}

#[test]
fn ps_ls_offers_neither_cid_nor_the_process_that_ran_it() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["ps", "ls"]);
    run.ok();
    let own = std::process::id().to_string();
    for line in run.lines() {
        let pid = line.split(' ').next().unwrap();
        assert_ne!(pid, own, "offered the process that ran it: {line:?}");
    }
}

/// Port 0 is the one port nothing can be listening on, which is what makes
/// this a test of the flag rather than of whatever the machine happens to be
/// running.
#[test]
fn a_port_nothing_holds_says_so_rather_than_listing_everything() {
    let sandbox = Sandbox::new();
    for verb in [&["ls"][..], &["sel"][..], &["kill"][..]] {
        let mut args = vec!["ps"];
        args.extend_from_slice(verb);
        args.extend_from_slice(&["--port", "0"]);
        let run = sandbox.run(&args);
        run.code(1);
        assert!(
            run.stderr.contains("port 0"),
            "`proc {}`: {}",
            verb.join(" "),
            run.stderr
        );
        assert!(
            run.stdout.is_empty(),
            "listed processes the port did not name: {}",
            run.stdout
        );
    }
}

#[test]
fn a_port_has_to_be_one() {
    let sandbox = Sandbox::new();
    sandbox.run(&["ps", "ls", "--port", "not-a-port"]).code(2);
    sandbox.run(&["ps", "ls", "--port", "70000"]).code(2);
}

#[test]
fn ps_ls_status_adds_columns_ahead_of_the_command() {
    let sandbox = Sandbox::new();
    let plain = sandbox.run(&["ps", "ls"]);
    let status = sandbox.run(&["ps", "ls", "--status"]);
    plain.ok();
    status.ok();
    let widest_plain = plain.lines().iter().map(|l| l.len()).max().unwrap_or(0);
    let widest_status = status.lines().iter().map(|l| l.len()).max().unwrap_or(0);
    assert!(
        widest_status > widest_plain,
        "--status added nothing:\n{}",
        status.stdout
    );
}

#[test]
fn ps_ls_status_is_pipe_safe_by_default() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["ps", "ls", "--status"]);
    run.ok();
    assert!(!run.stdout.contains('\x1b'), "coloured a pipe");
    let forced = sandbox.run(&["--color", "always", "ps", "ls", "--status"]);
    forced.ok();
    assert!(forced.stdout.contains('\x1b'), "--color always did nothing");
}

#[test]
fn ps_kill_refuses_an_unknown_signal_before_selecting() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["ps", "kill", "--signal", "HUPP"]);
    run.code(1);
    assert!(run.stderr.contains("unknown signal"), "{}", run.stderr);
    assert!(run.stderr.contains("known signals are"), "{}", run.stderr);
}

/// To `kill`, `0` and a negative number are process *groups*. Run for real,
/// with a signal that would work, so the refusal is what the test proves.
#[test]
fn ps_kill_refuses_a_pid_that_is_really_a_process_group() {
    let sandbox = Sandbox::new();
    for pid in ["0", "-1"] {
        let run = sandbox.run(&["ps", "kill", "--signal", "CONT", "--", pid]);
        run.code(1);
        assert!(
            run.stderr.contains("refusing to signal"),
            "{pid} was not refused: {}",
            run.stderr
        );
        assert!(run.stdout.is_empty(), "{pid} was signalled: {}", run.stdout);
    }
}

#[test]
fn ps_kill_still_accepts_an_ordinary_pid() {
    let sandbox = Sandbox::new();
    // What matters is that cid got as far as asking `kill`.
    let run = sandbox.run(&["ps", "kill", "--signal", "CONT", "2147483646"]);
    assert!(
        !run.stderr.contains("refusing to signal"),
        "an ordinary pid was refused: {}",
        run.stderr
    );
}

#[test]
fn ps_kill_will_not_take_a_signal_and_force_at_once() {
    let sandbox = Sandbox::new();
    sandbox
        .run(&["ps", "kill", "--signal", "TERM", "--force"])
        .code(2);
}

/// Uses a child of the test's own making, so nothing on the machine is at
/// risk.
#[test]
fn ps_kill_signals_the_pid_it_is_given() {
    let sandbox = Sandbox::new();
    let mut child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawning a process to kill");
    let pid = child.id().to_string();

    let run = sandbox.run(&["ps", "kill", &pid]);
    run.ok();
    assert!(
        run.stdout.contains("sent TERM to"),
        "did not report the signal:\n{}",
        run.stdout
    );

    let status = child.wait().expect("waiting for the killed process");
    assert!(
        !status.success(),
        "the process outlived the signal: {status:?}"
    );
}

#[test]
fn ps_kill_force_sends_kill() {
    let sandbox = Sandbox::new();
    let mut child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawning a process to kill");
    let pid = child.id().to_string();

    let run = sandbox.run(&["ps", "kill", "--force", &pid]);
    run.ok();
    assert!(run.stdout.contains("sent KILL to"), "{}", run.stdout);
    child.wait().expect("waiting for the killed process");
}

#[test]
fn ps_kill_passes_a_failure_through() {
    let sandbox = Sandbox::new();
    let mut child = Command::new("sleep").arg("0").spawn().expect("spawning");
    let pid = child.id().to_string();
    child.wait().expect("reaping");

    let run = sandbox.run(&["ps", "kill", &pid]);
    assert_ne!(run.code, Some(0), "signalled a process that had exited");
    assert!(
        !run.stderr.contains("error: "),
        "restated what kill already said:\n{}",
        run.stderr
    );
}

// --- fish history -----------------------------------------------------------

/// Write a fish history file where cid looks for one by default.
fn write_history(sandbox: &Sandbox, body: &str) {
    let path = sandbox.home().join(".local/share/fish/fish_history");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn history_ls_is_newest_first_and_one_command_per_line() {
    let sandbox = Sandbox::new();
    write_history(
        &sandbox,
        "- cmd: cargo test\n  when: 100\n\
         - cmd: git status\n  when: 200\n\
         - cmd: cargo test\n  when: 300\n\
         - cmd: git commit -m 'a\\nb'\n  when: 400\n",
    );

    let run = sandbox.run(&["history", "ls"]);
    run.ok();
    assert_eq!(
        run.lines(),
        vec!["git commit -m 'a ⏎ b'", "cargo test", "git status"],
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn history_ls_status_dates_every_row_in_one_column() {
    let sandbox = Sandbox::new();
    write_history(
        &sandbox,
        "- cmd: cargo test\n  when: 1785394626\n- cmd: undated\n",
    );

    let run = sandbox.run(&["history", "ls", "--status"]);
    run.ok();
    let lines = run.lines();
    // Newest first, and an entry with no `when:` sorts as it was stored.
    assert_eq!(lines.len(), 2, "{lines:?}");
    let (undated, dated) = (lines[0], lines[1]);

    assert!(
        dated.starts_with("2026-07-30 "),
        "not a local timestamp: {dated:?}"
    );
    // Same column on both rows, so `cut -c18-` and `sort` work.
    let column = |line: &str| line.len() - line.trim_start().len();
    assert_eq!(
        column(undated),
        "2026-07-30 13:57  ".len(),
        "an undated row did not hold the column open: {undated:?}"
    );
    assert!(undated.trim_end().ends_with("undated"), "{undated:?}");
}

/// ctrl-o reaches the selector by handing `cid-repo-cd` to fish, which
/// records it. Offered back, those rows sit at the top of ctrl-r — the newest
/// commands in the file — and none of them is a command anyone can use.
#[test]
fn the_key_bindings_cid_emits_are_not_listed_as_history() {
    let sandbox = Sandbox::new();
    write_history(
        &sandbox,
        "- cmd: git status\n  when: 100\n\
         - cmd: cid-repo-cd\n  when: 200\n\
         - cmd: cid-history-select\n  when: 300\n\
         - cmd: cid repo sel\n  when: 400\n\
         - cmd: fe -t\n  when: 500\n",
    );

    let run = sandbox.run(&["history", "ls"]);
    run.ok();
    assert_eq!(
        run.lines(),
        vec!["fe -t", "cid repo sel", "git status"],
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn history_without_a_file_names_the_path_it_looked_at() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["history", "ls"]);
    run.code(1);
    assert!(run.stderr.contains("fish_history"), "{}", run.stderr);
    assert!(run.stderr.contains("[history] file"), "{}", run.stderr);
}

#[test]
fn history_survives_invalid_utf8() {
    let sandbox = Sandbox::new();
    let path = sandbox.home().join(".local/share/fish/fish_history");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = b"- cmd: echo ".to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(b"\n  when: 100\n- cmd: git status\n  when: 200\n");
    std::fs::write(&path, bytes).unwrap();

    let run = sandbox.run(&["history", "ls"]);
    run.ok();
    assert_eq!(run.lines().len(), 2, "{:?}", run.lines());
    assert_eq!(run.lines()[0], "git status");
}

/// `println!` answers a closed pipe with a panic. A short listing fits in the
/// pipe buffer and never triggers it, so the fixture is several times its
/// size.
#[test]
fn a_listing_ends_quietly_when_the_reader_stops_reading() {
    let sandbox = Sandbox::new();
    let mut history = String::new();
    // Past the 64 KiB pipe buffer, so the child is still writing when the read
    // end goes away.
    for i in 0..5000 {
        history.push_str(&format!(
            "- cmd: echo this is history entry number {i}\n  when: {}\n",
            1_700_000_000 + i
        ));
    }
    write_history(&sandbox, &history);

    let mut child = Command::new(BIN)
        .args(["history", "ls"])
        .current_dir(sandbox.home())
        .env_clear()
        .env("HOME", sandbox.home())
        .env("PWD", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.home().join(".config"))
        .env("XDG_DATA_HOME", sandbox.home().join(".local/share"))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning cid");

    // Read a little, then close the pipe — this is what `head` does.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = [0u8; 64];
    stdout.read_exact(&mut buf).expect("no rows at all");
    drop(stdout);

    let out = child.wait_with_output().expect("waiting for cid");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a closed pipe panicked:\n{stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
}

// --- shell integration ------------------------------------------------------

#[test]
fn init_fish_emits_functions_bindings_and_completions() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        "[shell.bindings]\nctrl-o = \"repo-cd\"\nctrl-t = \"worktree-cd\"\n\
         ctrl-r = \"history-select\"\n\n[shell.aliases]\nfe = \"edit\"\n",
    );
    let run = sandbox.run(&["init", "fish"]);
    run.ok();
    for needle in [
        "function cid-repo-cd",
        "function cid-worktree-cd",
        "function cid-history-select",
        "function fe",
        "function cid_key_bindings",
        "complete -c cid",
    ] {
        assert!(run.stdout.contains(needle), "missing {needle:?}");
    }
}

/// cid takes no key of its own: a config that binds nothing gets a shell
/// with the completions and an empty binding function, and no more.
#[test]
fn init_fish_binds_nothing_until_the_config_says_so() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["init", "fish"]);
    run.ok();

    assert!(
        run.stdout.contains("function cid_key_bindings"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("    bind "), "{}", run.stdout);
    assert!(!run.stdout.contains("function fe"), "{}", run.stdout);
    assert!(run.stdout.contains("complete -c cid"), "{}", run.stdout);
}

#[test]
fn init_emits_completions_for_the_other_shells() {
    let sandbox = Sandbox::new();
    for shell in ["bash", "zsh", "powershell", "elvish"] {
        let run = sandbox.run(&["init", shell]);
        run.ok();
        assert!(run.stdout.contains("cid"), "{shell} completions were empty");
        assert!(
            !run.stdout.contains("cid_key_bindings"),
            "{shell} got fish's bindings"
        );
    }
}

#[test]
fn init_works_before_there_is_any_config() {
    let sandbox = Sandbox::new();
    assert!(!sandbox.config_path().exists());
    sandbox.run(&["init", "fish"]).ok();
}

// --- notes ------------------------------------------------------------------

/// Write a note into `<home>/notes`, dated `modified` seconds after the epoch
/// so a listing's order is decided rather than raced for.
fn mk_note(vault: &Path, rel: &str, body: &str, modified: u64) {
    let path = vault.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// A vault of three notes, the newest last in the alphabet so ordering by name
/// and ordering by date cannot be mistaken for each other. `archive` carries a
/// label and `zeta.md` sits at the root, so a listing has one of each.
fn mk_vault(sandbox: &Sandbox) -> PathBuf {
    let vault = sandbox.home().join("notes");
    mk_note(
        &vault,
        "archive/old.md",
        "---\ntags: [done]\n---\n\nold\n- [x] finished\n",
        1_000,
    );
    mk_note(
        &vault,
        "inbox.md",
        "---\ntitle: Inbox\ntags:\n  - todo\n  - work\ncreated: 2024-03-01\n---\n\n# Inbox\n\n- [ ] call the bank\n- [x] renew\n",
        3_000,
    );
    mk_note(&vault, "zeta.md", "no front matter\n", 2_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\nlabels = {{ old = [\"archive\"] }}\n",
        vault.display().to_string()
    ));
    vault
}

/// `note ls` is read by other tools, so what it prints is a path they can
/// open: absolute, one per line, newest first.
#[test]
fn note_ls_prints_absolute_paths_newest_first() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    let run = sandbox.run(&["note", "ls"]);
    run.ok();
    assert_eq!(
        run.lines(),
        vec![
            vault.join("inbox.md").to_str().unwrap(),
            vault.join("zeta.md").to_str().unwrap(),
            vault.join("archive/old.md").to_str().unwrap(),
        ]
    );
}

/// The vault holds an attachment and an Obsidian settings directory, neither of
/// which is a note.
#[test]
fn note_ls_offers_markdown_and_not_the_rest_of_the_vault() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    std::fs::write(vault.join("diagram.png"), "").unwrap();
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    std::fs::write(vault.join(".obsidian/app.md"), "").unwrap();

    let run = sandbox.run(&["note", "ls"]);
    run.ok();

    assert!(!run.stdout.contains("diagram"), "{}", run.stdout);
    assert!(!run.stdout.contains("obsidian"), "{}", run.stdout);
}

/// Every path the listing prints opens, which is the point of printing them.
#[test]
fn every_path_note_ls_prints_is_one_that_exists() {
    let sandbox = Sandbox::new();
    mk_vault(&sandbox);
    let run = sandbox.run(&["note", "ls"]);
    run.ok();
    for line in run.lines() {
        assert!(Path::new(line).is_file(), "{line}");
    }
}

/// The status listing is what a script reads a note's metadata out of, so the
/// name it leads with has to be the one `note edit` takes back.
#[test]
fn note_ls_status_carries_the_tags_and_both_dates() {
    let sandbox = Sandbox::new();
    mk_vault(&sandbox);
    let run = sandbox.run(&["note", "ls", "--status"]);
    run.ok();

    // `--status` is the listing a person reads, so the home directory it all
    // sits under is collapsed rather than repeated down every row.
    let inbox = run.lines()[0].to_string();
    assert!(inbox.starts_with("~/notes/inbox.md"), "{inbox}");
    assert!(inbox.contains("#todo #work"), "{inbox}");
    // The front matter's creation date, not the file's: this vault was written
    // moments ago.
    assert!(inbox.ends_with("2024-03-01"), "{inbox}");

    // The label its directory carries, which is the point of configuring one.
    let old = run
        .lines()
        .iter()
        .find(|l| l.contains("archive/old.md"))
        .expect("the archived note")
        .to_string();
    assert!(old.contains("old"), "{old}");
}

/// A directory nothing labelled still names itself, so a vault with two of five
/// directories labelled does not show three rows saying nothing.
#[test]
fn note_ls_status_names_an_unlabelled_directory_after_itself() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "scratch/idea.md", "idea\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "ls", "--status"]);
    run.ok();
    assert!(run.stdout.contains("scratch"), "{}", run.stdout);
}

/// A vault that files notes by kind and archives them the same way, with an
/// `archive` under each kind and one note still in use beside it.
fn mk_archived_vault(sandbox: &Sandbox) -> PathBuf {
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "work/plan.md", "plan\n", 4_000);
    mk_note(&vault, "work/archive/old.md", "old\n", 3_000);
    mk_note(&vault, "personal/idea.md", "idea\n", 2_000);
    mk_note(&vault, "personal/archive/2023.md", "last year\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\narchives = [\"work/archive\", \"personal/archive\"]\n",
        vault.display().to_string()
    ));
    vault
}

/// An archive is what a vault keeps and stops reading, so a listing leaves it
/// out — and `--all` is how you get at it anyway.
#[test]
fn note_ls_leaves_out_the_archives_until_all_asks_for_them() {
    let sandbox = Sandbox::new();
    let vault = mk_archived_vault(&sandbox);

    let listed = sandbox.run(&["note", "ls"]);
    listed.ok();
    assert_eq!(
        listed.lines(),
        vec![
            vault.join("work/plan.md").to_str().unwrap(),
            vault.join("personal/idea.md").to_str().unwrap(),
        ]
    );

    let all = sandbox.run(&["note", "ls", "--all"]);
    all.ok();
    assert_eq!(
        all.lines(),
        vec![
            vault.join("work/plan.md").to_str().unwrap(),
            vault.join("work/archive/old.md").to_str().unwrap(),
            vault.join("personal/idea.md").to_str().unwrap(),
            vault.join("personal/archive/2023.md").to_str().unwrap(),
        ]
    );
}

/// Naming a note is asking for that note, whatever a listing would have shown.
#[test]
fn note_open_takes_the_name_of_an_archived_note() {
    let sandbox = Sandbox::new();
    let vault = mk_archived_vault(&sandbox);

    let run = sandbox.run(&["note", "open", "work/archive/old.md"]);
    run.ok();
    assert!(
        run.stdout
            .contains(vault.join("work/archive/old.md").to_str().unwrap()),
        "{}",
        run.stdout
    );
    assert!(!run.stderr.contains("warning"), "{}", run.stderr);
}

/// A vault whose every note is archived reports that, rather than reporting a
/// vault with nothing in it.
#[test]
fn note_ls_says_the_archives_are_why_it_found_nothing() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "archive/old.md", "old\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\narchives = [\"archive\"]\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "ls"]);
    run.code(1);
    assert!(run.stderr.contains("archives"), "{}", run.stderr);
}

/// An archive path is relative to the vault, and one that names nothing hides
/// nothing — which is a setting the user meant and did not get.
#[test]
fn config_check_reports_an_archive_that_is_not_there() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "work/plan.md", "plan\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\narchives = [\"work/archive\"]\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["config", "check"]);
    assert!(run.stdout.contains("work/archive"), "{}", run.stdout);
}

/// The path `note new` handed the editor, from the `echo` the sandbox's vault
/// uses as one.
fn opened(run: &Run) -> PathBuf {
    PathBuf::from(run.stdout.trim().strip_prefix("-- ").expect("a path"))
}

#[test]
fn note_new_names_the_note_for_the_day_and_its_title_and_writes_the_title_as_its_h1() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    let run = sandbox.run(&["note", "new", "Migrate to AWS"]);
    run.ok();

    let opened = opened(&run);
    assert_eq!(opened.parent(), Some(vault.as_path()), "{opened:?}");
    let name = opened.file_name().unwrap().to_str().unwrap();
    let (day, rest) = name.split_at(10);
    assert!(
        day.chars().all(|c| c.is_ascii_digit() || c == '-'),
        "{name}"
    );
    assert_eq!(rest, "-migrate-to-aws.md");
    assert_eq!(
        std::fs::read_to_string(&opened).unwrap(),
        "# Migrate to AWS\n"
    );
}

/// One title typed twice is a second note, never the first one reopened.
#[test]
fn note_new_never_hands_back_a_name_already_in_use() {
    let sandbox = Sandbox::new();
    mk_vault(&sandbox);
    let first = sandbox.run(&["note", "new", "Standup"]);
    first.ok();
    let second = sandbox.run(&["note", "new", "Standup"]);
    second.ok();

    let first = opened(&first);
    let second = opened(&second);
    assert_eq!(
        second.to_str().unwrap(),
        first.to_str().unwrap().replace(".md", "-2.md")
    );
}

/// Without a title the command asks for one, and a script has no way to answer.
#[test]
fn note_new_without_a_title_or_a_terminal_fails_and_creates_nothing() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    let before = std::fs::read_dir(&vault).unwrap().count();
    let run = sandbox.run(&["note", "new"]);
    run.code(1);
    assert!(run.stderr.contains("TITLE"), "{}", run.stderr);
    assert_eq!(std::fs::read_dir(&vault).unwrap().count(), before);
}

#[test]
fn note_without_a_vault_says_which_key_is_missing() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[repo]\nroot = \"~/dev\"\n");
    let run = sandbox.run(&["note", "ls"]);
    run.code(1);
    assert!(run.stderr.contains("[note] root"), "{}", run.stderr);
}

#[test]
fn note_with_a_vault_that_is_not_there_says_so() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[note]\nroot = \"~/nowhere\"\n");
    let run = sandbox.run(&["note", "ls"]);
    run.code(1);
    assert!(run.stderr.contains("not a directory"), "{}", run.stderr);
}

/// `[note] editor` is `echo` here, so what it was asked to open comes back on
/// stdout. A name is a path below the vault — what `note ls` printed.
#[test]
fn note_open_resolves_a_name_against_the_vault() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    let run = sandbox.run(&["note", "open", "archive/old.md"]);
    run.ok();
    assert_eq!(
        run.stdout.trim(),
        format!("-- {}", vault.join("archive/old.md").display())
    );
}

/// Several names open together, in the order they were given.
#[test]
fn note_open_takes_several_names_at_once() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);
    let run = sandbox.run(&["note", "open", "inbox.md", "archive/old.md"]);
    run.ok();
    assert_eq!(
        run.stdout.trim(),
        format!(
            "-- {} {}",
            vault.join("inbox.md").display(),
            vault.join("archive/old.md").display()
        )
    );
}

/// The key exists so a note can be opened by something other than the editor
/// the rest of cid uses; with none set, it is that editor.
#[test]
fn note_open_falls_back_to_the_environment_editor() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "a.md", "a\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\n",
        vault.display().to_string()
    ));

    let run = sandbox.run_with_env(&["note", "open", "a.md"], &[("EDITOR", "echo")]);
    run.ok();
    assert_eq!(
        run.stdout.trim(),
        format!("-- {}", vault.join("a.md").display())
    );
}

/// `config check` is what a setup script runs, so a configured vault has to
/// report as found rather than as a thing cid knows nothing about.
#[test]
fn config_check_counts_the_notes_in_the_vault() {
    let sandbox = Sandbox::new();
    mk_vault(&sandbox);
    let run = sandbox.run(&["config", "check"]);
    assert!(run.stdout.contains("3 notes"), "{}", run.stdout);
    assert!(run.stdout.contains("note editor"), "{}", run.stdout);
    // `note open` shells out to it to search, so the report says whether it is
    // there.
    assert!(run.stdout.contains("rg"), "{}", run.stdout);
    // So does every preview pane, which is where the theme is applied.
    assert!(run.stdout.contains("bat"), "{}", run.stdout);
}

/// The scratch note is the same file every time, and its directory is made for
/// it — otherwise the editor has nowhere to write.
#[test]
fn note_scratch_opens_one_permanent_file() {
    let sandbox = Sandbox::new();
    let vault = mk_vault(&sandbox);

    let first = sandbox.run(&["note", "scratch"]);
    first.ok();
    let second = sandbox.run(&["note", "scratch"]);
    second.ok();

    assert_eq!(first.stdout, second.stdout, "the scratch note moved");
    assert_eq!(
        first.stdout.trim(),
        format!("-- {}", vault.join("scratch/scratch.md").display())
    );
    assert!(vault.join("scratch").is_dir(), "the directory was not made");
}

#[test]
fn note_scratch_goes_where_the_config_says() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "a.md", "a\n", 1_000);
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\nscratch = \"pad.md\"\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "scratch"]);
    run.ok();
    assert_eq!(
        run.stdout.trim(),
        format!("-- {}", vault.join("pad.md").display())
    );
}

/// A mistyped name would otherwise open a new, empty buffer and say nothing,
/// which is how a typo becomes a second note.
#[test]
fn note_open_warns_when_the_note_named_is_not_there() {
    let sandbox = Sandbox::new();
    mk_vault(&sandbox);
    let run = sandbox.run(&["note", "open", "inbx.md"]);
    run.ok();
    assert!(
        run.stderr.contains("no note called inbx.md"),
        "{}",
        run.stderr
    );
}

/// A vault where every note has a name and something in it has nothing to
/// clean up, and says so rather than opening an empty selector.
#[test]
fn note_cleanup_says_so_when_there_is_nothing_to_do() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(
        &vault,
        "thoughts.md",
        "a real note with rather more than a couple of dozen characters in it\n",
        1_000,
    );
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "cleanup"]);
    run.ok();
    assert!(run.stdout.contains("Nothing to clean up"), "{}", run.stdout);
}

/// The three kinds, and nothing else. Without a terminal the selector cannot
/// open, so this proves what was classified rather than what was chosen.
#[test]
fn note_cleanup_offers_the_three_kinds_and_leaves_real_notes_alone() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "empty.md", "", 1_000);
    mk_note(
        &vault,
        "Untitled 1.md",
        "this one has plenty of words in it\n",
        1_000,
    );
    mk_note(
        &vault,
        "2026-08-25 1043.md",
        "jotted down in a hurry, no name\n",
        1_000,
    );
    mk_note(
        &vault,
        "thoughts.md",
        "a real note with rather more than a couple of dozen characters\n",
        1_000,
    );
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "cleanup"]);

    // It got past classification and failed at the selector, which is the only
    // thing a test without a terminal can reach.
    run.code(1);
    assert!(
        run.stderr.contains("needs a terminal"),
        "cleanup did not reach the selector: {}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("Nothing to clean up"),
        "three candidates were classified as none: {}",
        run.stdout
    );
}

/// A vault is named by whoever writes in it, and this one's owner writes
/// Norwegian. `note cleanup` panicked on `Prosjektø` — the eighth byte of the
/// name is half an `ø`, and `untitled` is eight bytes long — so every note verb
/// a script can reach is run over a vault full of characters that are not
/// ASCII. A panic anywhere is the bug this is here for.
#[test]
fn every_note_command_survives_a_vault_that_is_not_ascii() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notater");

    // The shape that crashed it, and the only one that does: `untitled` is
    // eight bytes, so the name must have a character *straddling* its eighth —
    // seven ASCII bytes and then a two-byte `ø`. `Prosjektø` has eight before
    // the `ø` and is therefore fine, which is what makes this so easy to miss.
    mk_note(
        &vault,
        "Oppgaveøkt.md",
        "en økt med ganske mange ord i seg\n",
        5_000,
    );
    mk_note(
        &vault,
        "Notater—2024.md",
        "notater fra hele året, ganske mange\n",
        4_500,
    );
    // The near misses, kept so the test covers both sides of the boundary.
    mk_note(
        &vault,
        "Prosjektø.md",
        "et prosjekt med mange ord i seg\n",
        4_200,
    );
    mk_note(
        &vault,
        "øvelser.md",
        "øvelser og løsninger, ganske mange\n",
        4_000,
    );
    // Non-ASCII directories, front matter, tags and body.
    mk_note(
        &vault,
        "møter/årsmøte.md",
        "---\ntitle: Årsmøte\ntags: [løsning, ærlig]\ncreated: 2024-03-01\n---\n\n# Årsmøte\n\n- [ ] følge opp\nse [[nøtter/møte]] og #løsning\n",
        3_000,
    );
    // The three cleanup shapes, in Norwegian and beyond it.
    mk_note(&vault, "Untitled ø.md", "", 2_000);
    mk_note(
        &vault,
        "日本語/ノート.md",
        "日本語のノートです、かなり長い文章\n",
        1_000,
    );
    mk_note(
        &vault,
        "2026-08-25 1043.md",
        "notert i all hast, uten navn\n",
        900,
    );

    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\nlabels = {{ arbeid = [\"møter\"] }}\n",
        vault.display().to_string()
    ));

    sandbox.run(&["note", "ls"]).ok();
    sandbox.run(&["note", "ls", "--status"]).ok();
    sandbox.run(&["note", "open", "øvelser.md"]).ok();
    sandbox.run(&["note", "new", "Løsningsforslag"]).ok();
    sandbox.run(&["note", "scratch"]).ok();
    sandbox.run(&["config", "check"]);

    // The one that crashed. Without a terminal it stops at the selector, which
    // is past every byte offset that was the bug.
    let cleanup = sandbox.run(&["note", "cleanup"]);
    assert_eq!(
        cleanup.code,
        Some(1),
        "note cleanup did not reach the selector\n--- stdout ---\n{}\n--- stderr ---\n{}",
        cleanup.stdout,
        cleanup.stderr,
    );
    assert!(
        cleanup.stderr.contains("needs a terminal"),
        "note cleanup failed before the selector: {}",
        cleanup.stderr,
    );
    assert!(
        !cleanup.stderr.contains("panicked"),
        "note cleanup panicked: {}",
        cleanup.stderr,
    );
}

/// The label, the folder and the name all come off one path by cutting it up,
/// and a Norwegian directory makes every one of those cuts land differently.
#[test]
fn note_ls_status_reads_a_norwegian_vault_correctly() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notater");
    mk_note(
        &vault,
        "møter/årsmøte.md",
        "---\ntags: [løsning]\ncreated: 2024-03-01\n---\n\nærlig talt\n",
        1_000,
    );
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\nlabels = {{ arbeid = [\"møter\"] }}\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "ls", "--status"]);
    run.ok();

    let row = run.lines()[0].to_string();
    assert!(row.starts_with("~/notater/møter/årsmøte.md"), "{row}");
    assert!(row.contains("arbeid"), "{row}");
    assert!(row.contains("#løsning"), "{row}");
    assert!(row.ends_with("2024-03-01"), "{row}");
}

/// The scratch note is empty by design — that is what a scratch note is — so
/// a cleanup list that offered it would offer it on every single run.
#[test]
fn note_cleanup_never_offers_the_scratch_note() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "scratch/scratch.md", "", 1_000);
    mk_note(
        &vault,
        "thoughts.md",
        "a real note with rather more than a couple of dozen characters in it\n",
        900,
    );
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "cleanup"]);

    run.ok();
    assert!(
        run.stdout.contains("Nothing to clean up"),
        "the scratch note was offered for deletion: {}",
        run.stdout
    );
}

/// And it is still protected when the config moves it somewhere else.
#[test]
fn note_cleanup_protects_the_configured_scratch_note() {
    let sandbox = Sandbox::new();
    let vault = sandbox.home().join("notes");
    mk_note(&vault, "pad.md", "", 1_000);
    mk_note(
        &vault,
        "thoughts.md",
        "a real note with rather more than a couple of dozen characters in it\n",
        900,
    );
    sandbox.write_config(&format!(
        "[note]\nroot = {:?}\neditor = \"echo\"\nscratch = \"pad.md\"\n",
        vault.display().to_string()
    ));

    let run = sandbox.run(&["note", "cleanup"]);
    run.ok();
    assert!(run.stdout.contains("Nothing to clean up"), "{}", run.stdout);
}

// --- project ----------------------------------------------------------------

/// A directory with the manifests of `names`, standing in for a project.
fn mk_project(sandbox: &Sandbox, files: &[(&str, &str)]) -> PathBuf {
    let dir = sandbox.home().join("project");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).unwrap();
    }
    dir
}

#[test]
fn project_deps_dry_run_names_a_command_per_detected_toolchain() {
    let sandbox = Sandbox::new();
    let dir = mk_project(
        &sandbox,
        &[
            ("mise.toml", "[tools]\nnode = \"22\"\n"),
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("package.json", "{}"),
            ("bun.lock", ""),
        ],
    );

    let run = sandbox.run_in(&dir, &["project", "deps", "--dry-run"]);
    run.ok();

    let lines = run.lines();
    assert_eq!(lines.len(), 3, "{}", run.stdout);
    assert!(lines[0].ends_with("$ mise install"), "{}", lines[0]);
    assert!(lines[1].ends_with("$ cargo fetch"), "{}", lines[1]);
    assert!(lines[2].ends_with("$ bun install"), "{}", lines[2]);
}

#[test]
fn project_deps_dump_lists_what_each_manifest_declares_with_its_context() {
    let sandbox = Sandbox::new();
    let dir = mk_project(
        &sandbox,
        &[
            (
                "Cargo.toml",
                "[dependencies]\nanyhow = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n",
            ),
            (
                "package.json",
                r#"{"devDependencies": {"typescript": "^5.7.0"}}"#,
            ),
        ],
    );

    let run = sandbox.run_in(&dir, &["project", "deps", "--dump"]);
    run.ok();

    assert_eq!(
        run.lines(),
        vec![
            "rust  Cargo.toml",
            "  dependencies",
            "    anyhow    1",
            "  dev-dependencies",
            "    tempfile  3",
            "",
            "npm  package.json",
            "  devDependencies",
            "    typescript  ^5.7.0",
        ],
        "{}",
        run.stdout
    );
}

/// The alias `i` is bound to in fish, so the flag has to reach the same place
/// through the group's one-letter form.
#[test]
fn the_project_group_answers_to_its_abbreviation() {
    let sandbox = Sandbox::new();
    let dir = mk_project(&sandbox, &[("go.mod", "module x\n")]);

    let run = sandbox.run_in(&dir, &["pj", "deps", "-n"]);
    run.ok();
    assert!(run.stdout.contains("$ go mod download"), "{}", run.stdout);
}

#[test]
fn dumping_and_dry_running_the_same_run_is_refused() {
    let sandbox = Sandbox::new();
    let run = sandbox.run(&["project", "deps", "--dump", "--dry-run"]);
    run.code(2);
}

#[test]
fn a_directory_with_nothing_recognisable_in_it_is_not_a_failure() {
    let sandbox = Sandbox::new();
    let dir = mk_project(&sandbox, &[("README.md", "prose")]);

    let run = sandbox.run_in(&dir, &["project", "deps"]);
    run.ok();
    assert_eq!(run.stdout, "", "printed a result it does not have");
    assert!(
        run.stderr.contains("nothing recognisable"),
        "{}",
        run.stderr
    );
}

#[test]
fn project_build_runs_the_committed_task_runner_rather_than_the_toolchain() {
    let sandbox = Sandbox::new();
    let dir = mk_project(
        &sandbox,
        &[
            ("Makefile", "all:\n\t@true\n"),
            ("Cargo.toml", "[package]\n"),
        ],
    );

    let run = sandbox.run_in(&dir, &["project", "build", "-n"]);
    run.ok();

    let lines = run.lines();
    assert_eq!(lines.len(), 1, "{}", run.stdout);
    assert!(lines[0].starts_with("make  Makefile"), "{}", lines[0]);
    assert!(lines[0].ends_with("$ make"), "{}", lines[0]);
}

#[test]
fn two_task_runners_stop_the_build_rather_than_one_being_guessed_at() {
    let sandbox = Sandbox::new();
    let dir = mk_project(&sandbox, &[("Taskfile.yml", ""), ("Makefile", "")]);

    let run = sandbox.run_in(&dir, &["project", "build"]);
    run.code(1);
    assert!(run.stderr.contains("Taskfile.yml"), "{}", run.stderr);
    assert!(run.stderr.contains("Makefile"), "{}", run.stderr);
}

#[test]
fn a_project_nothing_knows_how_to_build_says_so_and_fails() {
    let sandbox = Sandbox::new();
    let dir = mk_project(&sandbox, &[("requirements.txt", "rich\n")]);

    let run = sandbox.run_in(&dir, &["project", "build"]);
    run.code(1);
    assert!(run.stderr.contains("nothing here builds"), "{}", run.stderr);
}

#[test]
fn project_build_runs_each_toolchain_it_found_when_there_is_no_runner() {
    let sandbox = Sandbox::new();
    let dir = mk_project(
        &sandbox,
        &[
            ("go.mod", "module x\n"),
            ("package.json", r#"{"scripts": {"build": "tsc"}}"#),
        ],
    );

    let run = sandbox.run_in(&dir, &["project", "build", "-n"]);
    run.ok();

    let lines = run.lines();
    assert_eq!(lines.len(), 2, "{}", run.stdout);
    assert!(lines[0].ends_with("$ go build ./..."), "{}", lines[0]);
    assert!(lines[1].ends_with("$ npm run build"), "{}", lines[1]);
}

// --- configurable shell integration -----------------------------------------

#[test]
fn init_fish_emits_what_the_config_binds() {
    let sandbox = Sandbox::new();
    sandbox.write_config(
        "[shell.bindings]\nctrl-o = \"repo-cd\"\nup = \"history-up\"\n\n\
         [shell.aliases]\nb = \"project-build\"\n",
    );
    let run = sandbox.run(&["init", "fish"]);
    run.ok();

    for expected in [
        "bind ctrl-o \"cid-run-as-command cid-repo-cd\"",
        "bind up \"cid-history-up; commandline -f repaint\"",
        "function b ",
        "command cid project build $argv",
    ] {
        assert!(run.stdout.contains(expected), "{expected} missing");
    }
}

/// What a table holds is the whole of what is bound: a key left out of it is
/// one the shell never hears about.
#[test]
fn a_binding_table_is_the_whole_of_what_is_bound() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[shell.bindings]\nf6 = \"repo-cd\"\nctrl-b = \"project-build\"\n");

    let run = sandbox.run(&["init", "fish"]);
    run.ok();

    assert!(run.stdout.contains("bind f6 "), "{}", run.stdout);
    assert!(run.stdout.contains("bind ctrl-b "), "{}", run.stdout);
    assert!(!run.stdout.contains("bind ctrl-o"), "{}", run.stdout);
    // The aliases were not written, so there are none.
    assert!(!run.stdout.contains("function b "), "{}", run.stdout);
}

#[test]
fn a_configured_alias_takes_the_name_it_was_given() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[shell.aliases]\nbuild = \"project-build\"\n");

    let run = sandbox.run(&["init", "fish"]);
    run.ok();

    assert!(run.stdout.contains("function build "), "{}", run.stdout);
    assert!(!run.stdout.contains("function fe "), "{}", run.stdout);
}

/// A shell where one key works and another silently does not is worse than one
/// that says why at the moment it is sourced.
#[test]
fn an_action_cid_does_not_define_stops_init_rather_than_thinning_it() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[shell.bindings]\nctrl-o = \"repo-jump\"\n");

    let run = sandbox.run(&["init", "fish"]);
    run.code(1);
    assert_eq!(run.stdout, "", "emitted a shell it could not finish");
    assert!(run.stderr.contains("repo-jump"), "{}", run.stderr);
    assert!(run.stderr.contains("repo-cd"), "{}", run.stderr);
}

/// The two commands split this between them: `check` says the integration
/// resolves, `print` says what it resolves to.
#[test]
fn the_shell_integration_is_counted_by_check_and_named_by_print() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[shell.aliases]\nbuild = \"project-build\"\n");

    let check = sandbox.run(&["config", "check"]);
    let row = check
        .lines()
        .into_iter()
        .find(|line| line.contains("shell integration"))
        .unwrap_or_else(|| panic!("no shell row:\n{}", check.stdout));
    assert!(row.trim_start().starts_with('✓'), "{row}");
    assert!(row.contains("1 alias"), "{row}");
    assert!(
        !row.contains("build"),
        "the checklist repeated what `config print` is for: {row}"
    );

    let print = sandbox.run(&["config", "print"]);
    print.ok();
    assert!(print.stdout.contains("build"), "{}", print.stdout);
}

#[test]
fn config_check_fails_on_a_binding_nothing_answers_to() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[shell.bindings]\nctrl-o = \"repo-jump\"\n");

    let run = sandbox.run(&["config", "check"]);
    run.code(1);
    assert!(
        run.stdout.contains("repo-jump"),
        "the row does not name it:\n{}",
        run.stdout
    );
}

/// `-v` is the version, not verbosity: clap's own short is `-V`, and the hand
/// reaches for the lowercase one.
#[test]
fn the_short_version_flag_is_the_lowercase_one() {
    let sandbox = Sandbox::new();

    let short = sandbox.run(&["-v"]);
    short.ok();
    assert_eq!(short.stdout, sandbox.run(&["--version"]).ok().stdout);

    sandbox.run(&["-V"]).code(2);
}

/// `-v` was `--verbose`'s, and the version flag is not global, so a `-v` left
/// on the end of a subcommand is a usage error rather than a run that quietly
/// stopped being verbose.
#[test]
fn verbose_keeps_only_its_long_form() {
    let sandbox = Sandbox::new();

    sandbox.run(&["--verbose", "config", "path"]).ok();
    sandbox.run(&["config", "--verbose", "path"]).ok();
    sandbox.run(&["config", "path", "-v"]).code(2);
}

// --- stats ------------------------------------------------------------------

#[test]
fn every_run_records_itself_under_the_name_clap_matched() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "path"]).ok();
    // An alias is the same command, and is recorded as the command.
    sandbox.run(&["c", "path"]).ok();

    let log = std::fs::read_to_string(sandbox.stats_path()).expect("no stats log");
    let commands: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split('\t').nth(3))
        .collect();
    assert_eq!(commands, ["config path", "config path"], "{log}");
}

#[test]
fn show_counts_what_has_run_and_names_what_has_not() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "path"]).ok();
    sandbox.run(&["config", "path"]).ok();

    let run = sandbox.run(&["stats", "show"]);
    run.ok();
    let row = |name: &str| {
        run.stdout
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("no {name} row:\n{}", run.stdout))
            .to_string()
    };
    assert!(row("path").contains(" 2 "), "{}", row("path"));
    assert!(run.stdout.contains("own"), "no own column:\n{}", run.stdout);
    // The whole tree, so the report says what is going unused as well.
    assert!(row("worktree").contains('-'), "{}", row("worktree"));
    assert!(run.stdout.starts_with("command"), "{}", run.stdout);
}

/// The point of taking the selector's time out is that a command left waiting
/// is not recorded as a slow one, and nothing else in a run takes seconds.
#[test]
fn a_run_is_recorded_in_milliseconds_rather_than_in_how_long_it_sat_there() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "path"]).ok();

    let log = std::fs::read_to_string(sandbox.stats_path()).unwrap();
    let millis: u64 = log
        .lines()
        .next()
        .and_then(|line| line.split('\t').nth(1))
        .and_then(|millis| millis.parse().ok())
        .unwrap_or_else(|| panic!("no duration in {log:?}"));
    assert!(millis < 10_000, "{millis}ms for `config path`");
}

/// An editor holds the terminal for as long as the user stays in it, which is
/// their time and not the command's. Counted, it made `edit` the most expensive
/// command cid had.
#[cfg(unix)]
#[test]
fn the_time_spent_in_an_editor_is_not_the_time_the_command_took() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let editor = sandbox.home().join("slow-editor");
    std::fs::write(&editor, "#!/bin/sh\nsleep 1\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();

    sandbox
        .run_with_env(
            &["edit", "notes.md"],
            &[("EDITOR", &editor.to_string_lossy())],
        )
        .ok();

    let log = std::fs::read_to_string(sandbox.stats_path()).unwrap();
    let millis: u64 = log
        .lines()
        .next()
        .and_then(|line| line.split('\t').nth(1))
        .and_then(|millis| millis.parse().ok())
        .unwrap_or_else(|| panic!("no duration in {log:?}"));
    assert!(
        millis < 500,
        "the editor's second was charged to `edit`: {millis}ms"
    );
}

#[test]
fn reset_forgets_the_runs_and_says_how_many() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "path"]).ok();

    // Without a terminal to ask at, it names the flag that skips the question.
    let refused = sandbox.run(&["stats", "reset"]);
    refused.code(1);
    assert!(refused.stderr.contains("--yes"), "{}", refused.stderr);
    assert!(sandbox.stats_path().exists(), "the log went anyway");

    let run = sandbox.run(&["stats", "reset", "--yes"]);
    run.ok();
    // One run: the reset that was refused is a reset, and no reset records
    // itself.
    assert!(run.stdout.contains("forgot 1"), "{}", run.stdout);

    // What is left is nothing at all, rather than the command that did the
    // forgetting.
    let left = std::fs::read_to_string(sandbox.stats_path()).unwrap_or_default();
    assert_eq!(left, "", "the log kept a row");

    // And again, with nothing to forget.
    let again = sandbox.run(&["stats", "reset", "--yes"]);
    again.ok();
    assert!(
        again.stdout.contains("nothing recorded"),
        "{}",
        again.stdout
    );
}

#[test]
fn improve_prints_the_prompt_it_would_hand_over() {
    let sandbox = Sandbox::new();
    sandbox.run(&["config", "path"]).ok();

    let run = sandbox.run(&["stats", "improve", "--dry-run"]);
    run.ok();
    assert!(run.stdout.contains("`cid config path`"), "{}", run.stdout);
    assert!(run.stdout.contains("| command | runs |"), "{}", run.stdout);
}
