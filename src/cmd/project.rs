//! `cid project` — build the directory you are standing in, and install what
//! it needs, without knowing beforehand what it is written in.
//!
//! The whole group is ambient: it acts on `$PWD` rather than on a set cid
//! keeps, so there is nothing to list or select. What it finds there comes from
//! [`crate::project`], which decides everything; this module reads the
//! directory, runs the commands, and prints what happened.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::project::detect::{MANIFESTS, MISE_CONFIGS};
use crate::project::report::{Outcome, Status};
use crate::project::{Scan, Step, Toolchain, build, deps as manifests, detect, install, report};
use crate::{Ctx, Reported, stats, term};

/// For what cid says about a command rather than what the command said.
const DIM: u8 = term::SECONDARY;

/// `cid project deps` — install every detected toolchain's dependencies.
///
/// `dump` reads the manifests instead of running anything, and `dry_run` names
/// the commands the run would have been.
pub fn deps(ctx: &Ctx, dry_run: bool, dump: bool) -> Result<()> {
    let dir = Path::new(ctx.pwd_str());
    let scan = scan(dir)?;
    let detections = detect::detect(&scan);

    if detections.is_empty() {
        return unrecognised(ctx, dir);
    }
    if dump {
        let manifests = manifests::list(&detections, &scan);
        return print(&manifests::listing(&manifests, ctx.color()));
    }

    let plan = install::plan(&detections);
    if dry_run {
        return print(&report::plan_listing(
            &plan.steps().collect::<Vec<_>>(),
            ctx.color(),
        ));
    }

    run(ctx, plan, dir)
}

/// `cid project build` — run whatever building this project means.
pub fn build(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let dir = Path::new(ctx.pwd_str());
    let scan = scan(dir)?;
    let detections = detect::detect(&scan);

    let steps = match build::plan(&scan, &detections) {
        build::Build::Ambiguous(files) => bail!(
            "{} both build this project — run the one you meant",
            files.join(" and ")
        ),
        build::Build::Steps(steps) if steps.is_empty() => bail!(
            "nothing here builds: no task runner, and nothing detected that builds on its own"
        ),
        build::Build::Steps(steps) => under_mise(steps, &detections),
    };

    if dry_run {
        return print(&report::plan_listing(
            &steps.iter().collect::<Vec<_>>(),
            ctx.color(),
        ));
    }

    // Sequentially, with the terminal handed straight to each command: a build
    // is watched while it runs, and two of them writing at once is unreadable.
    // The first failure ends the run, since what follows would be built against
    // what did not.
    for step in &steps {
        eprintln!(
            "{}",
            term::paint(&format!("$ {}", step.command_line()), DIM, ctx.color())
        );
        inherit(step, dir)?;
    }
    Ok(())
}

/// Run every build step through `mise exec` when the project pins its tools
/// with mise and mise is installed — the same resolution an activated shell
/// would have done, for one that has not activated it.
fn under_mise(steps: Vec<Step>, detections: &[detect::Detection]) -> Vec<Step> {
    let pinned = detections
        .iter()
        .any(|detection| detection.toolchain == Toolchain::Mise);
    if !pinned || crate::cmd::config::on_path("mise").is_none() {
        return steps;
    }
    steps
        .into_iter()
        .map(crate::project::through_mise)
        .collect()
}

/// What is said in a directory holding nothing any toolchain recognises. Not a
/// failure: asking is how you find out.
fn unrecognised(ctx: &Ctx, dir: &Path) -> Result<()> {
    eprintln!(
        "{}",
        term::paint(
            &format!("nothing recognisable in {}", dir.display()),
            DIM,
            ctx.color()
        )
    );
    Ok(())
}

fn print(lines: &[String]) -> Result<()> {
    let mut out = term::Listing::stdout();
    for line in lines {
        if !out.line(line)? {
            return Ok(());
        }
    }
    out.finish()?;
    Ok(())
}

