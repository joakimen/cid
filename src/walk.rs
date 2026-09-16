//! Walking a directory tree for files to offer in a selector.
//!
//! Built on the [`ignore`] crate — the same walker [fd] is — so `.gitignore`,
//! `.ignore` and `.fdignore` are honoured in-process, with no `fd` subprocess
//! to find on `PATH`.
//!
//! [fd]: https://github.com/sharkdp/fd

use std::path::Path;
use std::sync::mpsc::sync_channel;

use ignore::{WalkBuilder, WalkState};

/// Directory names never worth walking into. `.gitignore` covers most build
/// output; these are the directories that are commonly *not* ignored yet never
/// worth offering.
///
/// Every name here must be one that never holds source. `lib` is not such a
/// name: Clojure, Elixir, Ruby and much of npm keep their source there, and
/// skipping it hides those files from `cid edit`.
const WALKER_SKIP: &[&str] = &[".git", "node_modules", ".clj-kondo", ".cpcache", ".venv"];

/// How many discovered paths may sit between the walk and whatever is reading
/// it. Bounded so a full queue blocks the walker threads, which is what keeps
/// the footprint flat however large the tree.
const QUEUE_BOUND: usize = 1024;

/// Files under `root`, as paths relative to `root`.
///
/// Streamed as they are found, so a selector opens on the first filename rather
/// than the last. Dotfiles are included; ignore files and [`WALKER_SKIP`] are
/// respected.
///
/// **The order is not specified** — the walk runs on several threads.
/// **Unreadable paths are skipped, not reported**, as `find … 2>/dev/null`
/// does: a walk of `$HOME` on macOS crosses directories only an app with Full
/// Disk Access can read.
pub fn files(root: &Path) -> impl Iterator<Item = String> + Send + 'static {
    walk(root, Kind::File)
}

/// Directories under `root`, as paths relative to `root`. [`files`] in every
/// other respect. `root` itself is not offered.
pub fn dirs(root: &Path) -> impl Iterator<Item = String> + Send + 'static {
    walk(root, Kind::Dir)
}

/// Which entries a walk yields.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

impl Kind {
    fn matches(self, entry: &ignore::DirEntry) -> bool {
        entry.file_type().is_some_and(|ft| match self {
            Self::File => ft.is_file(),
            Self::Dir => ft.is_dir(),
        })
    }
}

fn walk(root: &Path, kind: Kind) -> impl Iterator<Item = String> + Send + 'static {
    let root = root.to_path_buf();
    let (tx, rx) = sync_channel::<String>(QUEUE_BOUND);

    let walker = WalkBuilder::new(&root)
        .hidden(false) // include dotfiles; config files are common targets
        .require_git(false) // honour .gitignore/.ignore even outside a git repo
        .add_custom_ignore_filename(".fdignore") // shared with fd, if the user has one
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .is_none_or(|name| !WALKER_SKIP.contains(&name))
        })
        .build_parallel();

    // Detached: dropping the receiver is how the walk is called off.
    std::thread::spawn(move || {
        walker.run(|| {
            let tx = tx.clone();
            let root = root.clone();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !kind.matches(&entry) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&root).unwrap_or(path);
                // The walk starts at `root`, which strips to nothing.
                if rel.as_os_str().is_empty() {
                    return WalkState::Continue;
                }
                match tx.send(rel.to_string_lossy().into_owned()) {
                    Ok(()) => WalkState::Continue,
                    // The receiver is gone; stop walking.
                    Err(_) => WalkState::Quit,
                }
            })
        });
    });

    rx.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn list(root: &Path) -> Vec<String> {
        files(root).collect()
    }

    #[test]
    fn files_walks_and_skips() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        // A skipped directory and a gitignored file must not appear.
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "").unwrap();

        let got = list(root);

        assert!(got.contains(&"a.txt".to_string()));
        assert!(got.contains(&"src/main.rs".to_string()));
        assert!(got.contains(&".gitignore".to_string())); // dotfiles included
        assert!(
            !got.iter().any(|f| f.contains("node_modules")),
            "WALKER_SKIP dir leaked: {got:?}"
        );
        assert!(
            !got.contains(&"ignored.txt".to_string()),
            ".gitignore not honoured: {got:?}"
        );
    }

    #[test]
    fn dirs_yields_directories_under_the_same_rules() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/cmd")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join(".gitignore"), "target\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();

        let got: Vec<String> = dirs(root).collect();

        assert!(got.contains(&"src".to_string()), "{got:?}");
        assert!(got.contains(&"src/cmd".to_string()), "{got:?}");
        assert!(got.contains(&"empty".to_string()), "{got:?}");
        assert!(
            !got.iter().any(|d| d.contains("node_modules")),
            "WALKER_SKIP dir leaked: {got:?}"
        );
        assert!(
            !got.iter().any(|d| d.starts_with("target")),
            ".gitignore not honoured: {got:?}"
        );
        assert!(
            !got.iter().any(|d| d.ends_with(".rs")),
            "a file reached the directory walk: {got:?}"
        );
    }

    #[test]
    fn the_root_itself_is_not_offered() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("inner")).unwrap();
        let got: Vec<String> = dirs(dir.path()).collect();
        assert_eq!(got, vec!["inner".to_string()]);
    }

    /// A skip list is invisible when it is wrong: nothing errors, the files
    /// simply are not there to open. `lib` holds source in several ecosystems.
    #[test]
    fn a_directory_that_holds_source_is_walked() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("lib/billing")).unwrap();
        fs::write(root.join("lib/billing/core.clj"), "").unwrap();

        assert!(
            list(root).contains(&"lib/billing/core.clj".to_string()),
            "a source directory is being skipped"
        );
    }

    #[test]
    fn files_honours_fdignore() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.txt"), "").unwrap();
        fs::write(root.join(".fdignore"), "drop.txt\n").unwrap();
        fs::write(root.join("drop.txt"), "").unwrap();

        let got = list(root);

        assert!(got.contains(&"keep.txt".to_string()));
        assert!(
            !got.contains(&"drop.txt".to_string()),
            ".fdignore not honoured: {got:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn files_skips_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("readable.txt"), "").unwrap();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.txt"), "").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let got = list(root);

        // Restore before the TempDir tries to remove it.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            got.contains(&"readable.txt".to_string()),
            "an unreadable sibling swallowed the walk: {got:?}"
        );
        assert!(!got.iter().any(|f| f.contains("hidden.txt")));
    }

    /// A tree several times [`QUEUE_BOUND`] only finishes if the walker threads
    /// block when the queue fills and resume as the consumer drains it. A
    /// mis-wired bounded queue deadlocks; an unbounded one passes every other
    /// test here.
    #[test]
    fn files_streams_a_tree_larger_than_its_queue() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let count = QUEUE_BOUND * 3;
        for i in 0..count {
            fs::write(root.join(format!("f{i}")), "").unwrap();
        }

        let mut walk = files(root);
        // A row is there for the taking well before the tree is exhausted.
        assert!(walk.next().is_some());
        assert_eq!(walk.count(), count - 1, "the walk lost or repeated rows");
    }
}
