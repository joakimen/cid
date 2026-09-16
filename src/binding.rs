//! What a shell can be told to do on a key or under a name.
//!
//! `cid init` writes shell code, and which code it writes is configuration:
//! `[shell.bindings]` maps a key to an action and `[shell.aliases]` maps a
//! name to one. Neither holds shell code — an action is a cid command plus
//! what the shell does with what it prints, and each shell's emitter knows how
//! to say that in its own language. That is what keeps one table serving every
//! shell cid learns to write for.
//!
//! I/O-free: [`resolve`] turns the configuration into the list an emitter
//! walks, and refuses a name no action answers to.

use anyhow::{Result, bail};

use crate::config::{Bindings, ShellConfig};
use crate::term;

/// What the shell does with the command an action runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Run it and let it write to the terminal.
    Run,
    /// Run it and `cd` to the path it prints. Only a shell can change its own
    /// directory, which is the whole reason this layer exists.
    Cd,
    /// Put what it prints on the command line instead of running it, with the
    /// line so far handed over as the query.
    Line,
    /// [`Kind::Line`], but only where the shell would have gone to history
    /// anyway — on the first line of a prompt, outside the pager.
    LineOrUp,
}

/// One thing a key or a name can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// What the configuration calls it. Stable: renaming one silently breaks
    /// every config that named it.
    pub id: &'static str,
    /// What the shell tells the user this is, where it has somewhere to say so
    /// — fish's `--description`.
    pub description: &'static str,
    /// The arguments handed to `cid`.
    pub args: &'static [&'static str],
    pub kind: Kind,
}

/// Everything that can be bound or aliased.
///
/// A verb belongs here when a shell can reach it faster than a typed command
/// can, which is either because it answers "which one" — a selector — or
/// because the shell has to act on the answer itself. Ordinary commands are
/// left to be typed.
pub const ACTIONS: &[Action] = &[
    Action {
        id: "repo-cd",
        description: "Select a repository and cd into it",
        args: &["repo", "sel"],
        kind: Kind::Cd,
    },
    Action {
        id: "worktree-cd",
        description: "Select a git worktree and cd into it",
        args: &["worktree", "sel"],
        kind: Kind::Cd,
    },
    Action {
        id: "repo-open",
        description: "Open this repository on GitHub, or select one",
        args: &["repo", "open"],
        kind: Kind::Run,
    },
    Action {
        id: "file-edit",
        description: "Select a tracked file and open it in $EDITOR",
        args: &["edit", "--tracked"],
        kind: Kind::Run,
    },
    Action {
        id: "edit",
        description: "Find a file, fuzzy-select it, open it in $EDITOR",
        args: &["edit"],
        kind: Kind::Run,
    },
    Action {
        id: "note-edit",
        description: "Find a note by name or by what it says, and open it",
        args: &["note", "open"],
        kind: Kind::Run,
    },
    Action {
        id: "branch-checkout",
        description: "Select a git branch and check it out",
        args: &["branch", "checkout"],
        kind: Kind::Run,
    },
    Action {
        id: "pr-checkout",
        description: "Select a GitHub pull request and check it out",
        args: &["pr", "checkout"],
        kind: Kind::Run,
    },
    Action {
        id: "pr-worktree-cd",
        description: "Select a pull request, check it out in a new worktree and cd there",
        args: &["pr", "checkout", "--worktree"],
        kind: Kind::Cd,
    },
    // The one pull request action that asks nothing: whatever this branch is,
    // it either has a pull request or it does not, and both are a page.
    Action {
        id: "pr-open",
        description: "Open this branch's pull request, or the list",
        args: &["pr", "open", "--current"],
        kind: Kind::Run,
    },
    Action {
        id: "proc-kill",
        description: "Fuzzy-select running processes and kill them",
        args: &["ps", "kill", "--force"],
        kind: Kind::Run,
    },
    Action {
        id: "project-deps",
        description: "Install this project's dependencies",
        args: &["project", "deps"],
        kind: Kind::Run,
    },
    Action {
        id: "project-build",
        description: "Build this project",
        args: &["project", "build"],
        kind: Kind::Run,
    },
    // The two whose result belongs on the command line, which only the shell
    // can write to. The emitter adds `--print0` and the query itself.
    Action {
        id: "history-select",
        description: "Fuzzy-select a past command onto the command line",
        args: &["history", "sel"],
        kind: Kind::Line,
    },
    Action {
        id: "history-up",
        description: "Search history, or move up within a multi-line command",
        args: &["history", "sel"],
        kind: Kind::LineOrUp,
    },
];

