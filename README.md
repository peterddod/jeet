<p align="center">
  <img src="assets/banner.png" alt="jeet — global git repository index and worktree manager" width="800">
</p>

# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, mirrors worktrees under a predictable layout, and maintains a SQLite index so you can list, find, and jump into repos quickly.

Run `jeet` with no arguments inside a repo and you get a file explorer: one level of the tree at a time, your current worktree pinned to the top, type-to-filter with shell-style `⇥` completion, and one keystroke to switch worktree, open a file, or hand the tree to a coding agent.

Beyond this, `jeet` is also very AI friendly and will help your agents juggle multiple workflows at once seamlessly.

## Install

### Homebrew

```bash
brew tap peterddod/jeet
brew install jeet
```

### apt (Debian / Ubuntu)

```bash
curl -fsSL https://peterddod.github.io/jeet/deb-install.sh | bash
```

Or manually:

```bash
echo "deb [trusted=yes] https://peterddod.github.io/jeet stable main" \
  | sudo tee /etc/apt/sources.list.d/jeet.list
sudo apt update
sudo apt install jeet
```

### cargo

```bash
cargo install --git https://github.com/peterddod/jeet --tag v0.6.0
```

## Quick start

```bash
# Shell integration (required for `jeet cd`)
eval "$(jeet init-shell)"

# Clone into the canonical store
jeet clone https://github.com/acme/widget.git

# Fetch every indexed repo now (this also happens on its own; see below)
jeet sync
jeet sync --repo acme/widget

# Register an existing local checkout
jeet adopt ~/Projects/exceed

# Scan configured roots (see config)
jeet scan

# List indexed repos
jeet ls
jeet ls acme

# Explore (this is the main event)
jeet cd acme/widget                      # jump to the trunk
jeet                                     # open the file explorer there

# Tasks — a workspace directory per piece of work, worktrees added on demand
jeet task new "Fix login"                # empty canvas; prints ~/.jeet/tasks/fix-login
jeet task new api-v2 --repo api --branch v2   # or start with a worktree in it
jeet task ls [--archived]
jeet task archive fix-login              # free the disk, keep the work
jeet task unarchive fix-login
jeet task delete fix-login [--force]

# Worktrees — of the task you are in (or --task <name>)
jeet worktree add api                    # detached at origin's default branch; prints the path
jeet worktree add api --branch feat-x    # existing branch, origin's, or a new one
jeet worktree ls                         # with dirty + diff counters
jeet worktree rm api [--force]
jeet worktree path api

# Navigation
jeet path acme/widget                    # print path (scripting)
jeet cd acme/widget                      # native cd (requires init-shell)
jeet checkout feature-x                  # cd into that branch's worktree, creating it if needed
jeet exec acme/widget                    # subshell in trunk
jeet exec acme/widget --branch feature-x # subshell in worktree
jeet exec acme/widget --ephemeral        # throwaway worktree (auto-removed on exit)

# Coding agents
jeet sessions                            # previous agent sessions for this worktree

# Reviewing pull requests — from anywhere inside the repo
jeet prs                                 # the open PRs, and which you have checked out
jeet review 123                          # or #123, or the PR's URL
jeet prs acme/widget                     # or name the repo, from anywhere
jeet review 123 --repo acme/widget
```

## The explorer

```bash
jeet          # inside any repo, trunk or worktree (also `jeet explore`)
```

```text
┌ jeet · github.com/acme/widget ────────────────────────────────┐
│worktree feature-x  [worktree]  1 uncommitted  +2/-0 in 1 file │
│path     /src                                                  │
│filter   co█  1 of 2                                           │
└───────────────────────────────────────────────────────────────┘
┌ 1 of 2 items ─────────────────────────────────────────────────┐
│▸ commands/                                                    │
└───────────────────────────────────────────────────────────────┘
```

### Type to find things

The filter is live the moment the window opens — no key to press first. Type
and the level narrows to what matches, case-insensitively: the name you spelled
out in full first, then names that *start* with what you typed, then names that
merely contain it.

