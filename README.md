# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, mirrors worktrees under a predictable layout, and maintains a SQLite index so you can list, find, and jump into repos quickly.

## MVP scope

- Canonical trunk store at `~/.jeet/store/<host>/<owner>/<repo>/`
- Global worktree mirror at `~/.jeet/worktrees/<host>/<owner>/<repo>/<branch-slug>/`
- SQLite index at `~/.jeet/index.db`
- Commands: `clone`, `adopt`, `scan`, `list`, `worktree`, `path`, `cd`, `init-shell`

## Install

```bash
cargo install --git https://github.com/YOUR_USER/jeet --tag v0.1.0
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

## Development

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## License

MIT