/// The key bindings the starter config offers, commented out.
///
/// Suggestions, not defaults: nothing is bound until the file says so. A key is
/// the scarcest thing a terminal has, and a tool that takes ten of them the
/// moment it is installed is a tool that has decided what the user's `ctrl-r`
/// is for.
///
/// What each one costs, since the file cannot say it at every line: ctrl-o and
/// ctrl-q are the only keys fish leaves free. ctrl-g displaces `cancel` (escape
/// and ctrl-c both still do that), ctrl-r `history-pager`, ctrl-t
/// `transpose-chars` — swapping the two characters around the cursor — and up
/// `up-line`, which `history-up` hands back wherever the selector would be
/// wrong. f1, f2, f3, f7 and f10 displace nothing; f4 and f5 are left alone
/// because users' own tools cluster there. ctrl-f7 displaces nothing in fish,
/// but macOS takes Control-F7 for "Change the way Tab moves focus" until that
/// shortcut is turned off under System Settings → Keyboard → Keyboard
/// Shortcuts → Keyboard.
pub const EXAMPLE_BINDINGS: &[(&str, &str)] = &[
    ("ctrl-o", "repo-cd"),
    ("ctrl-t", "worktree-cd"),
    ("f1", "repo-open"),
    ("f2", "pr-open"),
    ("f3", "file-edit"),
    ("ctrl-g", "branch-checkout"),
    ("f7", "pr-checkout"),
    ("ctrl-f7", "pr-worktree-cd"),
    ("f10", "note-edit"),
    ("ctrl-r", "history-select"),
    ("up", "history-up"),
];

/// The aliases the starter config offers, commented out like the bindings
/// above. Short because they are typed many times a day; unprefixed because
/// that is the point of them.
pub const EXAMPLE_ALIASES: &[(&str, &str)] = &[
    ("fe", "edit"),
    ("kl", "proc-kill"),
    ("i", "project-deps"),
    ("b", "project-build"),
];

/// One key or name, and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The key as the shell spells it, or the name the alias takes.
    pub trigger: String,
    pub action: &'static Action,
}

/// What an emitter walks: the bindings and aliases in the order they were
/// written, each already resolved to the action it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    pub bindings: Vec<Bound>,
    pub aliases: Vec<Bound>,
}

impl Integration {
    /// Every action reached by a binding, once each, in binding order. What an
    /// emitter has to define a function for.
    pub fn bound_actions(&self) -> Vec<&'static Action> {
        let mut seen: Vec<&'static Action> = Vec::new();
        for bound in &self.bindings {
            if !seen.iter().any(|action| action.id == bound.action.id) {
                seen.push(bound.action);
            }
        }
        seen
    }
}

/// The action `id` names.
pub fn action(id: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|action| action.id == id)
}

/// Resolve the configuration into what `cid init` will emit.
///
/// The tables are the whole of it: an absent one binds nothing, and a key left
/// out of a present one is a key cid does not touch. An action nobody defines
/// is an error rather than a line quietly left out — a shell where one key works
/// and another silently does not is worse than one that says why at the moment
/// it is sourced.
pub fn resolve(config: &ShellConfig) -> Result<Integration> {
    Ok(Integration {
        bindings: bind("binding", config.bindings.as_ref())?,
        aliases: bind("alias", config.aliases.as_ref())?,
    })
}

/// What a table names, in the order it was written.
///
/// Unlike [`resolve`], an action nobody defines is kept rather than refused:
/// `cid config print` reports a line the file really has, and leaves calling
/// it broken to `cid config check`.
pub fn entries(table: Option<&Bindings>) -> Vec<(&str, &str)> {
    table
        .into_iter()
        .flatten()
        .map(|(trigger, id)| (trigger.as_str(), id.as_str()))
        .collect()
}

/// One action, and every key and name the configuration gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment<'a> {
    pub action: &'static Action,
    /// Keys bound to it, in the order the table writes them.
    pub keys: Vec<&'a str>,
    /// Names defined for it, in the order the table writes them.
    pub aliases: Vec<&'a str>,
}

/// Every action there is, in catalogue order, with what the configuration
/// assigns to each. An action nothing names is listed all the same: finding
/// the ones that are still free is what the list is for.
///
/// A line naming an action cid does not define belongs to no row here;
/// `cid config check` is where that is reported.
pub fn assignments(config: &ShellConfig) -> Vec<Assignment<'_>> {
    let bindings = entries(config.bindings.as_ref());
    let aliases = entries(config.aliases.as_ref());
    ACTIONS
        .iter()
        .map(|action| Assignment {
            action,
            keys: naming(&bindings, action.id),
            aliases: naming(&aliases, action.id),
        })
        .collect()
}

