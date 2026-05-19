<p align="center">
  <img src="assets/banner.png" alt="jeet — global git repository index and worktree manager" width="800">
</p>

# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, mirrors worktrees under a predictable layout, and maintains a SQLite index so you can list, find, and jump into repos quickly.

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
cargo install --git https://github.com/peterddod/jeet --tag v0.2.0
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

# Worktrees
jeet worktree add acme/widget feature-x
jeet worktree ls acme/widget
jeet worktree remove acme/widget feature-x

# Navigation
jeet path acme/widget                    # print path (scripting)
jeet cd acme/widget                      # native cd (requires init-shell)
jeet exec acme/widget                    # subshell in trunk
jeet exec acme/widget --branch feature-x # subshell in worktree
jeet exec acme/widget --ephemeral        # throwaway worktree (auto-removed on exit)
```

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

## Configuration

On first run, `jeet` creates `~/.jeet/config.toml`:

```toml
scan_roots = ["~/Projects", "~/code"]
```

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
    github.com/acme/widget/<uuid>/    # temporary exec --ephemeral checkouts
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