/// Run the plan: `mise install` to completion first, then the rest at once.
///
/// The concurrency buys the wall clock of the slowest install rather than the
/// sum of them all — every one of them waits on a network it does not share
/// with the others. It costs the output of a failed step being held back until
/// every step has finished, and with no terminal to redraw on, the status lines
/// arriving as each step finishes rather than in plan order.
fn run(ctx: &Ctx, plan: install::Plan, dir: &Path) -> Result<()> {
    let started = Instant::now();
    let color = ctx.color();
    let live = Report::new(&plan, ctx.log.verbose(), color);

    let mut outcomes = Vec::new();
    let mut pinned = false;
    if let Some(step) = &plan.mise {
        let outcome = live.run(0, step, dir);
        // Only a mise that actually installed can resolve the rest: wrapping
        // them after a failed install replaces each tool's own error with
        // mise's.
        pinned = outcome.status == Status::Done;
        outcomes.push(outcome);
    }

    let offset = usize::from(plan.mise.is_some());
    let steps: Vec<Step> = plan
        .parallel
        .into_iter()
        .map(|step| {
            if pinned {
                crate::project::through_mise(step)
            } else {
                step
            }
        })
        .collect();

    let running = &live;
    outcomes.extend(std::thread::scope(|scope| {
        let handles: Vec<_> = steps
            .iter()
            .enumerate()
            .map(|(index, step)| scope.spawn(move || running.run(offset + index, step, dir)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("an install thread panicked"))
            .collect::<Vec<_>>()
    }));

    if !live.finish() {
        details(&outcomes);
    }
    eprintln!();
    for line in report::summary(&outcomes, started.elapsed(), color) {
        eprintln!("{line}");
    }

    if outcomes.iter().any(Outcome::failed) {
        // The failing command's own output has just been printed above it.
        return Err(Reported(1).into());
    }
    Ok(())
}

/// How a run is reported while it happens.
enum Report {
    /// The whole plan drawn before anything runs, each line turning into a
    /// spinner with the step's latest output as it starts and settling on its
    /// result as it ends. Output is held back and shown only for a failure.
    Tree {
        slots: Vec<report::Slot>,
        rows: Arc<Mutex<Vec<Row>>>,
        /// `None` when stderr is not a terminal: each step then prints its
        /// status line as it finishes.
        board: Option<term::Board>,
        color: bool,
    },
    /// Every line of every step written to stderr as it arrives, behind the
    /// step's coloured prefix.
    Stream { prefixes: Vec<String>, color: bool },
}

/// A step's place in the tree, as the run has it so far.
enum Row {
    Waiting { after: Option<&'static str> },
    Running { started: Instant, activity: String },
    Finished(Outcome),
}

impl Report {
    /// Lay out the whole plan before anything runs, so every step is on screen
    /// from the start — the ones held back by mise included.
    fn new(plan: &install::Plan, streaming: bool, color: bool) -> Self {
        let nodes: Vec<report::Node> = plan
            .dependencies()
            .map(|(step, after)| report::Node {
                name: step.name,
                after,
            })
            .collect();

        if streaming {
            let names: Vec<&str> = nodes.iter().map(|node| node.name).collect();
            let prefixes = report::prefixes(&names, color);
            for (node, prefix) in nodes.iter().zip(&prefixes) {
                if let Some(after) = node.after {
                    eprintln!(
                        "{prefix}{}",
                        term::paint(&format!("waiting for {after}"), DIM, color)
                    );
                }
            }
            return Report::Stream { prefixes, color };
        }

        let slots = report::layout(&nodes);
        let rows = Arc::new(Mutex::new(
            nodes
                .iter()
                .map(|node| Row::Waiting { after: node.after })
                .collect::<Vec<_>>(),
        ));
        let render = {
            let slots = slots.clone();
            let rows = Arc::clone(&rows);
            Arc::new(move |frame: &str, columns: usize| {
                let rows = rows.lock().expect("the tree's rows were poisoned");
                slots
                    .iter()
                    .zip(rows.iter())
                    .map(|(slot, row)| report::tree_line(slot, &row.view(), frame, columns, color))
                    .collect()
            })
        };

        Report::Tree {
            slots,
            rows,
            board: term::board(render),
            color,
        }
    }

    /// Run the step at `index` in the plan, reporting it as it goes.
    fn run(&self, index: usize, step: &Step, dir: &Path) -> Outcome {
        match self {
            Report::Tree {
                slots,
                rows,
                board,
                color,
            } => {
                let set =
                    |row: Row| rows.lock().expect("the tree's rows were poisoned")[index] = row;
                let started = Instant::now();
                set(Row::Running {
                    started,
                    activity: String::new(),
                });

                let outcome = run_step(step, dir, &|line| {
                    if let Some(activity) = report::activity(line) {
                        set(Row::Running { started, activity });
                    }
                });

                if board.is_none() {
                    eprintln!("{}", report::status_line(&outcome, &slots[index], *color));
                }
                set(Row::Finished(outcome.clone()));
                outcome
            }
            Report::Stream { prefixes, color } => {
                let (prefix, color) = (&prefixes[index], *color);
                eprintln!(
                    "{prefix}{}",
                    term::paint(&format!("$ {}", step.command_line()), DIM, color)
                );
                let outcome = run_step(step, dir, &|line| eprintln!("{prefix}{line}"));
                if let Status::Skipped { reason } = &outcome.status {
                    eprintln!(
                        "{prefix}{}",
                        term::paint(&format!("skipped: {reason}"), DIM, color)
                    );
                }
                outcome
            }
        }
    }

    /// Settle the tree on its final state, leaving it on screen. Returns
    /// whether the output was streamed, and so has been seen already.
    fn finish(self) -> bool {
        match self {
            // Dropping the board draws it one last time.
            Report::Tree { board, .. } => {
                drop(board);
                false
            }
            Report::Stream { .. } => true,
        }
    }
}

impl Row {
    fn view(&self) -> report::Row<'_> {
        match self {
            Row::Waiting { after } => report::Row::Waiting { after: *after },
            Row::Running { started, activity } => report::Row::Running {
                elapsed: started.elapsed(),
                activity,
            },
            Row::Finished(outcome) => report::Row::Finished(outcome),
        }
    }
}

fn run_step(step: &Step, dir: &Path, on_line: &(dyn Fn(&str) + Sync)) -> Outcome {
    let started = Instant::now();
    let finished = execute(step, dir, on_line);
    outcome(step, started.elapsed(), finished)
}

/// A finished child process, before it is read as an outcome.
struct Finished {
    status: ExitStatus,
    output: String,
}

fn outcome(step: &Step, duration: Duration, finished: io::Result<Finished>) -> Outcome {
    let (status, output) = match finished {
        Ok(finished) if finished.status.success() => (Status::Done, finished.output),
        Ok(finished) => (
            Status::Failed {
                code: finished.status.code(),
            },
            finished.output,
        ),
        // A tool the project asks for and the machine does not have. Reported
        // rather than failed: it is a machine to install it on, not a run that
        // went wrong.
        Err(error) if error.kind() == io::ErrorKind::NotFound => (
            Status::Skipped {
                reason: format!("{} not found", step.program),
            },
            String::new(),
        ),
        Err(error) => (Status::Failed { code: None }, error.to_string()),
    };

    Outcome {
        name: step.name,
        status,
        output,
        duration,
    }
}

fn command(step: &Step, dir: &Path) -> Command {
    let mut command = Command::new(&step.program);
    command.args(&step.args).current_dir(dir);
    command
}

/// Run a step, handing `on_line` each line it writes on either stream as it
/// arrives and keeping every one of them, in that order, for a failure to show.
///
/// The streams are read on separate threads so a child that fills one pipe
/// does not block writing to the other. The child gets no stdin, so a tool that
/// prompts fails rather than waiting for an answer nobody is asked for.
fn execute(step: &Step, dir: &Path, on_line: &(dyn Fn(&str) + Sync)) -> io::Result<Finished> {
    let _child = stats::in_child();
    let mut child = command(step, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let output = Mutex::new(String::new());
    let record = |line: &str| {
        on_line(line);
        let mut output = output.lock().expect("the output buffer was poisoned");
        output.push_str(line);
        output.push('\n');
    };

    std::thread::scope(|scope| {
        scope.spawn(|| lines(stdout, &record));
        scope.spawn(|| lines(stderr, &record));
    });

    let output = output.into_inner().expect("the output buffer was poisoned");
    Ok(Finished {
        status: child.wait()?,
        output: output.trim_end().to_string(),
    })
}

/// Call `on_line` with each line of `source`, without its line ending. Bytes
/// that are not UTF-8 are replaced rather than dropped.
fn lines(source: impl Read, on_line: &dyn Fn(&str)) {
    let mut reader = BufReader::new(source);
    let mut line = Vec::new();

    while let Ok(read) = reader.read_until(b'\n', &mut line) {
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        on_line(text.trim_end_matches(['\n', '\r']));
        line.clear();
    }
}

/// Print what each failed step had to say, under a heading naming it. Written
/// as the command wrote it — colour included — since it is the same output the
/// user would have seen had they run it themselves.
fn details(outcomes: &[Outcome]) {
    for outcome in outcomes
        .iter()
        .filter(|outcome| outcome.failed() && !outcome.output.is_empty())
    {
        eprintln!();
        eprintln!("── {} ──", outcome.name);
        eprintln!("{}", outcome.output);
    }
}

/// Run a build step with the terminal handed to it, and pass its status on.
fn inherit(step: &Step, dir: &Path) -> Result<()> {
    let _child = stats::in_child();
    let status = command(step, dir)
        .status()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => anyhow!("`{}` was not found on PATH", step.program),
            _ => anyhow::Error::new(error).context(format!("running {}", step.command_line())),
        })?;

    if !status.success() {
        return Err(Reported(status.code().unwrap_or(1)).into());
    }
    Ok(())
}