| key | action |
|-----|--------|
| any letter, digit or symbol | filter this level |
| `⇥` | complete the filter as far as the matches agree; again takes the highlighted row |
| `/` or `\` | step into the highlighted folder, and start a fresh filter |
| | (`\` is also a legal filename character, so it types whenever there is still a name it could be part of) |
| `⌫` | delete a character |
| `ctrl-u` | clear the filter |
| `esc` | clear the filter — or, with nothing typed, quit |

`⇥` fills in like a shell: with one match it completes the name outright, with
several it stops where they stop agreeing. Press it again and it takes the
highlighted row rather than sitting there — so arrowing down first completes
towards that row, and the cursor stays on it. With nothing typed it only fills
in what every name shares, as a shell does. `/` then walks in, so `sr⇥/` gets
you into `src/` without leaving the keyboard, exactly as typing a path does.

`/` steps into the highlighted folder — the same row `⏎` and `→` act on, and
the one you can see. Typing a folder's name out in full puts it there: an exact
match sorts to the top whatever else the filter caught.

### Moving around

| key | action |
|-----|--------|
| `↑` / `↓` | move within the current level |
| `→` | expand: step into the highlighted folder |
| `←` | back: leave the folder (never above the worktree root) |
| `⏎` | folder: step in · file: open it in your editor |
| click | a folder to step into it, a file to select it, a path crumb to jump up |
| scroll | move the cursor |
| shift-drag | select text (see below) |
| `Home` / `End` | jump to the top / bottom |
| `PageUp` / `PageDown` | move ten rows |

While the explorer is up it has the mouse, which means your terminal's own
click-drag text selection is off for as long as it runs. Hold **shift** while
dragging to select as you normally would — every terminal that reports mouse
events supports that escape hatch. jeet gives the mouse back whenever it steps
aside for an editor or an agent, and on the way out however it leaves.

### Everything else

Because a bare letter belongs to the filter, the commands carry a `ctrl`:

| key | action |
|-----|--------|
| `ctrl-a` | start a coding agent from the worktree root |
| `ctrl-s` | previous agent sessions for this worktree (⏎ resumes one) |
| `ctrl-w` | worktrees: `⏎` switch, `n` new branch, `e` detached, `m` rename, `d` delete, `o` open its PR |
| `ctrl-f` | diff the highlighted file or folder against the default branch |
| `ctrl-p` | the pull request: `o` open, `a` approve, `c` comment, `x` request changes, `d` diff, `v` review output |
| `ctrl-o` | the repository's open pull requests: `⏎` checks one out and switches to it, `o` opens it in the browser |
| `ctrl-d` | toggle hidden dotfiles |
| `ctrl-r` | refresh the listing and the counters |
| `ctrl-q` | quit, leaving your shell in the directory you were browsing |
| `esc` | quit without moving your shell (unless it moved out from under you) |
| `F1` or `ctrl-g` | the key list, in jeet |

In the worktree panel: `r` refreshes, `esc` closes, and `ctrl-u` clears the
name field in the new/rename prompts. Deleting asks first: `y` removes, and
`f` forces past git's refusal when the worktree still holds work. The panels
are keyboard-driven, so single letters still work inside them, and the key
that opened one closes it again.

Anything slow — reading a repo's worktrees, or creating and renaming, which
push to `origin` — runs in the background with a progress indicator, so the
explorer keeps drawing instead of freezing on the network.

Leaving your shell in the right directory needs the shell wrapper
(`eval "$(jeet init-shell)"`); without it jeet prints the path instead.

The editor defaults to `$VISUAL`/`$EDITOR` and falls back to `vim`; the coding
agent defaults to `claude`. Both are configurable (see below). Session listing
knows Claude Code's transcript store — other agents still launch with `ctrl-a`, they
just have no session history to show.

### Diffs

Every changed file shows its `+added -removed` line counts beside its size,
and every folder shows the sum of everything changed beneath it — measured
from the merge base with the default branch, uncommitted edits included, the
same diff the header counts. `ctrl-f` opens that diff for the highlighted file
or folder (or the whole worktree, with `d` in the PR panel) in `git difftool`,
one file at a time: `:qa` moves to the next, `:cq` stops. It uses your
`diff.tool` from git config, and `vimdiff` (`nvimdiff` for a neovim editor)
when there isn't one — `git config --global diff.tool nvimdiff`, `difftastic`
or anything else git supports all work.

### Pull requests

When the worktree's branch has a pull request, its number and status sit in
the header's top border; click it to open the PR in your browser. `ctrl-p`
opens a panel with the details, and lets you approve (`a`), comment (`c`) or
request changes (`x`) with a summary comment — `ctrl-j` for a new line, `⏎`
submits. If you have a **pending review** on the PR — an agent's inline
comments waiting to be submitted — the verdict submits that review, comments
and all, rather than posting a second one.

`ctrl-o` lists every open PR in the repository — the same list as
`jeet prs` — and `⏎` checks the highlighted one out (or finds the worktree
that already has it) and switches the explorer there. It is a way in, not
`jeet review`: no review command is started.

PR support goes through the [GitHub CLI](https://cli.github.com): install
`gh` and `gh auth login`, and the lookups happen in the background so the
explorer never waits on the network.

## Reviewing

```bash
jeet prs                    # open PRs in the repo you are in
jeet prs acme/widget        # or in any indexed repo
jeet review 123             # or #123, or https://github.com/acme/widget/pull/123
jeet review 123 --repo acme/widget
jeet review 123 --no-command
jeet review 123 --rerun
```

`jeet prs` lists the open PRs with their size and review status, and marks the
ones asking for your review, the ones you already have checked out, and how
any background review of them went:

```text
github.com/acme/widget: 3 open pull requests
  #128  Cache the index between runs    ada      +212 -40  needs review       your review requested
  #127  Fix the flaky clone test        grace      +3 -1   approved           checked out · review finished 2h ago
  #121  Rework the config loader        linus    +901 -377 draft
