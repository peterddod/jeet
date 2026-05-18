# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, mirrors worktrees under a predictable layout, and maintains a SQLite index so you can list, find, and jump into repos quickly.

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
cargo install --git https://github.com/peterddod/jeet --tag v0.1.1
```

Or from a checkout:

```bash
cargo install --path .
```

## Quick start

```bash
# Clone into the canonical store
jeet clone https://github.com/acme/widget.git

# Register an existing local checkout
jeet adopt ~/Projects/exceed

# Scan configured roots (see config)
jeet scan

# List indexed repos
jeet list
jeet list acme

# Worktrees
jeet worktree add acme/widget feature-x
jeet worktree list acme/widget
jeet worktree remove acme/widget feature-x

# Jump to a repo
jeet path acme/widget
jeet path acme/widget --branch feature-x

# Native cd (recommended)
eval "$(jeet init-shell)"
jeet cd acme/widget
jeet cd acme/widget --branch feature-x
```

Without shell integration, `jeet cd` starts a subshell in the target directory.

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
```

Repo ids look like `github.com/acme/widget`. Filters accept full ids or unique suffixes such as `acme/widget`.

## Branch slugs

Branch names are converted to filesystem-safe slugs (`feat/foo` → `feat-foo`).

## Packaging

Release tags build:

- Prebuilt binaries (Linux + macOS, x86_64 and arm64)
- `.deb` packages published to the [GitHub Pages apt repo](https://peterddod.github.io/jeet)
- An updated [Homebrew tap](https://github.com/peterddod/homebrew-jeet)

## Development

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## License

MIT