/// Collect what detection and the dependency listing read: every entry in the
/// project root, the nested places mise also keeps its config, and the text of
/// each manifest that is there. A manifest that cannot be read is treated as
/// absent — the toolchain is still detected by its name being on disk.
fn scan(dir: &Path) -> Result<Scan> {
    let mut paths = BTreeSet::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        if let Some(name) = entry?.file_name().to_str() {
            paths.insert(name.to_string());
        }
    }
    for config in MISE_CONFIGS.iter().filter(|path| path.contains('/')) {
        if dir.join(config).is_file() {
            paths.insert((*config).to_string());
        }
    }

    let modules: Vec<String> = paths
        .iter()
        .filter(|path| path.ends_with(".tf"))
        .cloned()
        .collect();
    let mut contents = BTreeMap::new();
    for name in MANIFESTS
        .iter()
        .map(|name| (*name).to_string())
        .chain(modules)
    {
        if !paths.contains(&name) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(dir.join(&name)) {
            contents.insert(name, text);
        }
    }

    Ok(Scan { paths, contents })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(source: &[u8]) -> Vec<String> {
        let seen = std::cell::RefCell::new(Vec::new());
        lines(source, &|line| seen.borrow_mut().push(line.to_string()));
        seen.into_inner()
    }

    #[test]
    fn lines_are_reported_without_their_endings() {
        assert_eq!(collect(b"one\r\ntwo\n\nthree"), ["one", "two", "", "three"]);
    }

    #[test]
    fn an_empty_stream_reports_nothing() {
        assert!(collect(b"").is_empty());
    }

    #[test]
    fn invalid_utf8_stays_readable() {
        assert_eq!(collect(&[0xff, b'a', b'\n']), ["\u{fffd}a"]);
    }

    #[test]
    fn a_directory_is_scanned_for_the_manifests_that_are_in_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.path().join("README.md"), "unread").unwrap();
        std::fs::create_dir_all(dir.path().join(".config/mise")).unwrap();
        std::fs::write(dir.path().join(".config/mise/config.toml"), "[tools]").unwrap();

        let scan = scan(dir.path()).unwrap();

        assert!(scan.has("Cargo.toml"));
        assert!(scan.has(".config/mise/config.toml"), "{:?}", scan.paths);
        assert_eq!(scan.text("Cargo.toml"), Some("[package]"));
        assert_eq!(scan.text("README.md"), None, "read a file it never needs");
    }

    #[test]
    fn every_terraform_module_in_the_root_is_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.tf"), "resource {}").unwrap();
        std::fs::write(dir.path().join("versions.tf"), "terraform {}").unwrap();

        assert_eq!(
            scan(dir.path()).unwrap().terraform(),
            "resource {}\nterraform {}"
        );
    }
}