/// The column headings of [`render_assignments`].
const HEADINGS: [&str; 5] = ["action", "key", "alias", "runs", "does"];

/// What an empty key or alias cell holds, so every row keeps one word per
/// column for `awk` and `cut` to count.
const NONE: &str = "-";

/// `cid config actions`, one line per action under a heading row.
///
/// Every column but the last two is a single word; several keys or names for
/// one action are joined by commas.
pub fn render_assignments(assignments: &[Assignment], color: bool) -> Vec<String> {
    let cells: Vec<[String; 5]> = assignments
        .iter()
        .map(|assignment| {
            [
                assignment.action.id.to_string(),
                joined(&assignment.keys),
                joined(&assignment.aliases),
                format!("cid {}", assignment.action.args.join(" ")),
                assignment.action.description.to_string(),
            ]
        })
        .collect();
    let width = |column: usize| {
        cells
            .iter()
            .map(|row| row[column].chars().count())
            .chain([HEADINGS[column].len()])
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2), width(3)];

    let heading = HEADINGS
        .iter()
        .enumerate()
        .map(|(column, text)| match widths.get(column) {
            Some(&width) => term::bold(&pad(text, width), color),
            None => term::bold(text, color),
        })
        .collect::<Vec<_>>()
        .join("  ");

    let rows = cells.iter().map(|[id, keys, aliases, runs, does]| {
        [
            pad(id, widths[0]),
            pad(keys, widths[1]),
            pad(aliases, widths[2]),
            term::paint(&pad(runs, widths[3]), term::SECONDARY, color),
            does.clone(),
        ]
        .join("  ")
    });

    std::iter::once(heading).chain(rows).collect()
}

/// The triggers in `table` that name `id`.
fn naming<'a>(table: &[(&'a str, &str)], id: &str) -> Vec<&'a str> {
    table
        .iter()
        .filter(|(_, named)| *named == id)
        .map(|(trigger, _)| *trigger)
        .collect()
}

