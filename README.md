# cid

[![ci](https://github.com/joakimen/cid/actions/workflows/ci.yml/badge.svg)](https://github.com/joakimen/cid/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/joakimen/cid?logo=github&color=blue)](https://github.com/joakimen/cid/releases/latest)
[![platform](https://img.shields.io/badge/platform-macOS%20%28Apple%20Silicon%29-blue?logo=apple&logoColor=white)](#install)
[![license](https://img.shields.io/github/license/joakimen/cid?color=blue)](LICENSE)

![An airship above a shell prompt reading cid](docs/art/banner.svg)

Provides fuzzy-completion for various local and remote resources.

One fuzzy selector over things that would otherwise each need their own command
and output parser. Every group lists its set, selects from it, and acts on the
selection: `ls` prints, `sel` picks one, and the rest act. `sel` writes to
stdout and composes — `cd (cid repo sel)`. The finder is linked into the
binary, so there is no `fzf` subprocess.

![cid: jump to a repository, check out a remote branch and a pull request, search notes by content, open a file](docs/demo.gif)

## Install

macOS on Apple Silicon.

```sh
# installer script — writes to ~/.local/bin and adds it to PATH
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/joakimen/cid/releases/latest/download/cid-installer.sh | sh

# version manager
mise use -g github:joakimen/cid

# from source
cargo install --git https://github.com/joakimen/cid
```

## Setup

```sh
cid config init     # write ~/.config/cid/config.toml, then set `root`
cid init <shell>    # shell functions, key bindings and completions, to source
cid config print    # every setting, and what is in force for it
cid config check    # what cid reaches for, and what is missing
```

## Commands

| Command | |
| --- | --- |
| `cid repo` | your Git repositories, laid out as `<root>/<owner>/<repo>` |
| `cid file` | the files you keep coming back to, wherever they live |
| `cid edit` | a file or directory below `$PWD`, opened in `$EDITOR` |
| `cid note` | the Markdown notes in your vault — by name, or by what they say |
| `cid branch` | this repository's branches, local and remote |
| `cid worktree` | this repository's worktrees |
| `cid pr` | this repository's GitHub pull requests |
| `cid ps` | what is running, and what to kill |
| `cid history` | the commands you have already run, back onto the command line |
| `cid project` | builds or installs `$PWD`, whatever it turns out to be written in |
| `cid config` | write, read back and check the configuration |
| `cid stats` | what you run, how often, and how long cid takes over it |
| `cid init` | the shell integration below |

Verbs and flags are in `cid <command> --help`; settings are described in the
generated `config.toml`.

## Key bindings

cid binds no key of its own. `[shell.bindings]` maps a key to an action and
`[shell.aliases]` maps a name to one — neither holds shell code, so one table
serves every shell cid can write for. `cid config init` writes a set out
commented, to uncomment and edit:

```toml
[shell.bindings]
ctrl-o = "repo-cd"          # cd to a repository
ctrl-t = "worktree-cd"      # cd to a worktree of this repository
ctrl-g = "branch-checkout"  # check out a branch
f10    = "note-edit"        # open a note from the vault

[shell.aliases]
fe = "edit"                 # cid edit
b  = "project-build"        # cid project build
```

What a table holds is the whole of what is bound, so leaving a key out is how
it stays free. `cid config actions` lists every action with whatever is bound
to it, `cid config print` lists what you have, and `cid config check` says
whether they all resolve — an action cid
does not define stops `cid init` rather than emitting a shell where one key
silently does nothing.

Inside a selector, `ctrl-v` hides and shows the preview pane and `tab` takes
several rows where several are allowed. Anything else is named in the
selector's own header.

## Development

```sh
make              # fmt check, clippy, tests, release build
make hooks        # install the git hooks in prek.toml
make demo         # re-record docs/demo.gif
make demo-fixture # build the demo sandbox and poke at it by hand
```

Dependency updates come from [Renovate](https://docs.renovatebot.com),
configured in `.github/renovate.json5`.

## Releasing

Merging the [release-plz](https://release-plz.dev) pull request tags the merge commit, and [dist](https://axodotdev.github.io/cargo-dist) builds and publishes the release from that version's section of `CHANGELOG.md`.

## License

[MIT](LICENSE)