```

From anywhere inside a repository, `jeet review` checks the PR out into its own
worktree (reusing the one that already has its branch, and bringing it up to
date if it is clean), opens it in the review viewer, and `cd`s you into the
worktree when you leave. A PR from a fork is checked out as `pr/<number>`, so
it can never land on a branch of yours with the same name.

The review viewer shows the PR's diff one file to a page, in GitHub's dark
colours (or the nearest 256-colour match when `COLORTERM` is not `truecolor`),
with each review thread boxed under the line it is about. `←`/`→` turn the
page, `↑`/`↓` or the wheel move the cursor, `A`, `C` and `X` approve, comment
or request changes — submitting your pending review if you have one — and
`Esc` goes back. `d` in the explorer's `ctrl-p` panel opens it too.

Give it a command in `config.toml` and it also starts that in the background
from the new worktree — say, Claude Code with your own review command:

```toml
[review]
command = "claude -p '/review-pr {pr}' --permission-mode acceptEdits"
notify = true   # desktop notification when it finishes (the default)
```

The command runs detached from your terminal, so quitting the explorer or
closing the window does not stop it. When it finishes you get a notification,
the header's `review running` turns to `review ready`, and `v` in the PR panel
opens its output. A Claude Code review also leaves a session behind, so
`ctrl-s` can resume the conversation with it. Running `jeet review` again for
the same commit does not start a second review; new commits do (and
`--rerun` forces one).

`{pr}` and `{url}` are substituted into the command. The PR's branch, base and
title are not — whoever opened the PR chose those, and pasted into a shell they
would run as code — so they arrive as `$JEET_PR_BRANCH`, `$JEET_PR_BASE` and
`$JEET_PR_TITLE` instead, alongside `$JEET_PR`, `$JEET_PR_URL` and
`$JEET_REVIEW_LOG`. Jobs and their logs live under `~/.jeet/reviews`.

## Tasks

A task is a directory under `~/.jeet/tasks/<slug>`: a blank canvas for one
piece of work, with any number of git worktrees inside it, added and removed as
the work needs them — several repositories, or several checkouts of one.

```bash
cd "$(jeet task new "Fix login")"   # prints the task directory (and cds there with the wrapper)
jeet worktree add api               # -> ~/.jeet/tasks/fix-login/api, detached at origin/main
jeet worktree add web --branch fix-login
jeet worktree add api               # a second checkout: api-2
jeet worktree ls
```

`jeet worktree` works on the task you are in — anywhere below its directory,
including inside one of its worktrees — or on the one in `$JEET_TASK_ID`, or on
`--task <name>`. Outside a task it says so rather than guessing. `add` and
`path` print the worktree's path and nothing else, so a script or a coding
agent can `cd "$(jeet worktree add api)"`.

`add` fetches the repository first, so a worktree starts from what origin has
now; offline, it starts from the last fetch and says so. The repository can be
a name (`api`), `owner/repo`, an id, or a clone URL — one jeet does not have yet
is cloned into the store. Without `--branch` the worktree is detached at the
default branch. With it, an existing local branch is checked out, a branch that
exists only on origin is tracked, and anything else is created from the default
branch. A branch already checked out elsewhere is refused, naming the task that
has it.

Every task gets a jeet skill (`.claude/skills/jeet/SKILL.md`) that tells a
coding agent how to do all of this itself. A worktree made with plain
`git worktree add` inside a task directory is adopted into the task the next
time jeet looks, and one whose directory was deleted is forgotten.

```bash
jeet worktree rm api        # refuses while it has uncommitted or unpushed work
jeet worktree rm api -f     # removes it anyway
```

A branch the task created is deleted with its worktree only when its commits are
on origin or on the default branch; otherwise it is kept, and jeet says so.

### Archiving and deleting

```bash
jeet task archive fix-login
jeet task unarchive fix-login
jeet task delete fix-login
```

**Archive** frees the disk and keeps the work: each worktree's uncommitted
changes — staged, unstaged and untracked — are saved under
`refs/jeet/stash/<task-id>/<dir>`, its HEAD is pinned under
`refs/jeet/head/<task-id>/<dir>` so not even a commit on a detached HEAD is lost
to gc, and only then is the worktree removed. Ignored files (build output,
`.env`) are not kept, and archive says how many went. The task directory itself
stays, so an agent's sessions still find it. **Unarchive** puts every worktree
back on its branch (or detached at its commit) with its changes reapplied, then
drops the refs.

**Delete** removes everything the task owns: its worktrees, the branches it
created (never ones that existed before), its `refs/jeet` refs and its
directory. It first lists what would be lost — uncommitted changes, and commits
that would be left on no branch or remote — and asks; without a terminal it
needs `--force`.

`jeet cd` is **not** a binary subcommand — it only works via the `init-shell` wrapper. Use `jeet exec` for subshells or `jeet path` in scripts.

Ephemeral sessions warn on uncommitted changes when you exit, then remove the worktree anyway.

## Tab completion

### Homebrew and apt

`brew install jeet` and `apt install jeet` ship **dynamic** completion scripts that call back into `jeet` at tab time, so repo filters and `--branch` values come from your index (not directory listing).

- **bash (Linux):** requires the `bash-completion` package (recommended by the `.deb`).
- **bash (macOS):** install Homebrew `bash-completion@2` and load it in your profile.
- **zsh:** needs `compinit` (default on interactive zsh). Homebrew users should have `eval "$(brew shellenv)"` in `~/.zprofile` / `~/.zshrc`.

### `jeet cd` wrapper

The `jeet cd` command is a shell function, not a binary subcommand. Add **one line** to `~/.zshrc` or `~/.bashrc` for native `cd` plus wrapper tab completion:

```bash
eval "$(jeet init-shell)"
```

Or let jeet write that line for you:

```bash
jeet install-shell
```

Manual install for other shells:

```bash
jeet completions fish > ~/.config/fish/completions/jeet.fish
```

Shell scripts can query candidates directly:

```bash
jeet complete repos
jeet complete branches acme/widget
```

## Migration from v0.1.x

| v0.1.x | v0.2.0 |
|--------|--------|
| `jeet list` | `jeet ls` |
| `jeet worktree list` | `jeet worktree ls` |
| `jeet cd` | `jeet exec` (subshell) or `jeet cd` via init-shell |
| `jeet cd --print` | `jeet path` |

## What's new in v0.6

- `jeet prs [repo]` lists a repository's open pull requests — size, review
  status, and which ones want your review, are checked out, or have been
  reviewed in the background.
- `ctrl-o` in the explorer shows the same list; `⏎` checks a PR out and
  switches to it (without starting the review command).
- `jeet review --repo` checks a PR out from outside its repository.

## What's new in v0.5

- `jeet review <pr>` checks a pull request out into its own worktree, opens
  the explorer there, and can start a review command (Claude Code, say) in the
  background with a desktop notification when it finishes. See
  [Reviewing](#reviewing).
- The explorer shows `+added -removed` for every changed file and folder
  against the default branch, and `ctrl-f` opens the diff in `git difftool`.
- The worktree's pull request shows in the header (click to open it), and
  `ctrl-p` approves, comments or requests changes — submitting your pending
  review, inline comments and all, if you have one. Needs the `gh` CLI.

## What's new in v0.4

- The explorer filters as you type, completes with `⇥`, walks into a folder on
  `/`, and follows the mouse.
- **Its commands moved onto `ctrl`** to leave the letters free for typing:
  `ctrl-w` worktrees, `ctrl-s` sessions, `ctrl-a` agent, `ctrl-d` dotfiles,
  `ctrl-r` refresh, `ctrl-q` quit, `F1`/`ctrl-g` help. The vim-style
  `j`/`k`/`h`/`l` aliases are gone; the arrow keys they aliased still work.
- While the explorer is up it takes the mouse, so your terminal's own
  click-drag text selection is off for as long as it runs. Shift-drag selects
  as usual.

## What's new in v0.3

- `jeet` with no arguments opens the file explorer (`jeet explore`).
- `jeet worktree [name]` creates a worktree from anywhere in a repo; a name
  publishes the branch to `origin`, no name gives you a detached checkout.
- `jeet worktree rename [old] <new>` renames a worktree's branch, and turns a
  detached scratchpad into a named branch without disturbing your work.
- `jeet worktree clean` and the explorer's worktree panel delete worktrees,
  showing uncommitted work and a diff counter against the default branch first.
- `jeet sessions` lists the coding-agent sessions recorded for a worktree.
- The `init-shell` wrapper now follows a `cd` hand-off, so the explorer and
  `jeet worktree` can leave your shell in the right directory.
- `jeet exec --branch <name>` works (the flag was previously documented but
  never wired up).

## Configuration

On first run, `jeet` creates `~/.jeet/config.toml`:

```toml
scan_roots = ["~/Projects", "~/code"]
```

Two optional keys are not written by default; add them yourself to override:

```toml
editor = "vim"       # opened by ⏎ in the explorer; defaults to $VISUAL/$EDITOR
agent = "claude"     # launched by `ctrl-a`; may include arguments
```

Both accept arguments (`editor = "code --wait"`) and are overridden by
`JEET_EDITOR` / `JEET_AGENT`.

A `[review]` section sets what `jeet review` runs in the background — see
[Reviewing](#reviewing).

### Keeping the store fetched

Commands that go to the network anyway (`jeet clone`, `jeet prs`,
`jeet review`) and the explorer on launch start a background fetch of every
indexed repo, at most once every five minutes. A trunk in the store that is
clean and on its default branch is fast-forwarded to origin; one with local
changes or another branch checked out is fetched but left alone, and repos
outside the store are only ever fetched. Its output goes to
`~/.jeet/.sync.log`. `jeet sync` does the same in the foreground.

```toml
sync_interval_secs = 300   # 0 turns background syncing off
```

`JEET_SYNC_INTERVAL_SECS` overrides it.

Override the home directory for testing:

```bash
export JEET_HOME=/tmp/jeet-test
```

## Layout

```
~/.jeet/
  config.toml
  index.db
  .last-sync, .sync.lock, .sync.log  # background store sync
  store/
    github.com/acme/widget/          # trunk (default branch)
  tasks/
    fix-login/                       # a task: the agent's working directory
      .jeet/task-id                  # marks it, so jeet finds the task from below
      .claude/skills/jeet/SKILL.md   # how an agent adds worktrees
      widget/  widget-2/             # its worktrees, added on demand
  projects/<name>/                   # templates: CLAUDE.md and skills/ copied into new tasks
  worktrees/
    github.com/acme/widget/feature-x/
  ephemeral/
    github.com/acme/widget/<uuid>/    # detached checkouts (exec --ephemeral)