fn joined(triggers: &[&str]) -> String {
    if triggers.is_empty() {
        NONE.to_string()
    } else {
        triggers.join(",")
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

fn bind(what: &str, table: Option<&Bindings>) -> Result<Vec<Bound>> {
    entries(table)
        .into_iter()
        .map(|(trigger, id)| match action(id) {
            Some(action) => Ok(Bound {
                trigger: trigger.to_string(),
                action,
            }),
            None => bail!(
                "the {what} `{trigger}` names `{id}`, which is not one of: {}",
                ACTIONS
                    .iter()
                    .map(|action| action.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> Bindings {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn triggers(bound: &[Bound]) -> Vec<&str> {
        bound.iter().map(|bound| bound.trigger.as_str()).collect()
    }

    /// Nothing is bound until the config says so: a key is the scarcest thing
    /// a terminal has, and cid takes none of them on its own.
    #[test]
    fn an_empty_configuration_binds_nothing() {
        let resolved = resolve(&ShellConfig::default()).unwrap();

        assert!(resolved.bindings.is_empty(), "{resolved:?}");
        assert!(resolved.aliases.is_empty(), "{resolved:?}");
    }

    /// The starter config offers them commented out, so a user who uncomments
    /// the block has to get a shell rather than an error.
    #[test]
    fn every_example_names_an_action_that_exists() {
        let config = ShellConfig {
            bindings: Some(table(EXAMPLE_BINDINGS)),
            aliases: Some(table(EXAMPLE_ALIASES)),
        };
        resolve(&config).expect("the examples do not resolve");
    }

    #[test]
    fn a_table_is_the_whole_of_what_is_bound() {
        let config = ShellConfig {
            bindings: Some(table(&[("f6", "repo-cd")])),
            aliases: Some(table(&[])),
        };
        let resolved = resolve(&config).unwrap();

        assert_eq!(triggers(&resolved.bindings), vec!["f6"]);
        assert!(resolved.aliases.is_empty(), "{resolved:?}");
    }

    #[test]
    fn the_order_a_table_is_written_in_is_the_order_it_is_emitted_in() {
        let config = ShellConfig {
            bindings: Some(table(&[
                ("f8", "edit"),
                ("ctrl-o", "repo-cd"),
                ("f6", "pr-open"),
            ])),
            ..ShellConfig::default()
        };

        assert_eq!(
            triggers(&resolve(&config).unwrap().bindings),
            vec!["f8", "ctrl-o", "f6"]
        );
    }

    #[test]
    fn an_action_nobody_defines_names_itself_and_the_ones_that_exist() {
        let config = ShellConfig {
            bindings: Some(table(&[("ctrl-o", "repo-jump")])),
            ..ShellConfig::default()
        };
        let error = resolve(&config).unwrap_err().to_string();

        assert!(error.contains("ctrl-o"), "{error}");
        assert!(error.contains("repo-jump"), "{error}");
        assert!(error.contains("repo-cd"), "{error}");
    }

    #[test]
    fn an_alias_is_refused_by_the_same_rule_a_binding_is() {
        let config = ShellConfig {
            aliases: Some(table(&[("x", "nope")])),
            ..ShellConfig::default()
        };
        let error = resolve(&config).unwrap_err().to_string();

        assert!(error.contains("alias `x`"), "{error}");
    }

    #[test]
    fn one_action_bound_to_two_keys_is_defined_once() {
        let config = ShellConfig {
            bindings: Some(table(&[
                ("f6", "repo-cd"),
                ("f8", "repo-cd"),
                ("f9", "edit"),
            ])),
            ..ShellConfig::default()
        };
        let actions = resolve(&config).unwrap().bound_actions();

        assert_eq!(
            actions.iter().map(|action| action.id).collect::<Vec<_>>(),
            vec!["repo-cd", "edit"]
        );
    }

    /// The list is for finding what is still free, so an action nothing names
    /// is as much a row as one that is bound.
    #[test]
    fn every_action_is_listed_whether_or_not_anything_names_it() {
        let config = ShellConfig::default();
        let listed = assignments(&config);

        assert_eq!(
            listed.iter().map(|row| row.action.id).collect::<Vec<_>>(),
            ACTIONS.iter().map(|action| action.id).collect::<Vec<_>>()
        );
        assert!(
            listed
                .iter()
                .all(|row| row.keys.is_empty() && row.aliases.is_empty()),
            "{listed:?}"
        );
    }

    #[test]
    fn keys_and_aliases_land_on_the_action_they_name_in_the_order_written() {
        let config = ShellConfig {
            bindings: Some(table(&[
                ("f8", "repo-cd"),
                ("f1", "pr-open"),
                ("ctrl-o", "repo-cd"),
            ])),
            aliases: Some(table(&[("kl", "proc-kill")])),
        };
        let listed = assignments(&config);
        let row = |id: &str| listed.iter().find(|row| row.action.id == id).unwrap();

        assert_eq!(row("repo-cd").keys, vec!["f8", "ctrl-o"]);
        assert_eq!(row("pr-open").keys, vec!["f1"]);
        assert_eq!(row("proc-kill").aliases, vec!["kl"]);
        assert!(row("proc-kill").keys.is_empty());
    }

    /// Reporting a line that names nothing is `config check`'s job; here it
    /// would be a row for an action that does not exist.
    #[test]
    fn a_line_naming_no_action_adds_no_row() {
        let config = ShellConfig {
            bindings: Some(table(&[("ctrl-o", "repo-jump")])),
            ..ShellConfig::default()
        };
        let listed = assignments(&config);

        assert_eq!(listed.len(), ACTIONS.len());
        assert!(listed.iter().all(|row| row.keys.is_empty()), "{listed:?}");
    }

    #[test]
    fn a_rendered_row_keeps_one_word_per_column_up_to_the_command() {
        let config = ShellConfig {
            bindings: Some(table(&[("ctrl-o", "repo-cd"), ("f6", "repo-cd")])),
            aliases: Some(table(&[("kl", "proc-kill")])),
        };
        let lines = render_assignments(&assignments(&config), false);
        let words = |prefix: &str| -> Vec<String> {
            let line = lines.iter().find(|line| line.starts_with(prefix)).unwrap();
            line.split_whitespace().map(str::to_string).collect()
        };

        assert_eq!(lines.len(), ACTIONS.len() + 1);
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            vec!["action", "key", "alias", "runs", "does"]
        );
        assert_eq!(
            words("repo-cd ")[..5],
            ["repo-cd", "ctrl-o,f6", "-", "cid", "repo"]
        );
        assert_eq!(words("proc-kill ")[..4], ["proc-kill", "-", "kl", "cid"]);
    }

    #[test]
    fn a_listing_without_colour_carries_no_escape_sequences() {
        for line in render_assignments(&assignments(&ShellConfig::default()), false) {
            assert!(!line.contains('\x1b'), "{line:?}");
        }
    }

    #[test]
    fn no_two_actions_share_an_id() {
        let mut ids: Vec<&str> = ACTIONS.iter().map(|action| action.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "two actions answer to one name");
    }

    #[test]
    fn every_action_runs_a_command() {
        for action in ACTIONS {
            assert!(!action.args.is_empty(), "{} runs nothing", action.id);
            assert!(
                !action.description.is_empty(),
                "{} says nothing about itself",
                action.id
            );
        }
    }
}
