<p align="center">
  <img src="assets/banner.png" alt="jeet — global git repository index and worktree manager" width="800">
</p>

# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, mirrors worktrees under a predictable layout, and maintains a SQLite index so you can list, find, and jump into repos quickly.

Run `jeet` with no arguments inside a repo and you get a file explorer: one level of the tree at a time, your current worktree pinned to the top, and one keystroke to switch worktree, open a file, or hand the tree to a coding agent.

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
cargo install --git https://github.com/peterddod/jeet --tag v0.3.0
```

## Quick start

```bash
# Shell integration (required for `jeet cd`)
eval "$(jeet init-shell)"

# Clone into the canonical store
jeet clone https://github.com/acme/widget.git

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

# Worktrees — from anywhere inside a repo or worktree
jeet worktree feature-x                  # branch feature-x, published to origin
jeet worktree                            # detached checkout of the default branch
jeet worktree rename login-page          # name a scratchpad once you know what it is
jeet worktree ls acme/widget             # with dirty + diff counters
jeet worktree clean                      # drop worktrees holding no work
jeet worktree add acme/widget feature-x  # explicit form; does not push
jeet worktree remove acme/widget feature-x [--force]

# Navigation
jeet path acme/widget                    # print path (scripting)
jeet cd acme/widget                      # native cd (requires init-shell)
jeet checkout feature-x                  # cd into that branch's worktree, creating it if needed
jeet exec acme/widget                    # subshell in trunk
jeet exec acme/widget --branch feature-x # subshell in worktree
jeet exec acme/widget --ephemeral        # throwaway worktree (auto-removed on exit)

# Coding agents
jeet sessions                            # previous agent sessions for this worktree
```

## The explorer

```bash
jeet          # inside any repo, trunk or worktree (also `jeet explore`)
```

```text
┌ jeet · github.com/acme/widget ────────────────────────────────┐
│worktree feature-x  [worktree]  1 uncommitted  +2/-0 in 1 file │
│path     /src                                                  │
└───────────────────────────────────────────────────────────────┘
┌ 2 items ──────────────────────────────────────────────────────┐
│▸ commands/                                                    │
│  main.rs                                                 13B  │
└───────────────────────────────────────────────────────────────┘
```

| key | action |
|-----|--------|
| `↑` / `↓` (or `k` / `j`) | move within the current level |
| `→` / `l` | expand: step into the highlighted folder |
| `←` / `h` | back: leave the folder (never above the worktree root) |
| `⏎` | folder: step in · file: open it in your editor |
| `c` | start a coding agent from the worktree root |
| `s` | previous agent sessions for this worktree (⏎ resumes one) |
| `w` | worktrees: `⏎` switch, `n` new branch, `e` detached, `m` rename, `d` delete |
| `.` | toggle hidden files |
| `g` / `G`, `Home` / `End` | jump to the top / bottom |
| `PageUp` / `PageDown` | move ten rows |
| `r` | refresh the listing and the counters |
| `q` | quit, leaving your shell in the directory you were browsing |
| `esc` | quit without moving your shell (unless it moved out from under you) |

In the worktree panel: `r` refreshes, `esc` closes, and `ctrl-u` clears the
name field in the new/rename prompts. Deleting asks first: `y` removes, and
`f` forces past git's refusal when the worktree still holds work.

Anything slow — reading a repo's worktrees, or creating and renaming, which
push to `origin` — runs in the background with a progress indicator, so the
explorer keeps drawing instead of freezing on the network.

Leaving your shell in the right directory needs the shell wrapper
(`eval "$(jeet init-shell)"`); without it jeet prints the path instead.

The editor defaults to `$VISUAL`/`$EDITOR` and falls back to `vim`; the coding
agent defaults to `claude`. Both are configurable (see below). Session listing
knows Claude Code's transcript store — other agents still launch with `c`, they
just have no session history to show.

## Worktrees

```bash
jeet worktree feature-x     # create the branch, worktree it, push it to origin
jeet worktree               # detached checkout of the default branch
jeet worktree feature-x --no-push
jeet worktree feature-x --repo acme/widget
```

Scratchpad first, name it later:

```bash
jeet worktree               # detached: poke at something
# ...an hour of hacking...
jeet worktree rename login-page
```

Both forms work from **anywhere** inside a repo or one of its worktrees, not
just the root, and both drop you into the new worktree when the shell wrapper is
installed. Named worktrees live under `~/.jeet/worktrees`; detached ones live under
`~/.jeet/ephemeral`. `jeet worktree clean` collects both — see below for exactly
what it will and will not delete.

### Renaming

```bash
jeet worktree rename login-page          # rename the worktree you are in
jeet worktree rename old-name new-name   # or name the one to rename
jeet worktree rename login-page --no-push
```

Like `jeet worktree <name>`, renaming **publishes the new branch to `origin`**
unless you pass `--no-push`. Renaming onto a name that already exists on the
remote is refused rather than pushed over.

Renaming a **detached** worktree creates that branch at its current HEAD and
moves it out of the ephemeral root, so a throwaway scratchpad becomes a real
branch once you know what it is. Nothing in the working tree is touched —
uncommitted and untracked files come along — and `clean` stops treating it as
disposable.

Renaming a **named** worktree renames its branch and moves the directory to
match. If the old branch was already published, jeet says so rather than
deleting anything on the remote for you:

```text
jeet: origin/old-name still exists; delete it with `git push origin --delete old-name`
```

If your shell was inside the worktree, it follows the move — right down to the
subdirectory you were standing in. Worktrees jeet did not create keep their
directory where you put it; only the branch is renamed.

### Cleaning up

```bash
jeet worktree ls acme/widget    # every worktree with its counters
jeet worktree clean --dry-run   # what would go, and why the rest stays
jeet worktree clean --yes
jeet worktree clean --all --force
```

`clean` removes worktrees that hold nothing you would miss, and reports the ones
it keeps with the reason. What counts as something to lose:

- **uncommitted changes** or **commits not on the default branch** — kept unless
  `--force`.
- **anything jeet could not assess** (a git error, a default branch that no
  longer resolves) — kept, because "I could not tell" must never read as "safe
  to delete".
- **ignored files** (`.env`, local databases, build output) — git deletes these
  without complaint, so jeet counts them separately. They are named in the
  report so an interactive run can decide, and an unattended `--yes` run keeps
  the worktree rather than destroying them silently. `--force` discards them.

Scope is separate from all of that. By default `clean` only touches worktrees
jeet created; **`--all` also deletes worktrees you made yourself, wherever you
put them**. `--force` never widens scope — it only decides whether work already
in scope may be discarded.

Removal itself is not forced: git independently re-checks for modified,
untracked and submodule content at removal time, and jeet reports the refusal
rather than overriding it. `d` in the explorer's worktree panel does the same,
showing the counters before it asks; `y` removes and `f` forces.

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
agent = "claude"     # launched by `c`; may include arguments
```

Both accept arguments (`editor = "code --wait"`) and are overridden by
`JEET_EDITOR` / `JEET_AGENT`.

Override the home directory for testing:

```bash
export JEET_HOME=/tmp/jeet-test
```

## Layout

```
~/.jeet/
  config.toml
  index.db
  store/
    github.com/acme/widget/          # trunk (default branch)
  worktrees/
    github.com/acme/widget/feature-x/
  ephemeral/
    github.com/acme/widget/<uuid>/    # detached checkouts (exec --ephemeral, jeet worktree)
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
