# Changelog

What changed in each release, newest first. Versions are [semantic
versioning](https://semver.org): while cid is `0.x`, a new command or flag is
a minor and a fix is a patch.

The entry for a version is what its GitHub release says, so it is written for
someone deciding whether to upgrade, not for someone reading the diff. Its
heading is that release's title, which is why the date sits on a line below
instead of in it: GitHub prints the date beside the title already. Both lines
are written by `.github/date-changelog.sh` when the release pull request is
raised, never by hand.

## Unreleased

## v0.24.1

*Released 2026-09-25*

### Fixed

- **`cid repo clone <owner>`** lists up to `--limit` repositories that are not
  archived. It used to stop after `--limit` rows including the archived ones
  it then hid, so an org with many archived repositories showed only part of
  the rest, and gave no note that the list had been cut short.

## v0.24.0

*Released 2026-09-23*

### Added

- **`cid note daily`** opens today's note, `daily/2026-09-23.md`, starting it
  with a `# Daily - 2026-09-23` heading the first time it runs that day.
  `[note] daily` moves the folder, and the `note-daily` action binds it —
  the starter config suggests `ctrl-f10`. `note cleanup` no longer offers a
  daily note just because its name is a date.

### Changed

- **`cid note new`** drops the date from the filename: `Migrate to AWS` is now
  `inbox/migrate-to-aws.md`. Notes go under `[note] inbox`, `inbox` unless set,
  and the date moves into a `created:` line in the note's front matter, which
  `note ls` and the selector already read.
- **`cid config check` is now `cid doctor`.** The report is unchanged; the
  old spelling is gone, so a setup script that runs it needs the new name.

## v0.23.0

*Released 2026-09-23*

### Changed

- **`cid note new`** takes a title instead of a filename. Without one it asks
  for it, showing the filename it will make as you type. The note is created
  at the top of the vault as `2026-09-23-migrate-to-aws.md`, starting with
  the title as its heading, and opened in `[note] editor`. A name no longer
  picks a directory or an extension, and running it with no argument outside
  a terminal is now an error rather than a note named for the time.

## v0.22.1

*Released 2026-09-18*

### Changed

- **`cid project deps`** draws the whole install plan before anything runs,
  with the steps that wait for `mise install` indented under it, and each step
  shows a running clock and the latest line its command printed. A long
  `mise install` now visibly makes progress instead of sitting on a blank
  screen. The output kept for a failed step is in the order the command wrote
  it, and `--verbose` lists what is waiting on mise up front and says when a
  step was skipped.

## v0.22.0

*Released 2026-09-16*

### Added

- **`cid pr checkout --worktree`** (`-w`) checks a pull request out in a
  worktree of its own, where `cid worktree add` would put one, and prints its
  path — so reviewing a pull request no longer means switching the tree you
  are working in. If a worktree already has the branch, you get that one.
  The new `pr-worktree-cd` action binds it and `cd`s there; the starter config
  suggests `ctrl-f7`, and `ctrl-f7` does the same inside every pull request
  selector. macOS claims Control-F7 for "Change the way Tab moves focus" by
  default: turn that off under System Settings → Keyboard → Keyboard Shortcuts
  → Keyboard before binding it.
- **`cid branch prune`** deletes every local branch whose remote branch has
  been deleted — which is what merging a pull request does on a repository
  that deletes head branches. It lists them and asks first; `--fetch` looks at
  the remotes again before deciding, and a branch a worktree has checked out
  is kept.
- **`cid worktree rm --branch`** deletes the branch each removed tree had
  checked out, so finishing a piece of work is one command and one question.
  The list shown before the question names each branch and whether it has
  landed, and the removal selector now marks every tree the same way.

### Changed

- A branch whose remote branch is gone is no longer shown as `local`, as if it
  had never been pushed. `cid branch ls --status` tags it `gone` and tints it
  blue, and `cid branch rm` marks it `upstream gone` rather than `not merged`
  — the mark every squash-merged branch used to carry.

## v0.21.0

*Released 2026-09-16*

### Added

- **`cid config actions`** lists every action `[shell.bindings]` and
  `[shell.aliases]` can name, with the keys and aliases your configuration
  gives each, the command it runs and what it does. Actions nothing is bound to
  are listed too, so finding a free one no longer means reading the source.

## v0.20.0

*Released 2026-09-15*

### Changed

- **scriv is now cid.** The binary, the crate and the repository are renamed;
  the commands, their verbs and their flags are not. `cid` is the engineer who
  builds the airships in Final Fantasy, which is nearer what this does than a
  name meaning a scribe ever was.

  Upgrading is four steps, none of them automatic:

  ```sh
  mv ~/.config/scriv ~/.config/cid
  mv ~/.local/share/scriv ~/.local/share/cid
  rm -f ~/.local/bin/scriv ~/.cargo/bin/scriv
  ```

  Then change `scriv init fish | source` to `cid init fish | source` in your
  fish config and start a new shell. The emitted functions are renamed with the
  binary — `scriv-repo-cd` is `cid-repo-cd`, `scriv_key_bindings` is
  `cid_key_bindings` — so a config.fish that calls the old names by hand needs
  the same edit.

  Your configuration file itself needs no edits. `[shell.bindings]` and
  `[shell.aliases]` name actions, not functions, and the action ids are
  unchanged.

  The environment variables move with the name: `SCRIV_CONFIG`, `SCRIV_NO_COLOR`
  and `SCRIV_BIN_DIR` are now `CID_CONFIG`, `CID_NO_COLOR` and `CID_BIN_DIR`.
  The old names are not read.

  Install lines change to `joakimen/cid`. GitHub redirects the old repository
  URL, so an existing clone keeps working, but `mise use -g github:joakimen/cid`
  is what resolves new releases.

## v0.19.4

*Released 2026-09-15*

### Changed

- Opening a pull request in the browser is about half a second quicker. scriv
  asked GitHub for the pull request before handing the browser a URL it had
  already worked out, and waited for the answer; it now builds the URL from the
  checkout's git remotes, as it already did for `scriv repo open`. `scriv pr
  open`, f2 from the prompt, and f2 inside the pull request selector are all
  affected.

## v0.19.3

*Released 2026-09-14*

### Fixed

- Ctrl-L in a selector no longer hangs for two seconds and then kills the run
  with "the cursor position could not be read". The key redrew the screen by
  asking the terminal where the cursor was, down a channel that a selector's
  answer is being read out of, so nothing ever replied. It does nothing at all
  now.

## v0.19.2

*Released 2026-09-10*

### Changed

- A preview pane showing only part of something longer now says so on its last
  row, where the text runs out and the eye is already looking. The mark sat on
  the first line before — a note at the top of a page saying there is a second
  one — and the pane went on past the bottom edge for anyone who knew which key
  scrolled it. It ends at the mark now.

## v0.19.1

*Released 2026-09-10*

### Changed

- A preview pane that is showing only the first screen of something longer now
  says so on its first line. The pane gave no sign that a file, a log or a pull
  request description ran past its bottom edge — it looked the same whether it
  ended there or a thousand lines below — and the key that scrolls it, `shift-↓`,
  was named nowhere.

## v0.19.0

*Released 2026-09-07*

### Added

- `[note] archives` names the directories in your vault holding notes you keep
  but no longer read — `archives = ["work/archive", "personal/archive"]`, as
  paths below `[note] root`. They are left out of `note ls`, `note sel`,
  `note cleanup` and the `note open` selector, text search included, so a vault
  that has been going for years lists what you are still working on. `--all`
  puts them back, and naming a note outright opens it either way.

### Changed

- The note selector names each note's group in a column of its own, the way the
  repository selector names a label. `[note] labels` was readable only as the
  row's colour, which two labels of similar hue and `--color never` both take
  away; the label is now written out, and a directory carrying none names
  itself. The column is not searched, so typing `work` still looks for a
  note rather than a directory.
- scriv no longer binds a key or defines an alias unless you have asked it to.
  `[shell.bindings]` and `[shell.aliases]` were defaults that applied when the
  tables were absent; they are now suggestions the starter config writes out
  commented. A key is the scarcest thing a terminal has, and which of yours a
  tool may take is yours to say. **If you never wrote those tables, uncomment
  them in `scriv config init`'s output to get your keys back** — `scriv config
  print` shows what is bound.
- The README is a page again rather than a manual: what scriv is, how to
  install it, the command groups, and how to bind a key. Verbs live in
  `scriv <command> --help` and settings in the generated `config.toml`.
- `note edit` and `note rg` are now one command, `scriv note open`. The selector
  opens on every note in the vault and three keys say what the query is read as:
  `ctrl-e` matches the names, `ctrl-r` searches inside every note as you type,
  and `ctrl-f` searches for the query exactly. Which of the two you want is
  rarely clear before the looking has started, and it no longer has to be:
  switching costs a keystroke rather than a second command. The mode in force is
  named in the header, and what you have typed is read again in whichever mode
  you switch to — a key is a second opinion on the same words.
- Without `ripgrep`, `note open` offers the names and nothing else, where
  `note rg` used to refuse to open at all.
- `scriv note open NAME` opens named notes without a selector, as `note edit`
  did. The query `note rg` took on the command line is gone — type it in the
  selector instead.

## v0.18.0

*Released 2026-09-02*

### Added

- `stats show` has an `own` column beside the average: how much of a run was
  scriv's own work, with the subprocesses it waited on — `git`, `gh`, a build
  tool — taken out. A command that is only slow because of what it shells out to
  now says so instead of looking like scriv being slow. `stats improve` hands
  that column on with the rest.

### Changed

- `stats improve` ranks the commands it hands over by the time scriv itself
  spent, not by the wall clock. A build that waits on cargo and an editor
  session used to head the list, where there was nothing for scriv to speed up.

### Fixed

- Time spent in the editor `scriv edit` opens is no longer counted as time the
  command took, as time in a selector already was not. An afternoon in vim used
  to make `edit` the most expensive command scriv had.

## v0.17.2

*Released 2026-09-02*

### Changed

- `ctrl-t` in the `repo clone` selector steps through the two lists on its own,
  and `ctrl-a` is no longer bound: one key is all it needs, and that one was
  spoken for elsewhere.

### Fixed

- Opening a repository on GitHub — `repo open`, or `f1` in the `repo sel`
  selector — no longer waits on a call to the GitHub API before the browser
  opens. The same page, roughly half a second sooner.

## v0.17.1

*Released 2026-09-02*

### Added

- `ctrl-t` in the `repo clone` selector shows only the repositories you have not
  cloned yet, and `ctrl-a` puts the whole list back.

### Fixed

- The `repo clone` and `repo sel` selectors match what you type against the
  repository alone. A query used to reach the description, the tags and the
  owner label as well, so a word in a description matched a row it had nothing
  to do with — and the row was then scrolled sideways to show where it hit,
  pushing the repository's own name off the screen.
- `repo clone` says `loading repositories for <owner>` while it waits. It used
  to count pages off as they arrived; they are fetched together and arrive in
  whatever order they finish, so the count went up and down and read as a bug.

## v0.17.0

*Released 2026-09-02*

### Added

- `scriv stats` counts scriv itself. Every run appends its command and how long
  it took to a log, and `stats show` reads that back as a tree of every command
  there is — with how often you have run each one and what a run of it costs —
  so the commands going unused are as visible as the ones that are not. The
  time you spend in a selector is taken out of the numbers: a list left open
  over lunch is not a slow command. `stats improve` hands the busiest commands
  to Claude Code to work on, and `stats reset` forgets the lot.

  Nothing but the command's name is recorded: no arguments, no paths, no what
  you picked, and `stats reset` does not record itself. The log is
  `$XDG_DATA_HOME/scriv/stats`.

### Changed

- `scriv proc` is `scriv ps`, after the tool it reads the process table with,
  and the `pc` alias is gone with it: the name is two letters already. `scriv
  proc` is a usage error now, so a shell function or a note that spelled it
  needs the new name. The `proc-kill` action a key or an alias can be bound to
  keeps its name, since renaming one silently breaks every config that named it.
- `scriv repo clone <owner>` lists an owner's repositories about three times
  faster — a second rather than three for an org of four hundred — and says
  what it is waiting for while it does. The listing is read a page at a time
  with the pages fetched together, which GitHub's REST API allows and the
  GraphQL one `gh repo list` uses does not. Asking for the owner instead of
  naming one says so too, rather than sitting silent through two round trips.

### Fixed

- Nothing scriv prints is bright black any more. `ps`, `project deps`,
  `project build` and every selector row drew what they say about a thing —
  durations, counts, the columns before a process's command — in the colour
  several dark themes use as their background, and the rest render as the
  faintest thing on the row. They now take the same hue `config print` does.

## v0.16.2

*Released 2026-09-02*

### Fixed

- `scriv config print` and `scriv config check` no longer draw half their output
  in bright black. It is the background colour on a good share of dark themes
  and barely-there grey on the rest, so the keys, the notes beside a value and
  everything a passing check found were hard to read. Field names are bold now,
  and the text beside them takes a colour the terminal resolves from its own
  theme.

## v0.16.1

*Released 2026-09-01*

### Changed

- `scriv config print` reports every setting there is, including the keys and
  names `scriv init` binds and the action each one runs. It had drifted to a
  handful of them, so a table added since — `[worktree]`, `[selector]`,
  `[shell]` — could not be read back from the tool that claims to print the
  configuration. Values scriv chose rather than you are marked as defaults.
- `scriv config check` is a checklist rather than a second report of the
  configuration. Rows are grouped, a working one is a tick rather than a line
  of green prose, and the run ends with what is left to look at. What a setting
  is set to now belongs to `config print` alone.

## v0.16.0

*Released 2026-08-28*

### Changed

- `-v` is `--version`. It was `--verbose`, which now has only its long form, and
  clap's own `-V` is gone. A `-v` meaning verbosity is a usage error rather than
  a run that quietly stopped being verbose.

## v0.15.0

*Released 2026-08-28*

### Added

- `b` runs `scriv project build`, beside `fe`, `kl` and `i`.
- `[shell.bindings]` and `[shell.aliases]` decide what `scriv init` emits. A
  binding maps a key to an action and an alias maps a name to one; neither holds
  shell code, so the same configuration serves any shell scriv learns to write
  for. `scriv config init` writes the defaults out commented, and `scriv config
  check` gains a row saying whether yours resolve.

### Changed

- `scriv init` now reads the configuration, so a config file that will not parse
  — or that names an action scriv does not define — stops it. It refuses to emit
  anything rather than emit a shell where one key silently does nothing.

## v0.14.0

*Released 2026-08-28*

### Added

- `scriv project` builds and installs the directory you are standing in without
  being told what is in it. `scriv project deps` works out which toolchains the
  project uses from the files in its root — mise, Cargo, go, npm/bun/pnpm/yarn,
  Deno, Clojure, Babashka, Maven, Gradle, uv and Terraform — and runs each one's
  install, `mise install` first and the rest at once with their output held back
  unless one fails. A tool the machine does not have is a skip, not a failure.
  The fish integration binds this to `i`.
- `scriv project deps --dump` reads the same manifests for what they declare
  rather than installing anything, grouped by the role each dependency is given:
  npm's `devDependencies`, Cargo's `[build-dependencies]`, a Maven scope, a
  Gradle configuration, a Clojure alias, go's indirect requirements.
- `scriv project build` runs the repository's own `task`, `make` or `just` where
  it has one, and builds each detected toolchain in turn where it has none. Two
  task runners is an error rather than a guess.

## v0.13.0

*Released 2026-08-27*

### Changed

- A branch listing leads with the default branch rather than the one you are
  standing on. From a feature branch, `scriv branch checkout` now opens with
  `main` — or whatever `origin/HEAD` says the default is — already under the
  cursor, so getting back is enter; the branch you are leaving is the row below
  it, where selecting it did nothing anyway. Standing on the default branch
  leaves the order as it was. A clone with no `origin/HEAD` — a repository
  started with `git init` and pushed — falls back to a branch named `main`, then
  `master`.

## v0.12.0

*Released 2026-08-26*

### Added

- `[selector] preview_theme` picks the `bat` theme every file preview is drawn
  with, and defaults to Catppuccin Mocha. It is passed as `--theme`, so it wins
  over `BAT_THEME` and your own `bat` config — a preview pane is scriv's to make
  legible, and a theme chosen for reading whole files in a pager is not always
  one. A theme your `bat` does not know draws in its default instead of failing.
  `config check` now reports whether `bat` is installed at all.

### Changed

- A note preview shows the note. It used to show a summary scriv had assembled
  — the front matter read out, the body redrawn with headings and tags coloured
  in — and a preview that has rearranged what it is previewing is a preview of
  something else. Every note pane is now the file itself, through the same
  `bat` every other preview in scriv goes through, and a `note rg` hit opens at
  its line with that line marked.
- `note cleanup` reads down its list. Candidates are grouped by what is wrong
  with them — the empty ones first, then the untitled, then the ones with no
  name — and smallest first within each group, since size is what separates a
  note that was abandoned from one that was written and never titled. Sizes are
  right-aligned in one letter, `b`/`k`/`M`, so the units stack into a column
  instead of wandering with the length of each number.
- `note cleanup` never offers the scratch note. Being empty is what a scratch
  note is *for*, so it would have been on the list every single run.

## v0.11.1

*Released 2026-08-25*

### Fixed

- `scriv note cleanup` crashed on a vault whose note names are not all English.
  It compared the first eight *bytes* of a name against `untitled`, and the
  eighth byte of a name like `Oppgaveøkt` is the middle of the `ø` rather than
  the end of a letter — which Rust refuses to slice at, taking the whole
  command down with it. Names are now read a character at a time.

## v0.11.0

*Released 2026-08-25*

### Added

- `scriv note scratch` opens the one permanent note that is filed nowhere —
  somewhere to put a thought without first deciding whether it is worth a note
  of its own, and somewhere to find it again afterwards. The same file every
  time: `[note] scratch`, or `scratch/scratch.md`.
- `scriv note cleanup` goes through the notes that were never really written
  and deletes the ones you agree about. Three kinds and no more: a note with
  nothing in it, one still called `Untitled`, and one whose name has no letters
  in it and whose front matter gives it no title either. Each row says which it
  is, the preview shows what is in it, `tab` takes several, and what you chose
  is printed before you are asked — nothing goes without being seen first.
- `note rg` now matches fuzzily: the letters you typed, in order, anywhere on
  the line, so `errhand` finds "error handling". `ctrl-x` searches for the query
  exactly instead — for a phrase, a path, a snippet of code — and `ctrl-f` goes
  back. The header says which is in force.

### Changed

- A note row is a date and a name again. The day it was created leads it, in
  its own colour and never searched, and the name follows tinted by the label
  its directory carries. The columns of tags, folders and task counts are gone
  from the row and remain in the preview pane, which has the width for them.
- `scriv note ls` prints absolute paths, one per line, so the listing pipes
  into whatever reads paths. `--absolute-paths` is gone with nothing left to
  do, and `--status` — the listing a person reads rather than a pipe — collapses
  your home directory to `~` instead of repeating it down every row.
- `scriv note edit` warns when the note you named is not there, rather than
  letting the editor open a new empty buffer and say nothing about it.

### Fixed

- The preview pane no longer keeps the last match on screen after the query
  stops matching anything. It described a note that was no longer in the list.

## v0.10.0

*Released 2026-08-25*

### Added

- `scriv note new` starts a note and drops you straight into your editor. It
  asks nothing first — being asked to name a note is being asked what it is
  about before writing it — so with no name it calls the file after the date and
  time, to the minute. A name with a `/` in it makes the directory. The file
  itself is left for the editor to write, so a note started and abandoned is one
  that never existed rather than an empty one in every listing after it.
- `scriv note rg` searches inside every note as you type. The query goes to
  ripgrep rather than to the fuzzy matcher, so the list is every matching *line*
  in the vault, rebuilt on each keystroke, with the note around the match in the
  preview pane. `tab` takes several; what you pick opens at its line and the
  rest land in the quickfix list behind it. `ctrl-q` switches to filtering what
  came back.
- `[note] labels` names the directories directly below your vault, one label to
  many directories — the same shape as `[repo] labels`, and the same colours, so
  `work` reads the same in both. Label two of five directories and the other
  three still show up, under their own names.

### Changed

- A note row is laid out differently. The note's own name comes first, where it
  is what the eye runs down and what the query matches, and everything else is
  an attribute in a column of its own with a colour that says which: the label
  or directory it is filed under, the folders below that, its tags, how many of
  its tasks are still open, and — no longer leading the row — how long ago it
  was modified and created. The dates and the task count are drawn but never
  searched, so typing `3` finds the note you meant rather than every note that
  is three days old. A column nothing in your vault fills is not drawn at all.
- The preview pane's header now names the label and spells out how many tasks a
  note has left.

## v0.9.0

*Released 2026-08-25*

### Added

- `scriv note` reads a directory of Markdown files — an Obsidian vault, or any
  tree of notes — as one more thing to select from. `ls` prints them,
  `sel` prints the path of the one you pick, and `edit` opens what you pick,
  several at a time on `tab`. Point `[note] root` at the vault to turn it on.
  fish binds `f10` to `note edit`.
- A note list is ordered by what you touched last, and each row carries what
  the note calls itself, the folder it is filed under and its tags, behind two
  dim columns saying how long ago it was modified and created. Titles, tags and
  creation dates are read from YAML front matter, which is also why a synced or
  freshly cloned vault still shows the dates you wrote rather than the afternoon
  the files arrived. Inline `#tags` in the body are not indexed.
- The preview pane for a note is drawn by scriv rather than by `bat`: the
  header spells out both dates and the tags, and the body arrives with its
  headings, quotes, lists, task boxes, wikilinks and tags coloured, with the
  front matter left out since the header has already said it. Nothing is
  spawned, so it keeps up with a held-down arrow key through a vault of
  thousands.
- `[note] editor` chooses what opens a note, since that is as often a reader as
  an editor — `glow` and `nvim` are both answers. Unset, it is `$VISUAL` then
  `$EDITOR`, as `scriv edit` uses. `scriv config check` reports the vault, how
  many notes are in it, and whether that editor is on `PATH`.

## v0.8.0

*Released 2026-08-17*

### Added

- A selector now says what it can do, in a line under the prompt, and can do
  more than one thing. In any pull request list, f2 opens the highlighted one
  in the browser and f7 checks it out — the same keys that do those things from
  the prompt — so the verb you meant is a key rather than another command and
  another search. In a repository list, f1 opens it on GitHub.
- ctrl-v hides and shows the preview pane in every selector that has one, which
  is what a row too wide to read wants.
- The repositories and files you actually work in are offered first.
  `repo sel`, `repo open`, `file sel` and `edit --tracked` now lead with what
  you have chosen before — frequently and recently both count — instead of
  making you type your way past a hundred and ninety-five repositories to reach
  the five you are living in. Everything unchosen keeps its old order below.
  `[selector] recent = false` turns it off and stops the choices being
  recorded; `ls` is unaffected, so anything piping it sees no change. The
  record lives in `recent`, beside your config.
- `scriv worktree add` creates a working tree and picks where it goes:
  `.worktrees/<branch>` inside the repository, or wherever `[worktree] root`
  says. A branch that does not exist yet is created, a remote-only one arrives
  tracking its remote, and the path is printed — `cd (scriv worktree add
  feat/x)` lands in the new tree. A root inside the repository is added to that
  clone's `.git/info/exclude`, so nothing else offers the tree twice.
- `scriv worktree rm` removes trees, several at a time. Neither the main tree
  nor the one you are standing in is offered.
- `scriv branch rm` deletes local branches, several at a time, each listed with
  whether git can see its commits have landed. Answering the question is what
  lets an unmerged branch go — a repository that squashes its merges has no
  other kind. Remote branches are never offered: deleting one is a push.
- `scriv proc --port 3000` narrows `ls`, `sel` and `kill` to what is listening
  on a TCP port, so "what is holding 3000" no longer means reading a pid out of
  `lsof`. `kill --port` opens no selector — a port names its processes as
  precisely as a pid does — and prints what it found as it signals them.

### Changed

- `scriv history` no longer offers scriv's own key bindings back. Pressing
  ctrl-o records `scriv-repo-cd` in fish's history, so the rows at the top of
  ctrl-r were the keys you had just pressed rather than anything you typed.
- `scriv pr` says it is not in a git repository itself, rather than passing on
  `gh`'s report of git's `fatal: not a git repository`. `GH_REPO` still names a
  repository to work on without one.
- `scriv pr` says when a listing stopped at `--limit`, as `repo clone` already
  did, so a missing pull request is not mistaken for one that is not there.
- A missing `root` now points at the config file to edit when there is one, and
  only sends you to `scriv config init` when there is not — that command refuses
  to overwrite a config, so the old advice was a dead end.
- `scriv config check` reports whether `gh` is still logged in. An expired token
  breaks `pr` as completely as a missing `gh` does, and looked like nothing at
  all. It is the one check that waits on the network.

## v0.7.1

*Released 2026-08-17*

### Changed

- Listings write in blocks rather than a line at a time, which halves
  `scriv history ls` over a long fish history.
- `scriv worktree` asks git one question fewer, taking about a quarter off how
  long ctrl-t takes to open.
- `scriv edit dir` no longer builds a preview command for every directory it
  finds, only for the one you are looking at — the walk of a large tree stays
  flat rather than growing a pane per row.

## v0.7.0

*Released 2026-08-17*

### Added

- `scriv pr open --current` opens the pull request for the branch you have
  checked out, and the repository's pull request list when it has none. In
  fish, f2 does it.

### Changed

- `scriv repo clone` shows the date each repository was last pushed to, as a
  column before the description.
- Its rows are no longer coloured end to end. Only the visibility word is
  tinted — yellow private, magenta internal — and a repository you already have
  is marked with a green tick rather than being greyed out.
- The preview pane is gone from that selector: everything it held is a column
  now.

## v0.6.0

*Released 2026-08-17*

### Added

- `scriv repo clone` colours its list by who can see a repository: private is
  yellow, internal magenta, public the terminal's own foreground. A repository
  you already have stays grey.
- `scriv repo clone --archived` lists an owner's archived repositories, which
  are now left out by default.

## v0.5.1

*Released 2026-08-13*

### Added

- An install script, published with every release:
  `curl -LsSf https://github.com/joakimen/scriv/releases/latest/download/scriv-installer.sh | sh`.
  It installs into `~/.local/bin`.

## v0.5.0

*Released 2026-08-12*

### Added

- `scriv worktree` lists and selects the working trees of the repository you are
  standing in. In fish, ctrl-t jumps to one.

### Changed

- A release is cut by pushing a tag, rather than by dispatching a workflow that
  needed a personal access token to open its own pull request.

## v0.4.0

*Released 2026-08-11*

### Changed

- scriv builds for Apple Silicon and refuses to compile anywhere else. Signal
  numbers past the POSIX five differ between Darwin and Linux — 19 is `CONT` on
  one and `STOP` on the other — so a binary for the wrong platform resumed the
  process it was told to suspend.

## v0.3.2

*Released 2026-08-11*

### Changed

- Releases are built and published by GitHub Actions. Nothing is compiled or
  signed on a maintainer's machine.

## v0.3.1

*Released 2026-08-11*

### Changed

- ctrl-q is left unbound, for your own use; `scriv edit` is reached through the
  `fe` function instead.

## v0.3.0

*Released 2026-08-10*

### Added

- `scriv proc` finds a running process and signals it. scriv's own process and
  everything that spawned it are never offered.
- `scriv config check` looks at everything scriv depends on in one pass and says
  what is wrong with each.
- `scriv file prune` drops tracked files that no longer exist.
- `scriv edit dir` selects a directory, splitting `edit` into `file` and `dir`.
- `--color auto|always|never`, and `SCRIV_NO_COLOR` under `auto`.

### Changed

- `pick` is now `sel` and `remove` is `rm`, and each group takes a one-letter
  abbreviation: `r`, `f`, `b`, `w`, `e`, `h`, `c`, and `pc` for `proc`.
- A development build reports `<version>-dev.<sha>`, so it cannot be mistaken
  for the release of the same name.

### Fixed

- A selector no longer spins at full CPU after its terminal goes away.
- A key binding no longer draws over the output it just produced.

## v0.2.1

*Released 2026-07-30*

### Added

- `scriv history` searches fish's history, newest first with repeats collapsed,
  each row dated by when you last ran it.

### Fixed

- A listing ends quietly when its reader stops reading, so `scriv history ls |
  head` no longer ends in a panic.

## v0.2.0

*Released 2026-07-30*

Rewritten in Rust, with the fuzzy finder compiled in rather than shelled out to.

### Added

- `scriv branch` and `scriv pr` select branches and GitHub pull requests, with
  preview panes and ctrl-r to refresh the list in place.
- `scriv edit` finds a file below `$PWD` and opens it in `$EDITOR`.
- `scriv repo clone` clones into `<root>/<owner>/<repo>`, and `scriv repo open`
  opens a repository's GitHub page.
- `scriv file` takes over the standalone `kf` tool, migrating its list on first
  use.
- `scriv init fish` emits helper functions, key bindings and completions.

### Changed

- Repositories live under one `root` laid out as `<owner>/<repo>`, and labels
  name owners rather than paths. A config in the old shape is refused with the
  replacement written out.

## Earlier releases

0.1.0 and 0.0.1 were the Go tool that only discovered repositories. Their
history is in git.