```

Repo ids look like `github.com/acme/widget`. Filters accept full ids or unique suffixes such as `acme/widget`.

## Branch slugs

Branch names are converted to filesystem-safe slugs (`feat/foo` → `feat-foo`).

## Packaging

Release tags build prebuilt binaries, `.deb` packages, and update the Homebrew tap automatically.

## Development

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

### Maintainer: Homebrew tap auto-update (CI)

Release workflows push an updated formula to [`homebrew-jeet`](https://github.com/peterddod/homebrew-jeet). The default `GITHUB_TOKEN` cannot write to other repos — you **must** add a PAT:

1. GitHub → **Settings → Developer settings → Personal access tokens → Fine-grained tokens**
2. **Repository access:** only `peterddod/homebrew-jeet` (not the main `jeet` repo)
3. **Permissions → Repository → Contents:** Read and write
4. On repo **peterddod/jeet** → **Settings → Secrets and variables → Actions**
5. **New repository secret:** name `HOMEBREW_TAP_TOKEN`, value = the PAT
6. Re-run the failed **homebrew-tap** job or tag a new release

The workflow no longer falls back to `GITHUB_TOKEN` (that always 403s on the tap). If the secret is missing or scoped to the wrong repo, **homebrew-tap** fails with a clear error.

Implementation: [`packaging/homebrew/push-formula.sh`](packaging/homebrew/push-formula.sh) and [`secrets.HOMEBREW_TAP_TOKEN`](.github/workflows/release.yml) in the release workflow.

## License

MIT
