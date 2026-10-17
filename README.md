<p align="center">
  <img src="assets/banner.png" alt="jeet — global git repository index and worktree manager" width="800">
</p>

# jeet

Global git repository index and worktree manager.

`jeet` keeps a canonical store of repository trunks, organises work into **tasks** — a directory per piece of work, with as many git worktrees inside it as the work needs — and maintains a SQLite index so you can list, find, and jump into repos quickly. The only checkouts on disk are the store's trunks and the tasks' worktrees.

Run `jeet` with no arguments and you get your tasks: one row per piece of work, with what its coding agent is doing, the worktrees it holds and when anything last happened in it, and one keystroke to attach to the agent, browse the task, or archive it. `jeet explore` inside a repo gives you a file explorer: one level of the tree at a time, type-to-filter with shell-style `⇥` completion, and one keystroke to switch worktree, open a file, or hand the tree to a coding agent.

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
cargo install --git https://github.com/peterddod/jeet --tag v0.13.0
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

# Bring repositories and work from outside jeet into the store and tasks
jeet import                              # scans scan_roots (see config)
jeet import ~/Projects/exceed            # or just these

# List the store's repos
jeet ls
jeet ls acme

# The tasks list (this is the main event)
jeet                                     # or `jeet tasks`

# Explore
jeet cd acme/widget                      # jump to the trunk
jeet explore                             # open the file explorer there

# Tasks — a workspace directory per piece of work, worktrees added on demand
jeet task new "Fix login"                # empty canvas; prints ~/.jeet/tasks/fix-login
jeet task new api-v2 --repo api --branch v2   # or start with a worktree in it
jeet task ls [--archived]                # with what each task's agent is doing
jeet task open fix-login                 # the task's agent, in tmux (started if need be)
jeet task new fix-x --prompt "Fix X"     # …and start its agent on a brief
jeet task send fix-login "add tests too" # message the task's agent
jeet task interrupt fix-login            # end the agent's current turn, keeping its session
jeet task restart fix-login [--force]    # end the agent and resume its conversation in a new one
jeet task adopt fix-login                # vouch for an agent an older jeet started (you, at a terminal)
jeet task last fix-login [-n 3] [--json] # what the task's agent last said, and how its turn ended
jeet task archive fix-login              # free the disk, keep the work
jeet task unarchive fix-login
jeet task delete fix-login [--force]
jeet task delete api --recursive         # a task and every task under it (also archive, unarchive)
jeet task delete api --reparent          # api alone; the tasks under it move to its parent
jeet task prs fix-login                  # the PRs that came out of the task

# The assistant — one organiser for everything, in tmux
jeet chat                                # resume its latest session (set up on first run)
jeet chat --new "plan the week"          # a fresh session beside the others
jeet chat --pick                         # choose, rename, end or mark one primary
jeet todo add "ship the fix" -p 1 --task fix-login
jeet todo                                # its todo list; done, rm, --json
jeet task new fix-x --parent assistant --goal "Fix X" --done "tests pass"  # a task that reports to it
jeet manager new api --goal "Ship the v2 API" --budget 4h  # a manager under it, started
jeet manager ls                          # the tree under it; show, grant, budget <task> …
jeet manager pause fix-x                 # interrupt it and hand it nothing; resume, stop (--all)
jeet report done "PR #12 is up"          # from inside such a task: tell the assistant
jeet watch install                       # poll GitHub in the background (launchd)

# Worktrees — of the task you are in (or --task <name>)
jeet worktree add api                    # detached at origin's default branch; prints the path
jeet worktree add api --branch feat-x    # existing branch, origin's, or a new one
jeet worktree ls                         # with dirty + diff counters
jeet worktree rm api [--force]
jeet worktree path api

# Navigation
jeet path acme/widget                    # print the trunk's path (scripting)
jeet cd acme/widget                      # native cd (requires init-shell)
jeet exec acme/widget                    # subshell in the trunk

# Reviewing pull requests — from anywhere inside the repo
jeet prs                                 # the open PRs, and which you have checked out
jeet review 123                          # or #123, or the PR's URL
jeet prs acme/widget                     # or name the repo, from anywhere
jeet review 123 --repo acme/widget
jeet pr merge 123                        # merge it, once it is ready (see Merging)
```

## The explorer

```bash
jeet explore  # inside a store repo, trunk or task worktree — or a task's directory
```

From the tasks list, `e` opens it on a task, and `esc` goes back to the list.

```text
┌ jeet · github.com/acme/widget ────────────────────────────────┐
│worktree feature-x  [task]  1 uncommitted  +2/-0 in 1 file     │
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
| `⌫` | delete a character — or, with nothing typed, leave the folder, as `←` |
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
| `ctrl-a` | the task's coding agent, in its tmux session — started if it is not running |
| `ctrl-s` | the task's agent sessions (`⏎` resumes one) |
| `ctrl-w` | the repository's worktrees: `⏎` switches to one, `o` opens its PR |
| `ctrl-f` | diff the highlighted file or folder against the default branch |
| `ctrl-p` | the pull request: `o` open, `a` approve, `c` comment, `x` request changes, `d` diff, `v` review output |
| `ctrl-o` | the repository's open pull requests: `⏎` checks one out in a task and switches to it, `o` opens it in the browser |
| `ctrl-d` | toggle hidden dotfiles |
| `ctrl-r` | refresh the listing and the counters |
| `ctrl-q` | quit, leaving your shell in the directory you were browsing |
| `esc` | quit without moving your shell (unless it moved out from under you) |
| `F1` or `ctrl-g` | the key list, in jeet |

In the worktree panel: `r` refreshes and `esc` closes. Worktrees are added to
and removed from tasks — `jeet worktree`, or `w` in the tasks list — not here.
The panels are keyboard-driven, so single letters still work inside them, and
the key that opened one closes it again.

Anything slow — reading a repo's worktrees, or looking up pull requests —
runs in the background with a progress indicator, so the explorer keeps
drawing instead of freezing on the network.

Leaving your shell in the right directory needs the shell wrapper
(`eval "$(jeet init-shell)"`); without it jeet prints the path instead.

The editor defaults to `$VISUAL`/`$EDITOR` and falls back to `vim`; the coding
agent defaults to `claude`. Both are configurable (see below). `ctrl-a` and
`ctrl-s` act on the task you are browsing in — its agent runs in the task's
tmux session, exactly as `⏎` in the tasks list runs it — so outside a task
(a store trunk, say) they only say so. Sessions are Claude Code's; other agents
still run with `ctrl-a`, they just have no sessions to show.

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
`jeet prs` — and `⏎` switches the explorer to it in a task: the task whose
worktree has the PR's branch already, or else the PR's review, which it
makes just as `jeet review` would. It is a way in, not `jeet review`: no
review agent is asked to review it.

PR support goes through the [GitHub CLI](https://cli.github.com): install
`gh` and `gh auth login`, and the lookups happen in the background so the
explorer never waits on the network.

## Reviewing

```bash
jeet prs                    # open PRs in the repo you are in
jeet prs acme/widget        # or in any indexed repo
jeet review 123             # or #123, or https://github.com/acme/widget/pull/123
jeet review 123 --repo acme/widget
jeet review 123 --no-agent  # only check it out (or --no-command)
jeet review 123 --rerun
```

`jeet prs` lists the open PRs with their size and review status, and marks the
ones asking for your review, the ones you already have checked out, and how
any background review of them went (run by an agent, the list comes fenced
as `<github-data untrusted="true">`, since titles are anyone's to write):

```text
github.com/acme/widget: 3 open pull requests
  #128  Cache the index between runs    ada      +212 -40  needs review       your review requested
  #127  Fix the flaky clone test        grace      +3 -1   approved           checked out · review finished 2h ago
  #121  Rework the config loader        linus    +901 -377 draft
```

From anywhere inside a repository, `jeet review` finds or makes the PR's
**review**, `review-<repo>-<number>`, checks the PR out in it (or, when it
has the PR already, brings that checkout up to date if it is clean, and
brings an archived review back), has the review's agent review it, opens it
in the review viewer, and `cd`s you into the worktree when you leave. A
review is not a task: it is its own thing, one per pull request (a
repository and a PR number), with exactly one worktree and one session, and
flat, with no parent and nothing under it. It is shown in the tasks list's
[reviews tab](#the-reviews-tab) alone — never among the active or archived
tasks, in the agents tab, in the projects tab, or in `jeet task ls` — and
there `v` opens the viewer again, `a` archives it and brings it back, and
`D` deletes it. `jeet task open|archive|unarchive|delete|path|send|restart
review-<repo>-<number>` take its name too. Archiving or deleting it frees the
disk and ends its agent, and once the PR is merged or closed jeet archives it
itself. A PR from a fork is checked out as `pr/<number>`,
so it can never land on a branch of yours with the same name; and when the
PR's branch is checked out already — in your own task, say — the review is
checked out detached at the PR's head instead.

The review viewer shows the PR's diff one file to a page, in GitHub's dark
colours (or the nearest 256-colour match when `COLORTERM` is not `truecolor`),
with each review thread boxed under the line it is about. Code is
syntax-highlighted in the same theme, over the added and removed backgrounds;
a file shows plain for the moment it takes to work its colours out. `←`/`→`
turn the page, `↑`/`↓` or the wheel move the cursor, `A`, `C` and `X` approve, comment
or request changes — submitting your pending review if you have one — and
`Esc` or `q` goes back. `d` in the explorer's `ctrl-p` panel opens it too.

On a line of code, `c` writes an inline comment, which joins your pending
review (starting one if need be) until a verdict submits it. On a comment, `R`
replies to its thread, `+` picks a reaction, and — on your own comments only —
`e` edits it and `d` deletes it, after asking. The viewer checks the PR every
30 seconds; when someone else has pushed or commented, the header shows
`⟳ updates – r`, and `r` fetches it all again with the cursor kept on the line
it was on.

### The review agent

The review is done by the review's own agent — your harness (Claude Code),
started in the review's tmux pane like a task's agent — so `⏎` on the review
(or `jeet task open review-<repo>-<number>`) shows it at work, and you can
talk to it there (`jeet task send` and `jeet task restart` reach it too, for
you and for the assistant when it asked for the review). It starts with this
prompt:

```toml
[review]
# The default; {pr}, {url} and {worktree} are filled in.
prompt = "Review pull request #{pr} ({url}). It is checked out at the PR's head in {worktree}. Read the change (`gh pr view {pr}`, `gh pr diff {pr}`) and the code around it, then list what is wrong or risky, most serious first, with file and line. Do not push, merge, approve or comment on the PR unless asked."
```

The PR's branch and title are never filled in — whoever opened the PR wrote
them — so the agent reads them with `gh`. The prompt is sent as a message is:
one starting with `/`, `!` or `#` is marked so Claude does not run it as a
command.

One review is one worktree and one session. Running `jeet review`
again for the same commit asks nothing; a new head commit, or `--rerun`,
brings the worktree up to the PR's head (when it is clean) and sends the
prompt again into the same session: typed in when the agent is idle, queued
while it works (once: a prompt still waiting is not queued twice), and
resuming its session in a new pane when it has ended — or, run with no tmux
server to start it on, waiting until the review is next opened. Once its agent
has been asked, a review stays with it even if `[review] command` is set
later. A review paused at its [limits](#review-limits) keeps the prompt until
the review is asked for again. `--no-agent` only checks the PR out.

A review remembers who asked for it: the assistant's id when the assistant
ran `jeet review`, and that holds until the review is deleted. You running
it again, even for the same commit, never clears it; only a later request by
the assistant sets it (to the assistant, still).

Every review agent has the same fixed gate, whoever asked for the review. It
reads: `jeet ls`, `jeet path` and `jeet prs`, and nothing else in jeet. It has
the worker deny rules: no push, force-push, merge, `gh pr review` or
approval, or `gh pr comment`, nor their API equivalents. And it may
`jeet report progress|done|blocked|question` only when a task asked for the
review: a review the assistant asked for reports to the assistant (as a
sender labelled `review <repo>#<number>`), and a review you asked for reports
to no one. The assistant is not its parent: the review is in no tree, takes
no manager's budget or `max_live` slot, and nothing is delegated to it.

#### Review limits

Reviews have limits of their own, apart from `[managers]`: review agents
neither take a `[managers] max_live` slot nor spend any budget above them,
and tasks and managers take nothing from the review limits. They apply to
every review, yours included.

```toml
[review]
max_live = 2    # review agents running at once; a review past it waits for one to end
time = "1h"     # busy time each review may take (30m, 2h, ...)
turns = 20      # turns each review may take
```

`max_live` counts every review agent, yours included: a review past it is
queued, and starts when a review agent's session ends (`jeet task open`
from your terminal starts it anyway, with a warning). `time` and `turns`
apply to every review: each time a review is asked of the agent (a first
one, a new head commit or `--rerun`) it gets them afresh, on top of what it
has used. At 80% the review's agent is told, and so is the assistant when it
asked for the review; at 100% the review is paused until the next review is
asked, and the assistant is told it is blocked. A review you asked for
reports to no one: the reviews tab shows it `paused at its caps`. A task
with a parent may not run `jeet review` at all, so no agent's review escapes
these caps that way. The defaults keep
reviews from crowding out other work (2 at once) and stop a stuck one
(an hour of busy time, or 20 turns, a turn being one prompt answered: a
review is one or a few). Like `[managers]`, these are checked when used: 0,
or a `time` that is not a time, refuses starting a review.

#### Legacy `[review] command`

Before the review agent, `jeet review` could start a command of your own in
the background instead. It still works, but is **deprecated**: set, it runs
in place of the review agent, and `jeet review` warns.

```toml
[review]
command = "claude -p '/review-pr {pr}' --permission-mode acceptEdits"
notify = true   # desktop notification when it finishes (the default)
```

The command runs detached from your terminal, so quitting the explorer or
closing the window does not stop it. When it finishes you get a notification,
the header's `review running` turns to `review ready`, and `v` in the PR panel
opens its output. While it runs the review reads `running – review of #<number>`,
and opening it is refused — headless, there is nothing to attach to; once it
has finished, opening the review resumes the session it left (jeet picks it up
from the PR's worktree), and so does the review agent if you later remove the
command. Running `jeet review` again for the same commit does not start a
second review; new commits do (and `--rerun` forces one). Run for your own
review, the command runs as you, ungated, as it always has; only the assistant's
reviews are held to the review agent's gate.

The job is part of the review: its command, the commit it reviewed, when it
started and ended, its exit status and its runner are columns of the review's
row in the index (the hidden `jeet review-job <review id>` that runs it only
runs a job jeet recorded there, once). Only the log is a file. Job files an
older jeet left under `~/.jeet/reviews` are adopted as reviews on upgrade
([What's new in v0.13](#whats-new-in-v013)).

`{pr}` and `{url}` are substituted into the command. The PR's branch, base and
title are not — whoever opened the PR chose those, and pasted into a shell they
would run as code — so they arrive as `$JEET_PR_BRANCH`, `$JEET_PR_BASE` and
`$JEET_PR_TITLE` instead, alongside `$JEET_PR`, `$JEET_PR_URL` and
`$JEET_REVIEW_LOG`. A job's log is `~/.jeet/reviews/<host>/<owner>/<repo>/<number>.log`; nothing
reads that directory to find reviews.

### Merging

```bash
jeet pr merge 123                   # or #123, or the PR's URL
jeet pr merge 123 --repo acme/widget
jeet pr merge 123 --method squash   # merge, squash or rebase
jeet pr merge 123 --keep-branch
```

`jeet pr merge` merges only a PR that is open, not a draft, free of
conflicts, with every check passed (none pending or failed), no changes
requested or approving review missing, and that GitHub does not block or
want brought up to date. Anything else is refused, saying why; it never
uses admin rights, auto-merge or a merge queue to get round a rule. The
merge is of the commit it checked, so a push in between makes it fail.

The method is `--method`, else `[merge] method` in config.toml when the
repository allows it, else the repository's default. The PR's branch is
then deleted on GitHub (not locally: a task may have it checked out), unless
`--keep-branch` or the repository deletes merged branches itself. Tasks
holding the PR record it as merged in their PR history. Run by an agent, the
PR's title, branch and check names come inside
`<github-data untrusted="true">…</github-data>`, as `jeet review`'s title
does; the user gets them without control characters.

```toml
[merge]
method = "squash"
```

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
is cloned into the store, which the assistant and a task with a parent may do
only holding `repo.clone` (as for `jeet clone`; so too `jeet task new --repo`).
A URL whose path has a `..` or `.` segment, or a control or invisible
character, is refused, so a clone always lands under the store. Without `--branch` the worktree is detached at the
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

### Projects

A project is a template for tasks, in `~/.jeet/projects/<name>`: a
`project.toml` naming the repositories each task starts with a worktree of, an
`AGENTS.md` with instructions for the agent, and `skills/`. `jeet task new
--project <name>` copies the instructions and skills into the new task — a
snapshot — and `jeet task resync` brings them up to date later, keeping any file
the task changed. `jeet manager new --project <name>` starts a manager from one
too, with the project's skills and its `AGENTS.md` as `PROJECT.md`, imported
from the end of the manager's own `AGENTS.md`, never replacing it; it adds no
worktrees, and tasks made under it default to the same project.

```bash
jeet projects new web --repo api --repo web   # prints ~/.jeet/projects/web
jeet task new "Launch" --project web
jeet task resync launch                       # --force replaces the task's edits too
jeet manager new web --goal "Own the web app" --project web   # its tasks start from web too
```

`AGENTS.md` is the file Codex, Cursor, Amp, Gemini CLI and most other coding
agents read, so a project works whichever agent you run. Next to it, jeet writes
a `CLAUDE.md` holding only `@AGENTS.md`, an import: Claude Code reads
`AGENTS.md` itself only when there is no `CLAUDE.md`, and some of its sessions
(older versions, the first session after an upgrade) read `CLAUDE.md` alone.
Claude-only instructions can go below the import.

A project made before jeet wrote `AGENTS.md` has only a `CLAUDE.md`. The next
time a task is made or resynced from it, jeet moves it to `AGENTS.md` and leaves
the import in its place, and says so. A project with both files is left as it
is, and if the move fails, its `CLAUDE.md` is copied as it is. Tasks already
made keep their `CLAUDE.md` until they are resynced: one the task never changed
becomes the import, next to the project's `AGENTS.md`; one it changed is kept.

### The tasks list

```bash
jeet          # or `jeet tasks`
```

```text
┌ jeet · tasks ─────────────────────────────────────────────────────────────┐
│ active 5   agents 6   reviews 2   projects 3    / filter  ⇥ switch        │
└───────────────────────────────────────────────────────────────────────────┘
 ✦ assistant  2 sessions  ● *plan the week · inbox 1 · todo 4 open
 assistant
┌ 3 active tasks ───────────────────────────────────────────────────────────┐
│  task    Fix login        ● running        2 wt: api, web   1.2G just now │
│          Edit the token refresh                                           │
│ manager  backend          ◆ needs you (1)  empty             12K   5m ago │
│          progress: api-docs is up                                         │
│  task    notes            ○ idle 1h        empty             12K   1h ago │
│                                                                           │
└───────────────────────────────────────────────────────────────────────────┘
```

The active tasks are a tree, walked like the explorer's folders: the list
opens on the managers and tasks directly under the assistant (and any with no
parent), `→` on a manager goes into it, to the managers and tasks under it,
`⏎` opens its agent, and `←` or `⌫` backs up to the level above. The line over the list says
where you are (`assistant › backend › …`). Each task takes two lines: a
`manager` or `task` tag, its name, what its agent is doing, its project (the
project it was started from, blank for none; from 120 columns), the worktrees
it holds (as of the last look at the disk, since the agent adds them as it
goes), its size and when anything last happened in it; and under the name, dimmed,
the running agent's latest prompt, else its last report (`progress: …`). A
manager shows the most urgent of its own state and everything below it, as in
the [agents tab](#the-agents-tab), with how many below are so: running when
any task under it runs, though its own chat is idle, and needs you over
running over idle. `/` filters by name the way the explorer does, across every
level: each match says where it is under its name, and `→` on a manager there
goes into it.

Each level is most recently active first. Statuses refresh
every two seconds and sizes are measured in the background, so the list never
waits on either. `⇥` goes on to the [agents tab](#the-agents-tab), then the
[reviews tab](#the-reviews-tab), then the [projects tab](#the-projects-tab), and
back to these (`⇧⇥` the other way). `A`
lists the archived tasks instead of the active ones (a flat list), and back;
both lists end with a dim reminder of it.

The [assistant](#the-assistant) is pinned above the list: how many sessions it
has, and the active one, the session `c` opens, with what it is doing. While
any of its tmux windows were not started by jeet, a red `N unanchored` follows
(`jeet task restart assistant` resumes them in windows jeet started). It is
read from its files on a thread of its own, like the statuses.

| Key | |
|---|---|
| `→` | on a manager, go into it: the managers and tasks under it |
| `←` / `⌫` | back up a level; with a filter, drop it, the cursor on the match, on its level |
| `ctrl-a` | attach to the agent of the manager or task under the cursor, as `⏎` |
| `⏎` | on a manager or task, attach to its agent in tmux, starting it if need be — with a choice of session when there is more than one to resume. `Ctrl-Q` comes back here and leaves it running. On a task with tasks under it, `⏎` goes into it, as `→` |
| `e` | the explorer, on the task's only worktree, or a choice of its worktrees and the task directory itself; `esc` comes back |
| `w` | the task's worktrees, with branch, ahead/behind and diff: `n` adds one, `D` removes one, `⏎` explores it |
| `n` | a new task: a name, and optionally a project, repositories and a branch. Left blank, an empty canvas |
| `a` | archive the task (asks first if its agent is running) — or, in the archived list (`A`), bring it back. A task with tasks under it takes them along, after one confirm |
| `A` | list the archived tasks instead of the active ones, and back |
| `D` | delete the task, after listing any work that would be lost; a task with tasks under it takes them along, after one listing for them all |
| `P` | projects: `n` new, `e` edit `project.toml`, `D` delete, `⏎` start a task from one |
| `S` | copy the task's project files in again |
| `p` | the task's pull requests: state, size and title (a link); `⏎` or `o` opens one in the browser, `v` in the review viewer |
| `v` | the task's pull request in the review viewer: the task's one open PR, else the `p` list to pick one |
| `c` | talk to the assistant: attach to its most recently active session, starting one if need be |
| `C` | the assistant's sessions: `⏎` attach, `n` new, `R` rename, `x` end, `*` primary |
| `t` | the todo list: `space` ticks an item off (or back), `⏎` goes to the task it links, `a` adds one |
| `i` | the assistant's inbox and activity, newest first: what is queued for it, what `jeet watch` saw on GitHub and what it did, from `actions.log`. `⏎` opens an event's page |
| `o` | sort by todo priority: tasks the open items link first, by their best item; again for latest activity |
| `r` | refresh, re-reading the worktrees on disk |
| `?` | every key |
| `q` / `esc` | quit; `ctrl-q` quits into the selected task's directory |

Launching the list also starts a background fetch of the store, when one is
due. `jeet projects` on a terminal opens it with the projects panel up.

### The agents tab

The second tab shows the tree under the [assistant](#the-assistant): the
assistant at the root, then its managers and tasks and theirs, as `jeet manager
ls` orders them (top-level tasks stay in *active*, archived ones in the
archived list).

```text
┌ jeet · agents ──────────────────────────────────────────────────────────────┐
│ active 6   agents 7   reviews 0   projects 2    live 5/8   needs you 1      │
└─────────────────────────────────────────────────────────────────────────────┘
┌ 6 of 7 agents shown ────────────────────────────────────────────────────────┐
│✦ assistant          ◆ needs you (1)        3 children   inbox 2  todo 4     │
│├─ ▾ backend         ◆ needs you (1)        2h10/6h      inbox 1             │
││  ├─ api-docs       ● running – Edit       0h05/1h                          │
││  └─ fix-token-ref… ◆ needs input – Bash   0h40/2h      api#131             │
│├─ fix-login         ✓ done                              web#88              │
│└─ ▸ frontend        ⏸ paused (1 below)     3h50/4h      budget 96%          │
└─────────────────────────────────────────────────────────────────────────────┘
```

Each node shows the most urgent of its own state and its subtree's, in the
order needs you, needs input, blocked (`!`, an open `blocked` or `question`
report), budget (`$`, 80% used or paused by it), running, idle, done; with a
count when it comes from below. A permission prompt is *needs input* on the
task that shows it and *needs you* above it, since only you can answer it; a
prompt in the assistant's own pane counts too, and so does a question the
assistant passed to you (`◆ needs you – question` on the task that asked
it). A paused or stopped node shows how many it holds, and what below it
still needs you. Then its budget used
against its limit (the assistant: how many tasks are under it) and notes: its
inbox, the assistant's open todo items, its newest open PR, a budget past 80%
and a start waiting on `max_live`. The header counts the agents live against
`[managers] max_live`, the prompts and questions waiting on you and the
starts queued.
Statuses come from the same poll as the active tab; the rest is read from the
index every two seconds while the tab is shown.

| Key | |
|---|---|
| `⏎` | attach to the node's agent, as in the active tab; on the assistant, pick one of its sessions |
| `→` / `←` / `⌫` | open / close a manager (everything starts open); else to the first task under it / to its parent |
| `l` | the worklog (`notes/worklog.md`) of the assistant or a manager, scrollable: `↑↓`, `PgUp`/`PgDn`, `g`/`G` |
| `I` | the node's open question, what is queued in its inbox and the reports by and to it, newest first; `⏎` its pull requests |
| `b` | its brief, grant, what it may do (and who cut the rest), budget, run state and the tasks under it, as `jeet manager show` |
| `z` / `Z` | pause the node and everything below it / let it run again, as `jeet manager pause` / `resume` |
| `x` | stop the node and everything below it, as `jeet manager stop`, after a confirm listing what runs; on the assistant, every task under it (`--all`) |
| `n` | a new task or manager under the assistant or a manager, as `jeet task new --parent` / `jeet manager new --parent`: its kind (`space`), name, goal, done, budget (`2h,40turns`) and allow (capabilities, comma-separated); a manager also takes a project; a task takes a project, repositories and a branch (left on "no project", a task under a manager with a project gets the manager's) |
| `y` | answer the open question at the node or nearest below it, one passed to you first, as `jeet report answer` does: type the answer, `⏎` sends it, `esc` cancels |
| `c` `C` `t` `i` | the assistant, as in the active tab: `i` is its inbox, not the node's reports (`I`) |

What each node made of a pause, resume or stop is shown when it is done. A
manager made with `n` starts at once, with its brief, and the status line
says whether it started, was queued on `max_live` or is held by a paused
manager above it; a task made with `n` is not started (`⏎` starts it), as
in the active tab. What jeet refuses (a manager with no goal, a capability
the parent does not hold) is shown on the form. `l`, `I` and `b` show what
agents wrote, unfenced, and `z`, `Z`, `x`, `n` and `y` act as you, so they
work only when you run jeet from your own terminal: not inside an agent's or
a top-level task's pane, nor any window of jeet's tmux sessions. An answer
goes to the task that asked, with a copy to each manager it passed through;
how each went is shown when it is done, and a refusal (answered meanwhile,
say) stays on the form. The worktree, PR, archive and delete keys are the
active tab's: `⇥` back to it.

### The projects tab

The fourth tab groups the active tasks and managers by project: the jeet
project a task was started from (`jeet task new --project`, `n` in the form).
These are the projects `P` manages; `P` makes and edits them, and this tab shows
the work in them. A manager is listed under every project that a task below it,
through any tasks in between, was started from, and under its own project if it
has one. Tasks and managers with no project are not listed here (the active
tab has them).

```text
┌ jeet · projects ──────────────────────────────────────────────────────────┐
│ active 5   agents 6   reviews 2   projects 2    / filter  ⇥ switch        │
└───────────────────────────────────────────────────────────────────────────┘
┌ 2 projects ───────────────────────────────────────────────────────────────┐
│ api                      ◆ needs input  1 manager, 2 tasks       just now │
│ ops, deploy, fix-login                                                    │
│ web                      ● running      1 task                     5m ago │
│ ship-web                                                                  │
└ A other projects ─────────────────────────────────────────────────────────┘
```

Each project takes two lines: its name, the most urgent status among the tasks
and managers in it, how many managers and tasks it has and when anything last
happened in it, most recent first; and under the name, dimmed, who is in it.
`→` or `⏎` goes into a project, to every manager and task
in it, drawn as in the active tab and walked the same way: `→` goes into a manager
or a task with tasks under it, `⏎` opens its agent, `←` or `⌫` backs up a level
and, from the top of the project, out of it. The line over the list says where
you are (`projects › api › …`). `e`, `w`, `p`, `v`, `a`, `D`, `n` and `S` act on
the task under the cursor as in the active tab. `/` filters the projects by
name, or, inside one, its tasks by name.

`A` switches the tab to the other projects, every one with no active task or
manager: those with only archived tasks and those with no tasks at all; inside
one, the archived tasks are a flat list (empty for a project with none), and `a`
brings one back. Both lists end with a dim reminder of it.

### The reviews tab

The third tab lists the reviews: each active review (what `jeet review`
makes). Reviews are shown here alone, never in the other tabs. `A` switches
the tab to the archived reviews (`▪ archived`), and back, as it does the
tasks list: they stay in the reviews tab rather than joining the archived
tasks, so a review is always found in the one place. Every review can be
archived, brought back and deleted here.

```text
┌ jeet · reviews ────────────────────────────────────────────────────────────────────────────────────────┐
│ active 6   agents 7   reviews 2   projects 3    / filter  ⇥ switch                                     │
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ 2 reviews ─────────────────────────────────────────────────────────────────────────────────────────────┐
│● running – Read   api            #131   open    changes requested Refresh tokens         1.2G  just now│
│○ idle 2h          web            #88    draft   review required   New login page         310M    2h ago│
└────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Each row shows whether the review's session is running (or what holds it back:
paused at its `[review]` caps, or queued on `max_live`), the
repository and the PR number, each in a column of its own, the review's
size on disk, its state on
GitHub (open, draft, merged, closed) and where its review stands: GitHub's
review decision (approved, changes requested, review required), else what the
reviewers' latest reviews say (changes requested, approved, commented), else
awaiting review. GitHub is asked through `gh` on a thread of its own when the
tab is shown, on `r`, and every two minutes while it stays shown, so the list
never waits on it; until it answers a row shows `…`, and a lookup that fails
keeps what it said before. PR titles are shown as text, never run.

| Key | |
|---|---|
| `⏎` | open the review's session: attach to the review's agent, or resume or start it, as `⏎` does in the active tab |
| `v` | the review's PR in the review viewer, read from the review's checkout when it has one, else the repository's trunk |
| `e` | explore the review's worktree |
| `R` | ask an active review's agent to review again, as `jeet review <pr> --rerun` does (without opening the viewer); on an archived review, `a` brings it back first |
| `a` | archive the review, after listing the ignored files that would go; on an archived one, bring it back |
| `D` | delete the review, after listing any work that would be lost |
| `A` | show the archived reviews instead of the active ones, and back |
| `/` | filter by PR (`api#131`), the review's name or title |

A review has one worktree and one pull request, so the task keys `w`, `p`, `S`,
`n` and `o` do nothing here.

A review whose PR is merged or closed is archived for you, the way `a` (or
`jeet task archive`) archives it: its agent is ended, its worktree removed,
and its uncommitted work kept to come back if the PR is reviewed again. It is
done by each `jeet watch --once` poll and when the reviews tab hears from
GitHub, and never while the review's agent is mid-turn — running, asking you
something, or with no hooks to say — so it waits for the agent to go idle. A
review you bring back (`a`, or `jeet review`) after its PR was done stays
until you archive it; one that is merged or closed again later is archived
again.
Archiving deletes the files git ignores; unattended, that is allowed only for
build caches a build makes again (`target/`, `node_modules/`, `.venv/`,
`__pycache__/`, `dist/`, `build/` and the like). A review with any other
ignored file — a `.env`, local config — is left for you to archive, and the
watch log or the status line says why.

### Pull request history

A task remembers the pull requests that came out of it. When the tasks list
opens (and on `r`, and on `p` for the one task), each worktree is looked up on
GitHub in the background through `gh`: PRs from its branch, open or not; for a
detached checkout with commits of its own, the PRs that hold its HEAD. What
comes back is kept, so the history
outlives the branches — a merged PR still shows long after its worktree is
gone.

```bash
jeet task prs              # the current task's PRs, after asking GitHub for news
jeet task prs fix-login    # another task's
jeet task prs --offline    # only what is recorded
```

```text
STATE     ADDED REMOVED PR      TITLE                                            URL
merged     +212     -40 #128    Cache the index between runs                     https://github.com/acme/widget/pull/128
open         +3      -1 #131    Fix the flaky clone test                         https://github.com/acme/widget/pull/131
```

Without `gh` there is nothing to ask, and the history is what was recorded.
Run by an agent (the assistant or a task's), the table comes inside
`<github-data untrusted="true">…</github-data>`, with `<` and `>` in a
title shown as `‹` and `›`: a title is whatever its author typed.

### The agent

```bash
jeet task open fix-login                 # attach to the task's agent, starting it if needed
jeet task open fix-login --session <id>  # resume that session instead of the latest
```

Each task's coding agent runs in its own tmux session, `jeet-<slug>`, in the
task directory, so it keeps working when you detach (`Ctrl-Q`, or tmux's own `C-b d`) or close the
terminal. tmux is required (`brew install tmux`, `apt install tmux`); without
it `jeet task open` says so. Opening a task attaches to its session, starting
the agent first if it is not running: Claude Code resumes the task's latest
session, or starts a new one. Without a terminal, `open` leaves the agent
running detached and prints the task directory.

jeet starts Claude Code with hooks (passed with `--settings`, as a file
only you can read under `~/.jeet/run/<task id>/<session>.json`, replaced at
each launch, removed by a later launch once its session is no longer running,
and removed with the rest when the task is archived or deleted; nothing is written into the
task or a repository) that record what the agent is doing in
`.jeet/status/<session>.json`, and with permission to run `jeet worktree`,
`jeet ls` and `jeet report` without asking. Those settings also exclude
`jeet` from Claude Code's sandbox, should you turn it on: jeet writes under
`~/.jeet`, which the sandbox does not allow, and on Linux the sandbox hides
the processes jeet identifies its caller by. So jeet runs unconfined, and a
Bash command that chains other commands with `jeet` may run them unconfined
too. `jeet task ls` shows what the agent is doing: `starting`,
`running – <tool>`, `needs input – <question>`, `idle 5m`, or `not running`
once the tmux session is gone. An agent with hooks that has said nothing 15
seconds after starting is most likely held up at a startup prompt, and shows
`needs input – startup prompt?`. Tasks are listed most recently active first,
counting the agent's activity. An agent jeet has no harness for
(`agent = "aider"`) runs in tmux the same way, with no status: just
`running`. Archiving or deleting a task ends its tmux session.

Shift+Enter starts a new line in Claude Code instead of sending the message.
Inside tmux that needs tmux's `extended-keys`, which is off by default, so
jeet turns it on (with `set -s extended-keys on` and
`set -as terminal-features 'xterm*:extkeys'`) as it starts an agent. The
setting covers the whole tmux server, so jeet changes it only while it is at
tmux's default, and only once per server. If you switch it off again, it stays
off. If your `tmux.conf` sets `extended-keys` itself, jeet leaves it alone.
tmux older than 3.2 has no such setting, and jeet skips it. Agents that were
already running when jeet turned it on, and terminals that were already
attached, need a restart or a detach and reattach. A terminal that cannot
send Shift+Enter differently from Enter still sends the message on
Shift+Enter. In that case, use `Ctrl+J`, or type `\` and then press Enter, to
start a new line.

```bash
jeet task last fix-login         # the agent's last message, how its turn ended and when
jeet task last fix-login -n 3    # its last three messages, oldest first
jeet task last fix-login --json  # the same, for scripts
```

`jeet task last` reads the task's latest session from the agent's own record
of it (Claude Code's transcript), not from its tmux pane, and says how the
last turn ended: `finished`, `interrupted`, `API error: <what it said>`, or
`still running`, beside the task's status as `jeet task ls` shows it. An
agent jeet cannot read the record of says so.

```text
task:      fix-login (idle 4m)
session:   3f1c2a9e-5b7d-4e61-9a0b-2c4d6e8f0a1b
last turn: API error: Connection lost mid-response. The response above may be incomplete. (4m ago)

Pushed fix/login-cookie; opening the PR next.
```

Claude Code asks once per folder whether you trust it, before any hook runs,
and a task's agent waits at that question until you answer it. Trusting a
folder covers the folders inside it (checked with Claude Code 2.1.296), so
trusting `~/.jeet/tasks` once, by running `claude` there, covers every task
to come. Until then, jeet watches each agent it starts for the question for
about 30 seconds: the task shows `needs input – trust folder`, with a
notification unless you are attached to it, or, with
[`auto_trust`](#the-coding-agents-harness) on, jeet answers yes for you
(never for a task paused or stopped).

### Messaging an agent

```bash
jeet task send fix-login "also add tests"
jeet task new fix-x --repo api --prompt "Fix X; add a test for it"
```

`jeet task send` gets a message to the task's agent however it is doing. An
idle agent has it pasted in, as one bracketed paste (`tmux paste-buffer -p`),
and submitted. A message typed in key by key could arrive cut off: Claude Code
reads a long burst of keys as several pastes and keeps only the end. A busy
agent, one waiting on you, or one with text in its prompt that you have not
sent (the message would be joined to it) has it queued in `.jeet/inbox.jsonl`; when the agent
finishes, its Stop hook hands the queue over and it carries straight on. A
turn you interrupt with Esc has no Stop, so the queue is also typed in once
Claude reports it has sat idle at its prompt (after about a minute). A message
is never typed into a permission prompt, or into the dialogs Claude shows
before its first hook runs, which it would answer. An agent that is not
running is started with the message as its first prompt. `jeet task new
--prompt` starts the new task's agent on a brief the same way.

With Claude Code, each agent also has a *wake channel*: a small background
process (`jeet inbox watch`, started by the agent's own session-start hook)
that hands it what is queued for it as soon as it is queued, through Claude
Code's messaging socket, rather than waiting for its turn to end or typing
into its pane. An idle agent wakes and reads it; a busy one gets it at its
next tool call (or as its turn ends); one waiting on a prompt gets it once
the prompt is answered, and a message never answers the prompt. What you
send yourself, from your own terminal, to an idle agent is still typed in,
as your turn; unless you have unsent text in its input box, when it goes
through the channel as a message from another session, not over your text.
Everything else (messages from the assistant or a manager, reports,
answers, watch events, and what you send to an agent that is busy) arrives
as a message from another session, which Claude Code tells the agent cannot
approve anything, and `jeet task send` says "it gets it now, through its
wake channel". If Claude Code refuses the message (a repo's
`crossSessionInbound: "refuse"`, say), jeet notices it never arrived and
falls back to the queue and typing for that session, and tries the channel
again after 10 minutes, then after an hour. A third refusal ends that
session's watcher, and the next `jeet watch --once` poll repairs it: once the
agent is idle, has no unsent text and nobody has typed in its window for two
minutes, jeet ends it and resumes the same session in the same place (the
conversation is kept; the cost is one prompt-cache write), which starts a new
watcher. This is also how an agent started before the channel existed, or
whose watcher died, gets one. It happens at most once per session per day and
is logged in the assistant's `actions.log`; a refusal after a repair turns the
channel off for that session for good, and tells its parent. A Stop leaves
the queue to a working watcher; without one it hands over at most 2 KB at a
time. Like `jeet task restart`, a repair ends the session's process, so background
work it left running (background shells, subagents) ends with it. `jeet doctor wake` shows each running session's channel. `[harness]
wake = false` turns the channel off, and `wake_repair = "off"` keeps the
repair, the back-off and the Stop's cap from happening ([The coding agent's
harness](#the-coding-agents-harness)).

A message holds at most 8,000 bytes, which keeps what is typed in under
tmux's limit on a command; queued messages that do not fit with it wait for
the next hand-over. Text starting with `!`, `/` or `#`, which Claude would run
as a shell command, a slash command or a memory, is sent as `Message: …`; a
`--prompt` brief must fit in 8,000 bytes with that.

### Interrupting an agent

```bash
jeet task interrupt fix-login
```

`jeet task interrupt` ends the turn the task's agent is running, as Esc in
its window would, and leaves its session open for the next message. The key
goes to the agent's own pane, never to a pane you split off its window or a
window you opened in its session (as `jeet task send` types into it). It presses the key
once, and only while the agent is working or waiting on you
(at a permission prompt, Esc refuses the pending tool call): an idle agent,
one starting up or held at a startup prompt such as the trust dialog, one
whose prompt holds text you have not sent, or one waiting on a prompt, asking
you a question or showing its plan for approval while someone is attached to
its session (you may be typing an answer into it; once a question or plan is
answered, this lasts until the agent's next tool call or the end of its turn,
as Claude reports no answer) is left alone, since Esc there
would clear your text, rewind the conversation or exit the agent, and the
command says why nothing was sent. Once the key is pressed the task's
status reads idle (`Interrupted`), and the turn's time so far is charged to
its budget then; a turn you end with Esc yourself may read `running`, and be
charged, for about a minute, until Claude reports it sits idle. The assistant is yours to interrupt in its own window, and a
harness that cannot be interrupted is refused. Under the assistant it needs
`task.send` (which pre-approves it, as it does `jeet task send`), and an
interrupt that sent nothing is logged as `skipped`, with why, and not
announced; a task with a parent may not interrupt another.
To hold a task under the assistant and everything below it rather than end
one turn, use `jeet manager pause`, `resume` and `stop` ([Reporting to the
parent](#reporting-to-the-parent)): a pause interrupts every working agent in
the subtree and hands them nothing until it is resumed.

### Restarting or adopting an agent

```bash
jeet task restart fix-login [--force]   # `assistant`: each of the assistant's windows
jeet task adopt fix-login               # `assistant`: its windows jeet has no record of
```

`jeet task restart` ends the task's agent and starts it again in the same
tmux session, resuming its latest session, so its conversation carries on
with this jeet's hooks and settings. It is refused while the agent is
mid-turn, waiting on you, starting up, or has text in its prompt you have
not sent, unless you pass `--force`; and while the task is paused or
stopped. Anything the agent left running in the background (subagents,
workflows, `/loop` schedules) ends with it. `jeet task restart assistant`
restarts each running assistant window, leaving busy ones alone (and
failing, so you know) without `--force`; only you can restart the
assistant. Under the assistant it needs `task.open`.

jeet knows an agent by the process it launched for it ([who is
calling](#the-assistants-permissions)). An agent started by an older jeet,
or outside jeet, has no such record: `jeet task ls` marks it `(unanchored)`
and says how to fix it, the tasks list shows `unanchored` beside its status
(and the fix on the status line when it is selected), and its `jeet report`,
`jeet worktree add` and the like are refused. Restart it when it is idle, or,
to keep work in progress running, adopt it: `jeet task adopt <task>` shows
you the process running in the task's agent pane and records it as the
task's once you type the task's name back. Only you can adopt, from a
terminal of your own: it is refused to the assistant and every task, and
to anything running in a pane of jeet's tmux sessions, so an agent cannot
vouch for itself or another. An adopted agent keeps the hooks it was started
with, so its status reports to its parent may not work until it restarts.

### Reporting to the parent

```bash
jeet report progress|done|blocked|question "…"
```

A task the assistant makes (or one made with `--parent assistant`) reports to
it, and that is its only way up: tasks never message each other sideways. A
report is kept in the index and reaches the assistant fenced as the task's
claim, `<jeet-report … untrusted="true">`; the assistant's session context
lists each such task with its last report. Reports hold at most 2,000 bytes:
put detail in the pull request or a file and say where. `done`, `blocked` and
`question` start an assistant session when none is running; `progress`
waits for the next one. A task under a manager reports to that manager the
same way, fenced with its path in the tree (`path="assistant/backend/fix-x"`):
handed to the manager now through its wake channel (or, without one, typed
into its pane when it sits idle and queued while it is busy), and held in
its inbox when it is not running, unless the report is one that starts it.

```bash
jeet report ls                 # the open questions you hold
jeet report answer <id> "…"    # answer one
jeet report escalate <id>      # pass one up
jeet show r123                 # a report, whole (m45: a message)
```

A `question` or `blocked` report stays open until it is answered or the
task reports done, blocked or another question (a `progress` report, or
jeet's own `status` report, does not close it). Its parent holds it, and
either answers it or passes it to its own parent, which gets it fenced
with `via="manager …"` and may be started by it. Only
the holder answers or passes it on. Once the assistant passes one up it is
yours: jeet raises one desktop notification (`jeet: task w asks you
(#42)`), even with `[assistant] notify = false`. You may answer any open
question in the tree, from a terminal of your own (not a task's pane, nor
any pane of jeet's tmux sessions); `jeet report ls` there lists every one,
those with you first. The answer reaches the task that asked as
`<jeet-answer to="42" from="user">`, as a message does, and may start it;
each manager the question passed through, up to the one holding it, gets a
copy with the question. An answer is kept even if it is not delivered, and
is not sent again: the TUI's `i` shows it. No agent can pass its words off
as an answer: `jeet task send` and `jeet task new --prompt` from anyone but
you turn the `<` of a `<jeet-…` tag into `‹`.

jeet reports for the task too, as `status`: when its agent waits on a prompt
only you can answer, sits idle longer than `[managers] idle_report_mins`
(default 10; `jeet watch` and the assistant's own turns look), or ends without
reporting done. These never start an assistant session. `--goal` and `--done`
on `jeet task new` give the task a definition of done, shown to its agent at
the start of each session with what it may do and how to report. `--allow
task.create,worktree.add` names what it may do instead of `[managers]
worker_default`, refused unless its parent holds each one, and `--budget
1h,20turns` gives it a budget of busy time and turns (no `$`: cost budgets
wait). A budget is charged with the time the task's agent is busy (running,
or waiting on a prompt, while its agent's pane is alive: a window you opened in its session does not count) and a turn for each turn it ends, and counts toward
every budget above it too, even once the task is deleted (archiving or
deleting a task mid-turn charges the turn so far first). At 80% the parent is told and the task gets a
note; at 100% the task is paused and its parent told `blocked: budget`: its
turn ends with nothing more handed to it, and messages for it wait in its
inbox until the budget is raised and it is resumed.
At most `[managers] max_live` tasks under the assistant run at once (a
review agent counts toward `[review] max_live` instead, and spends only its
own `[review]` budget: [Review limits](#review-limits)): a
start past it is queued and the task's parent told, and it starts when a
running one's agent ends (a window you opened in its session does not hold
the slot) (its session's end hook starts it; on a harness without
that hook, the next `jeet watch` poll or TUI refresh does). `jeet task open`
from your terminal (or the TUI's open) starts it anyway, with a warning, and
with its brief and whatever waited for it, in the session you picked if any (an agent jeet cannot give a first prompt starts
without it, and what waited is typed in with the next message). A queued
task whose directory has gone is dropped from the queue with a warning.
`jeet manager ls [--json]` shows the tree under the assistant, `jeet manager
show <task> [--json]` a task's brief, grant (and what cut it), budget, run
state and children, and `jeet manager grant <task> <caps>|--none` replaces
its grant (`manager.grant`); what a task loses, the tasks below it lose too.
`jeet manager budget <task> +1h` raises a limit it has, and `=2h,40turns`
sets one (`manager.grant`), within what is left above it (a unit kept or
lowered is not checked); the 80% and 100%
reports start over, and a limit set at or below what the task has used
pauses it at once. Both write a manager's `BRIEF.md` again, as does each
of its sessions starting.

```bash
jeet manager new api --goal "Ship the v2 API" --done "PRs open, CI green" \
  --brief-file brief.md --allow task.create,task.send --budget 6h --max-children 4
```

`jeet manager new` (`manager.create`) makes a manager and starts its agent.
It goes under the assistant, or under `--parent` (a manager by its exact
name); made by a manager, it goes under that manager. `--goal`, `--done`,
`--allow` and `--budget` work as on `task new`, but an unasked grant is
`[managers] manager_default` and an unasked budget `[managers]
default_budget`; `--brief-file` adds a longer brief after the goal (the two
together at most 8,000 bytes; a regular file, and, named by the assistant
or a manager, one inside its own home and of one name, no hard link), and
`--max-children` lowers `[managers] max_children` for it. `--project <name>`
starts it from a [project](#projects): its skills are copied in and its
`AGENTS.md` becomes `PROJECT.md`, imported from the end of the manager's own
`AGENTS.md` (which stays as it is); no worktrees are added, as a manager holds
them read-only and asks for them when it wants one. The project is stored with
the manager, and a task or manager made under it (`jeet task new` from its
agent, `--parent`, or `n` on its node in the agents tab) starts from the same
project unless given `--project` of its own (and so with that project's
repositories as worktrees, a `--repo` for one of them with no `--branch` adding
nothing twice); the command line has no way to ask for none, but the `n` form
starts on the parent's project and takes "no project" at its word. A project
that can no longer be read costs the new task its project, with a warning. The
project's `[agent] args` are not applied to the manager itself, and relative
`@` imports inside the project's `AGENTS.md` do not resolve for a manager, whose
copy is `PROJECT.md`. A manager is
not made without `--goal` or `--brief-file`: its brief is all it works
towards. A manager sits at
most `max_depth` − 1 below the assistant, and needs a harness with recorded sessions, hooks,
continue-turn output, `interrupt_keys` and deny rules (Claude Code has them
all), and is not started on one configured since that lacks them. It prints its home and its place in the tree, then starts it as any
start is: now, queued over `max_live`, or held while its parent is paused.
If its agent cannot start, the manager is kept and the command fails.
A task made under a parent, by anyone, may not have a control character,
a line break, a bidirectional, zero-width or other invisible format
character (such as Unicode tag characters or variation selectors), `<` or `>` in its
name, nor more than 100 characters: other agents read it, and the user sees
it. The assistant's session summary lists a task a manager
made by its slug alone, since its name is that manager's text; `jeet task
ls` and `jeet task last --json`, run by an agent, show only the names of
tasks with no parent, made under the assistant, or made under itself
(others get `-` and `null`), and the other `jeet task` commands name such
a task by its slug in what they print.

A manager's home is a task directory with a manager's role in place of
the worker's skill: `AGENTS.md` (plan, delegate, report; stay in your
subtree; worktrees are read-only; the limits are cooperative), the
`jeet-manager` skill, `notes/worklog.md`, `todo.md`, which it edits by
hand (`jeet todo` stays the assistant's list), and `BRIEF.md`: its goal,
done-when, budget and what it may do, fenced as its parent's brief and
read-only. Each of its sessions starts with that brief, the tasks below it
and their last reports, its budget, its todo list and what is queued for
it. The assistant learns of managers from its `jeet-managers` skill.

```bash
jeet manager pause fix-x       # interrupt its turn, and every task's below it
jeet manager resume fix-x      # hand over what waited, and let it start again
jeet manager stop fix-x        # end its agent and every one below it
jeet manager stop --all        # every task under the assistant, and its reviews (only you)
```

These need `manager.stop`. A paused or stopped task, and every task below
it, is handed nothing: messages and reports to it wait in its inbox, its
turn's end hands it nothing, and nothing starts it; `jeet task open`
attaches to it while its agent still runs, and otherwise says to resume it.
Pause interrupts the turn of each of their agents that is working (one
Escape, never over what you typed), and says, task by task, what it did; an
agent started with a prompt or typed a message in the last few seconds,
whose turn has not begun yet, is waited for and then interrupted. One batch
of messages its wake channel handed a working agent just before, which
Claude had not yet shown it, may still start one more turn; that turn's end
hands it nothing. Stop ends their
tmux sessions, and any legacy review job running for them or in their
worktrees (which pause cannot interrupt), and their ending is not reported.
`--all` also ends the agent and any legacy job of each review the assistant
asked for (one you ran yourself after it asked counts too, as the assistant
stays its asker) and takes it off the `max_live` queue; a review holds no
stop, so it starts again the next time it is asked for;
Resume is refused while a
task above holds it, while its budget is used up (`manager budget` first),
and to anyone ranked below whoever paused or stopped it: you, then the
assistant, then a manager above, so what you stop only you resume. Once
resumed, a task a message was held for starts with what waited for it if
its agent is not running, and one still running is handed it; reports
(progress, idle and the like) never start it, and wait for its next start.
`task new --prompt` under a paused manager makes the task and keeps its
brief for then.

### Archiving and deleting

```bash
jeet task archive fix-login          # -f: without asking about ignored files
jeet task unarchive fix-login
jeet task delete fix-login           # -f: without asking about what is lost
jeet task delete api -r              # -r: every task under it too (archive, unarchive as well)
jeet task delete api --reparent      # api alone: the tasks under it go to its parent
```

**Archive** frees the disk and keeps the work: each worktree's uncommitted
changes — staged, unstaged and untracked — are saved under
`refs/jeet/stash/<task-id>/<dir>`, its HEAD is pinned under
`refs/jeet/head/<task-id>/<dir>` so not even a commit on a detached HEAD is lost
to gc, and only then is the worktree removed. Files git ignores (build output,
`.env`, local config) are not kept: removing the worktree deletes them. So
archive names them first and asks — the tasks list in its confirm, the CLI on
the terminal — and without a terminal it needs `--force`. The task directory itself
stays, so an agent's sessions still find it. Archive first ends the task's
agent and any review job running for it or in its worktrees (a `jeet review`
still running is stopped, its result never written), and is refused from
inside such a job. **Unarchive** puts every worktree
back on its branch (or detached at its commit) with its changes reapplied, then
drops the refs.

**Delete** removes everything the task owns: its agent and any review job
running for it or in its worktrees, its worktrees, the branches it
created (never ones that existed before), its `refs/jeet` refs and its
directory, with everything else on its canvas. It first lists what would be
lost — uncommitted changes, commits that would be left on no branch or remote,
and any git checkout on the canvas that is none of the task's worktrees (a
clone made there, or a worktree of a repository outside the store) — and asks;
without a terminal it needs `--force`.

**A task with tasks under it**, a manager's say, is not deleted while any
task sits under it, archived ones included, nor archived while one under it is active, nor unarchived under
an archived parent or past its parent's `max_children`. `--recursive`
(`-r`) takes the tree instead: archive and delete list what every task in
it would lose, ask once (`--force` without a terminal), end every agent in
it, leaves first, then archive or delete each task, leaves first; delete
removes their rows together at the end. `unarchive --recursive` brings the
archived tasks under it back top-down, each under a parent already back
and held to its parent's `max_children`. A failure
part-way leaves what is done done, and running the command again finishes
the job. In the tasks list, `a` and `D` on a task with tasks under it are
recursive, and `a` in the archived list brings its tree back.

**`delete --reparent`** deletes a manager alone and moves the tasks under
it, archived ones too, with their own tasks, to its parent; it conflicts
with `--recursive`, and a task with nothing under it is refused (a plain
delete takes it). It needs what delete needs (`task.delete`, held, for an
agent). It asks once, listing what the manager would lose and which tasks
move where (`--force` without a terminal), and checks every rule of the
tree before ending the manager's agent and removing its files, then again
as it moves the tasks, all at once: the new parent's `max_children`
(archived tasks do not count), and that no task's pause or stop would
change. A manager whose pause holds the tasks under it is refused ("pause
or resume first"), since deleting it would free them. Each moved task
keeps only what it could use where it was and its new parent lets
through, so nothing it may do changes, and a capability its grant drops is
named. The reports sent to the manager are kept, as sent to its parent;
what it used still counts toward every budget above it. The new parent is
told of each task, in jeet's words, with the open question it now holds,
and is handed each question the manager held, as passed on by jeet;
a running task is handed its new brief, and every manager moved gets its
`BRIEF.md` again. If the move is refused after the files are gone, the
row is kept, and running the delete again once the cause is gone finishes
the job. It is CLI only.

## The assistant

```bash
jeet chat                          # resume its most recently active session
jeet chat --new [name]             # start a fresh one
jeet chat --pick                   # ⏎ attach  n new  R rename  x end  * primary
jeet task send assistant "…"       # message it, as you would a task's agent
```

The assistant is one Claude Code organiser for all your work: it keeps a
running worklog and your todo list, and sets up, steers and tidies away tasks
through the jeet CLI, within the permissions you give it (below). Its home is
`~/.jeet/assistant`, set up by the first `jeet chat`: an `AGENTS.md` on its
role (with a `CLAUDE.md` importing it, as in a project), a `jeet-assistant`
skill with the commands it uses, `notes/worklog.md` and `todo.md`. They are
written only when missing, so your edits and its own survive. A home from
before `AGENTS.md` has its `CLAUDE.md`, edited or not, moved to `AGENTS.md`
behind the import, never written over.

One home, any number of sessions. Continuity lives in those files rather than
in any one conversation, so a fresh session or a `/clear` loses nothing that
matters, and two can run at once — one planning your week, one triaging
reviews. Each runs in a window of its own in the tmux session
`jeet-assistant`, with the same hooks and status as a task's agent, and with
`JEET_ASSISTANT=1`. Every session starts with a summary of the active tasks,
their open pull requests and your own that `jeet watch` follows, with their
checks, the GitHub events the watcher saw in the last week, such as review
requests (all fenced as data written by others), the tasks it made with their
last report (fenced as their claims), the todo list and any messages and
reports queued for it. Tasks it makes report back to it (see "Reporting to
the assistant").

Messages for the assistant go to the session you marked primary (`*` in the
picker) while it runs, else to the most recently active running one, by the
same idle/busy/not-running rules as a task's agent; with none running, a
session called `events <date>` starts with them. One queued for a busy session
waits for that session, not whichever finishes first. Any task's agent can
send one, so each reaches the assistant fenced as untrusted, with who jeet
thinks sent it (`task <slug>`, or `the command line`), and the assistant is
told to read it as a request to weigh, never as your instructions. To tell it
something as yourself, talk to it in its window.

A `/clear`, a fork or a resume inside a session's window carries on in a new
session, which follows on from the one before it in that window, keeps its
name and its queued messages, and is the one `jeet chat` finds there.

The assistant is not a task: it is not listed, opened, archived or deleted
like one, and no task can be called `assistant`. The tasks list pins it above
the tasks instead, where `c` talks to it, `C` picks a session, `t` opens the
todo list and `i` its inbox and activity.

### Todo

```bash
jeet todo add "fix the cookie" -p 1 --task fix-login   # in priority order
jeet todo ls [--json]                                  # numbered; also plain `jeet todo`
jeet todo done cookie [--undo]                         # by its text, or a part only it has
jeet todo rm 3                                         # or by number
```

`~/.jeet/assistant/todo.md` is the list: markdown, one checkbox per item, most
important first, with an optional `!p1`/`!p2`… priority and `@task-slug` links.
Everything else in it is kept as written. `jeet todo` changes it under a lock
and writes it whole, so the assistant's sessions never lose each other's
edits. An item is best named by its text: a number is where it was when
listed, and an item added above it since moves it down. The assistant needs
no permission for its own list.

### Watching GitHub

```bash
jeet watch --once        # poll now: what launchd and the tasks list run
jeet watch install       # every `interval_secs`, with launchd (macOS)
jeet watch uninstall
jeet watch status        # installed? last poll, what it follows
```

`jeet watch --once` asks GitHub, through `gh`, for your unread notifications
(with `If-Modified-Since`, so a poll with nothing new costs nothing), the
checks on your open pull requests, and the items of the GitHub Projects you
name. What is new since the last poll becomes an event: `review_requested`,
`mention`, `comment`, `assigned`, `ci_failed`, `ci_passed`, `pr_merged`,
`pr_closed` or `project_item_changed`. The first poll of each only notes what
is there, so turning the watcher on is not a flood. Each poll also archives the
reviews whose PR has been merged or closed, once their agent is idle
([The reviews tab](#the-reviews-tab)).

Rules decide where each event goes; the first that matches wins, and an event
none matches is a desktop notification:

```toml
[watch]
repos = ["acme/*", "me/jeet"]                 # only these (default: all)
projects = [{ owner = "acme", number = 3 }]   # needs `gh auth refresh -s read:project`
interval_secs = 120

[[assistant.rules]]
on = ["review_requested"]
repos = ["acme/*"]
action = "assistant"   # hand it to the assistant, which acts within its permissions

[[assistant.rules]]
on = ["ci_failed"]
action = "task"        # tell the task whose PR it is (by its PR history); else the assistant

[[assistant.rules]]
on = ["comment"]
repos = ["acme/api"]
action = "manager:backend"   # hand it to the manager with that slug; else the assistant

[[assistant.rules]]
on = ["mention"]
action = "notify"      # just a notification
```

`on` and `repos` may be left out to match anything. Events for the assistant
go to its sessions by the rules above (primary, else most recent, else a new
`events <date>` session), several to a message; an event for a task goes to
its agent the way `jeet task send` would send it (never to a manager, which
may have a worker's PR in its history from reading it). A `manager:<slug>`
rule hands its events to the active manager with exactly that slug (not its
name), several to a message under a line in jeet's words, and may start it
through the start gate, as an event for a task may; the slug is looked up at
each poll, and one with no active manager of that slug sends its events to
the assistant, with a warning in the watch log. Either way the event is
fenced as `<github-event untrusted="true">`, since titles and comments are
written by other people: data to weigh, never instructions. One a task or a
manager cannot be given goes to the assistant; one the assistant cannot be
given is notified, as is one held for an agent jeet could not start. Each
event, and where it went, is appended to
`~/.jeet/watch-events.jsonl`.

`jeet watch install` writes `~/Library/LaunchAgents/dev.jeet.watch.plist`,
which runs `jeet watch --once` every `interval_secs` with the `PATH` you
installed it from, logging to `~/.jeet/watch.log`, and loads it with
`launchctl bootstrap`. Elsewhere it installs nothing, and prints a systemd user
service and timer to set up instead. The job never starts a tmux server: one
started there would hand launchd's bare environment to every pane it ever
made, yours included. While no tmux server is running, an event for an agent
waits in its inbox until the agent next runs, and is notified now. With a
`[watch]` section, the tasks list also polls while it is open. Installing or removing the watcher takes the
assistant the `watch.install` capability; polling takes none.

## Importing

```bash
jeet import                          # scan_roots, jeet's old directories, every indexed trunk
jeet import ~/Projects/api ~/src     # just these
jeet import --yes                    # everything found, one task each, no questions
jeet import --remove-originals       # and remove each original once its task has a copy
```

`jeet import` is the way in for work that lives outside jeet. It looks for git
checkouts and lists what it would do:

```text
jeet: into the store, cloned from origin:
  github.com/acme/api                      git@github.com:acme/api.git
jeet: to import, one task each unless grouped:
  1  github.com/acme/api   feat/login · 2 unpushed commits, 3 uncommitted changes · copied from ~/Projects/api
  2  github.com/acme/api   parked · 1 unpushed commit · copied from (branch)
  3  github.com/acme/web   old-feature · moved from ~/.jeet/worktrees/github.com/acme/web/old-feature
Import which? ⏎ all, one task each · `1+3` puts 1 and 3 in one task, `1+3=name` names it · …
```

- **A repository outside the store** is cloned into it from its origin. Each
  of its branches and checkouts with work in it — commits origin does not
  have, or uncommitted changes — becomes a task whose worktree holds the same
  work: the branch is fetched across from the original, and uncommitted
  changes (staged, unstaged and untracked; not ignored files) are reapplied on
  top. Work on the default branch comes across detached at the same commit,
  since the store's trunk has that branch. Branches with nothing origin lacks
  are left out: `jeet worktree add <repo> --branch <name>` gets those.
- **A worktree of a store trunk outside any task** — what `jeet checkout` and
  `jeet exec --ephemeral` made before tasks — is jeet's own, so it is moved
  into a task as it is with `git worktree move`.
- **Claude Code sessions** started in a checkout are copied to its task, so
  `jeet task open` can resume them.

The originals are only read. `--remove-originals` removes, after asking (or
with `--yes`), each linked worktree and parked branch whose task holds an
exact copy and that has not changed since. A worktree holding ignored files
stays, since those were not copied, and a repository's main working tree is
never removed. Running import
again skips work already in a task. Everything it says goes to stderr.

`jeet cd` is **not** a binary subcommand — it only works via the `init-shell` wrapper. Use `jeet exec` for subshells or `jeet path` in scripts.

## Tab completion

### Homebrew and apt

`brew install jeet` and `apt install jeet` ship **dynamic** completion scripts that call back into `jeet` at tab time, so repo filters, task and worktree names, and `--branch` values come from your index (not directory listing).

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

## Moving to tasks

Every checkout now belongs to a task, and the commands that made or registered
checkouts anywhere else are gone. Run `jeet import` once: it brings repositories
you registered with `adopt` or `scan` into the store, and moves what `checkout`
and `exec --ephemeral` left under `~/.jeet/worktrees` and `~/.jeet/ephemeral`
into tasks.

| before | now |
|--------|-----|
| `jeet adopt <path>`, `jeet scan` | `jeet import [path]` |
| `jeet checkout <branch>` | `jeet worktree add <repo> --branch <branch>` in a task |
| `jeet exec <repo> --branch <b>`, `jeet path <repo> --branch <b>` | `jeet worktree path <name>` |
| `jeet exec <repo> --ephemeral` | `jeet task new <name> --repo <repo>`, then `jeet task delete` |
| `jeet sessions` | `jeet task open <task> --session <id>`; `ctrl-s` in the explorer, inside a task |
| `jeet worktree clean`, `jeet worktree rename` | `jeet worktree rm`, `jeet task delete` |

## Migration from v0.1.x

| v0.1.x | v0.2.0 |
|--------|--------|
| `jeet list` | `jeet ls` |
| `jeet worktree list` | `jeet worktree ls` |
| `jeet cd` | `jeet exec` (subshell) or `jeet cd` via init-shell |
| `jeet cd --print` | `jeet path` |

## What's new in v0.13

Reviews stop being tasks: a review is its own thing, with its own table in
the index.

- **A review is not a task.** One per pull request (a repository and a PR
  number), with exactly one worktree and one session, flat: no parent, no
  tasks under it, no brief, grants or project. `jeet review` finds or makes
  it as before, and `jeet task open|archive|unarchive|delete|path|send|
  restart review-<repo>-<number>` still take its name. It is in no task
  list, tree, manager or project; the [reviews tab](#the-reviews-tab) lists
  it, and there `a` archives it and brings it back (`A` toggles the archived
  ones) and `D` deletes it, for every review.
- **Limits apply to every review.** `[review] time` and `turns` held only the
  reviews the assistant started; now they hold every review, yours included,
  given afresh each time a review is asked ([Review limits](#review-limits)).
- **Who a review reports to.** A review the assistant asked for reports to
  the assistant (not as its child); one you asked for reports to no one.
- **One gate for every review agent**: read-only jeet, the worker deny rules
  (no push, merge, approval or PR comment), and `jeet report` only when a
  task asked for the review ([The review agent](#the-review-agent)).
- **`jeet task send` and `jeet task restart` reach a review's agent**, for you
  and for the task that asked for the review; nobody else may.

v0.13.0: reviews leave the tasks table (#99, #102, #103).

**Upgrading to v0.13.0:** the index moves to **schema v9** the first time any
v0.13 command opens it (from v7, two steps run one after the other: v8 moves
reviews out of the tasks, v9 puts a review's `[review] command` job on its
row), and v0.12 and older refuse the index after that, so upgrade every jeet
you run at once, the one `jeet watch` runs included. Before anything changes,
the index is copied to `$JEET_HOME/backups/index-v7-<unix time>.db`
(`~/.jeet/backups/` by default; a second copy the same second gets `-2`), and
the migration is refused if the copy cannot be made. The copy
is a whole index of the old schema: it is what an older jeet opens, if you go
back.

- Each review task becomes a review, keeping its slug (so its tmux session
  `jeet-<slug>`, its hooks and its `$JEET_TASK_ID` carry on), its worktree,
  its session, its limits and use, its place in a `max_live` queue, and its
  archived or active state. A review under the assistant becomes one the
  assistant asked for.
- **Reports of a kept review are kept**: its `done`, `blocked` and other
  reports to the assistant stay in `jeet report ls`. Only those of review
  tasks that were superseded or dropped go, and any that were sent to a
  review task.
- **Only one review per pull request survives.** Where several review tasks
  were for one PR, the newest active one is kept (the newest, when all are
  archived). The others' worktrees are archived the
  way `jeet task archive` does it, into `refs/jeet/head/<old-task-id>/…` and
  `refs/jeet/stash/<old-task-id>/…` in the repository's trunk (the old task id
  is in the log), or, when one holds ignored files that are not build caches (a
  `.env`, local config), left in place on disk untouched. Their task
  directories stay on disk; jeet deletes nothing. A review task with no PR
  recorded gets no review, and its worktrees are handled the same way.
- Archiving those worktrees happens at the first command you run from your
  own terminal after the upgrade (not from a hook or an agent's shell), and
  says so on stderr in one line. What it did, a line each, is in
  `$JEET_HOME/logs/migrate-v8.log`; a worktree it could not archive then is
  tried again at the next command, up to three times, then left in place.
- **Review jobs an older `jeet review` left under `~/.jeet/reviews`
  (`<host>/<owner>/<repo>/<number>.json`, the state of a `[review] command`
  run) become reviews**, at the first commands you run after the upgrade
  (in a task's shell too, not from hooks or jeet's own background commands),
  and are no longer shown read-only in the reviews tab. Each job
  whose pull request has no review gets one: **archived** when the job
  finished or was lost (its process is gone), **active** only while the job
  still runs, and then archived like any review once its PR is merged or
  closed. It has no worktree: the one the job ran in is never touched. A PR
  that already has a review keeps it, and gets the file's job only if it has
  none. A job that is not running is stored as ended, so nothing can run it
  again. These reviews can be brought back, archived and deleted like any
  other. **The files are left where they are, untouched; jeet deletes none of
  them.** Each file is looked at once: its outcome is recorded in the index,
  so a review you delete later is not made again from its file. A file that
  cannot be read is left and logged; one whose review cannot be made is tried
  up to three times, then left. Each file's outcome is a line of
  `$JEET_HOME/logs/migrate-v9.log` (adopted, put on a review, skipped, or
  left), and a one-line summary goes to stderr only by a run that adopted or
  gave up on something. A job still running during the upgrade is adopted as
  active, but an older runner records its end in the old file, which is not
  read again: its row shows "stopped without finishing" once the process
  exits. Archiving or deleting such a review warns that its job keeps
  running; jeet did not start it, so it does not stop it.
- The `<task>/.jeet/review.json` and `brought-back` files are folded into the
  review and no longer read.
- In the reviews tab, `⏎` opens the review's session and `R` reruns it; `w`,
  `p`, `S`, `n` and `o` do nothing (a review has one worktree and one PR),
  and a review is no longer "paused by hand" or
  "stopped" from a tree: it is paused only at its caps. `jeet manager stop
  --all` still ends the agents (and legacy jobs) of the reviews the
  assistant asked for, and drops them from the `max_live` queue.

After upgrading:

1. **Upgrade every jeet at once**, including the one `jeet watch` runs (on
   macOS, `brew upgrade jeet` updates the `/opt/homebrew/bin/jeet` launchd
   runs): an older one refuses the index once it is at v9.
2. **Run any jeet command from your own terminal**, `jeet ls` say, so the
   migration and both post-steps run there and print their one-line
   summaries.
3. **Read `$JEET_HOME/logs/migrate-v8.log` and `migrate-v9.log`** for what
   moved, what was archived and what was left in place; the index as it was
   is in `$JEET_HOME/backups/`.
4. No config change and nothing to regenerate.

## What's new in v0.12

Reviews become agents: `jeet review` hands the PR to the review task's own
agent, working in its tmux pane, with limits of its own and a tab of its own
in the tasks list.

- **The review agent**: `jeet review <pr>` starts the review task's agent
  (your harness, Claude Code) in the task's tmux pane with a first prompt,
  `[review] prompt`, instead of a headless background job. `⏎` on the task,
  or `jeet task open review-<repo>-<number>`, shows it at work, and you can
  talk to it there. One review is one task, one worktree and one session:
  a new head commit or `--rerun` sends the prompt again into that session,
  and the same commit is not asked twice ([The review
  agent](#the-review-agent)).
- **Review limits**: `[review] max_live`, `time` and `turns`, apart from
  `[managers]`. Review agents no longer take `[managers] max_live` slots or
  spend a manager's budget ([Review limits](#review-limits)).
- **The reviews tab**: a new tab in the tasks list, after agents, shows every review
  with its session's status, the PR's state on GitHub and where its review
  stands; `⏎` opens a menu to view the diff (`v`) or open the review's
  session (`s`) ([The reviews tab](#the-reviews-tab)).
- **A subagent finishing late no longer leaves an agent busy**: a subagent
  or background agent ending after its session's turn used to mark the
  idle session running again, so messages to it waited, queued as busy,
  until its next turn.
- **The tasks screen no longer grows while you are away**: its pollers
  kept queueing updates while you were attached to an agent or in the
  explorer, about 3 GB over a day; they now hold off once 256 are waiting.
- **No tmux call waits forever**: every tmux call but `attach` is given up
  on after 20 seconds.

**Upgrading to v0.12.0:** the index stays at schema v6, so there is no
migration and v0.11.0 still opens it. The one new piece of state is a small
file per review task, `<task>/.jeet/review.json`, recording the last head
its agent was asked to review. Review jobs recorded under `~/.jeet/reviews`
by an older `jeet review` still load, show in the tasks list and the new
reviews tab, and their sessions are picked up as before.

- **`jeet review` now starts an agent even with no `[review]` config.**
  Before, with no `[review] command`, it only checked the PR out and opened
  the viewer; pass `--no-agent` (or `--no-command`) for that.
- **`[review] command` is deprecated.** It still works: set, it runs
  headless in place of the review agent, as before, and `jeet review` warns.
  Once a review's agent has been asked, the command is no longer run for
  that review (`jeet review` says so) and the agent is asked instead.
  Remove the command to switch to the agent.
- **New keys in `[review]`**, all optional:

  ```toml
  [review]
  prompt = "…"    # placeholders {pr}, {url}, {worktree}; the default asks the
                  # agent to review PR {pr} in its worktree, and not to push,
                  # merge, approve or comment unless asked
  max_live = 2    # review agents running at once, yours included
  time = "1h"     # busy time each review in the tree may take
  turns = 20      # turns each review in the tree may take
  ```

  These are checked when used, with a `[review] <key>` message, and the
  rest of config.toml still loads: `max_live = 0` refuses starting any
  review; `turns = 0`, `time = "0m"` or a `time` that is not a time refuses
  asking a review in the tree. v0.11.0 ignores these keys.
- **Reviews have their own limits now.** They no longer use `[managers]
  max_live` slots or manager budgets: a review below a manager spends
  nothing of its budget. At most 2 review agents run at once, **including
  your own `jeet review`**; a third waits until one ends. To keep the old
  behaviour of no review cap, raise `[review] max_live`. `jeet task open`
  from your terminal still starts one past the cap, with a warning.
- **Time and turn caps apply to reviews in the tree** (the ones the
  assistant starts; a review task already made at the top level stays
  there, uncapped): each review asked gets `[review] time` and `turns`
  afresh, is warned at 80% and paused at 100%. A review task's budget is
  `[review]`'s: `--budget` on one is refused, and one set with `jeet
  manager budget` is replaced at its next review. **Your own top-level
  `jeet review` is held by `[review] max_live` only**: like any top-level
  task it has no budget, so `time` and `turns` never pause it. A task with
  a parent may not run `jeet review`.
- **Queue messages name the cap that held a start**, e.g. `waits on
  [review] max_live (2/2 review agents live)` or `queued on [managers]
  max_live, 1/1 live` (a `status: queued` report body). Scripts matching
  the old `queued, n/m live` need updating. The tasks list's header still
  counts review agents in its `live n/max`, against `[managers] max_live`.
- **In the tasks list, `⇥` now cycles active → agents → reviews →
  archived** (`⇧⇥` back): reaching archived from active takes three presses.
- A session left marked running by a late subagent before this release
  clears at its next turn's end; nothing repairs its status file sooner.
- The assistant's and managers' `AGENTS.md` and skills are unchanged by
  this release; nothing needs regenerating.

v0.12.1: the active tab becomes a tree ([The tasks list](#the-tasks-list)).

- **Walk the tree**: the active tab opens on the managers and tasks
  directly under the assistant (and any with no parent, or whose parent is
  no longer active). `⏎` or `→` on a manager, even an empty one, or on a
  task with tasks under it, goes into it; `←` or `⌫` backs up a level, the
  cursor on the one you left, as in the explorer.
- **A breadcrumb** over the list says where you are (`assistant › backend
  › …`), eliding the middle to `…` when it is too wide.
- **Two-line rows**: a `manager` or `task` tag, the name, status,
  worktrees, size and age; under the name, dimmed, the running agent's
  latest prompt, else its last report. The archived tab stays a flat list
  but uses the same rows.
- **Managers roll up their subtree**: a manager (or any task with tasks
  under it) shows the most urgent state of its own and everything below it,
  with how many below are so, as in the agents tab (needs you over running
  over idle), e.g. `● running (1)` while its own chat is idle.
- **The filter searches every level**: each match shows its path under its
  name. Once the filter is set with `⏎`, `⏎` or `→` on a manager match goes
  into it and drops the filter, and `←` or `⌫` drops the filter, the cursor
  on the match, on its level.
- **`ctrl-a` opens the agent under the cursor** in the active tab, the
  explorer's agent key: a manager's own agent, now that `⏎` goes into it.

**Upgrading to v0.12.1:** no schema change (stays v6; v0.12.0 and v0.11.0
still open the index), no config change, nothing to regenerate.

- **`⏎` on a manager, or on a task with tasks under it, now goes into it**
  instead of attaching to its agent; use `ctrl-a` for that. `⏎` on any
  other task still attaches to its agent.
- **Tasks under a manager are no longer listed at the top level**: go into
  the manager, or type `/` to find one at any level.
- The ` ↳ parent` marker and the `(manager)` suffix are gone; the tag and
  the breadcrumb say the same.
- The `manager` (orange) and `task` (periwinkle) tag colours need a
  256-colour terminal.

v0.12.2: reviews keep to the reviews tab ([The reviews
tab](#the-reviews-tab)).

- **Archived reviews leave the main list**: the reviews tab lists the
  active reviews (and any review job with no task); `A` switches it to the
  archived reviews (`▪ archived`) and back.
- **Merged or closed PRs archive their review**: each `jeet watch --once`
  poll, and the reviews tab, while it is shown, when it hears from GitHub,
  archives a review whose PR is merged or closed, as `a` does: agent
  ended, worktree removed, uncommitted work kept for when it is brought
  back. Never while its agent
  is mid-turn; and, since archiving deletes ignored files, only when those
  are build caches (`target/`, `node_modules/`, `.venv/` and the like). A
  review holding a `.env` or other local file is left for you, and the
  watch log or status line says why. A review you bring back after its PR
  was done stays until you archive it.
- **A size column** in the reviews tab, and **the repository and the PR
  number in columns of their own** (`api` · `#131`, no longer `api#131`).
- **Reviews show in the reviews tab alone**: never in the active or
  archived tabs or their counts, the filter's results, the agents tab (nor
  its roll-ups), or `jeet task ls` (with or without `--archived`). The
  reviews count is of active reviews. Selecting a review from elsewhere
  opens the reviews tab on it.

**Upgrading to v0.12.2:** jeet migrates the index to schema v7 the first
time it runs, and an older jeet refuses it after that (copy
`$JEET_HOME/index.db` first if you may go back). The migration only marks
as reviews the tasks that hold a PR checkout (only a review makes one) but
were recorded as plain tasks; nothing else changes. No config change,
nothing to regenerate.

- **Reviews are gone from the active, archived and agents tabs and from
  `jeet task ls`**: find them in the reviews tab (`A` for archived ones).
  A manager with only reviews under it can no longer be gone into.
- **Reviews of PRs already merged or closed are archived at the next
  `jeet watch --once` or reviews-tab refresh** once their agent is idle,
  so expect a batch of them to go the first time. Keep one by bringing it
  back (`a` in the archived reviews).
- `jeet watch --once` now asks GitHub about each active review's PR on
  every poll, one `gh` call each.

v0.12.3: `⏎` on a manager opens its agent, and the assistant's line says
less ([The tasks list](#the-tasks-list)).

- **`⏎` on a manager opens its agent** in the active tab, as `ctrl-a` does;
  `→` goes into it, `←` or `⌫` back out. With a filter set, `→` on a
  manager among the matches goes into it. `⏎` on a task is unchanged (one
  with tasks under it is still gone into).
- **The assistant's line shows how many sessions it has and the active
  one**, the session `c` opens, with what it is doing. The running
  sessions, the inbox and todo counts and the key hints are gone from it
  (`i`, `t` and `C` still open them); a red `N unanchored` follows only
  while any of its windows were not started by jeet.
- **Dim text stays readable on the selected row**: a task's snippet, and
  the dim cells of the agents, reviews, explorer and session-picker rows,
  turn light grey under the highlight instead of vanishing into it.

**Upgrading to v0.12.3:** no schema change (stays v7), no config change,
nothing to regenerate.

- **`⏎` on a manager no longer goes into it**: use `→`.

v0.12.4: a projects tab, and `A` in place of the archived tab
([The tasks list](#the-tasks-list), [The projects tab](#the-projects-tab)).

- **The projects tab**, after reviews (`⇥` cycles active → agents →
  reviews → projects), lists each repository that an active task or manager
  has a worktree of. Tasks with no worktree are listed under `(no repo)`.
  `→` goes into a project, showing its managers and tasks with the active
  tab's tree keys: `→`/`←`/`⌫` move in and out, `⏎` opens, and `e w p v a D S n`
  work on the selected task. `A` shows the projects with only archived tasks.
- **The archived tab is gone.** `A` in the tasks list switches between
  active and archived tasks, as it already did in the reviews tab. `a`
  unarchives and `D` deletes there. A hint for `A` sits at the bottom of the
  tasks, reviews and projects lists, and the first tab reads `archived N`
  while the archived tasks are shown.
- **A project column** in the tasks list, from about 120 columns, shows the
  repository of the task's first worktree, `+N` for others, or `(no repo)`.
  It is the first column dropped on narrower terminals.

**Upgrading to v0.12.4:** no schema change (stays v7), no config change,
nothing to regenerate.

- **The archived tab is gone**: press `A` in the tasks list instead of
  `⇥`-ing to it.

v0.12.5: projects are jeet's own projects again, managers can start from
one, and agents started by `jeet watch` are anchored
([Projects](#projects), [The projects tab](#the-projects-tab)).

- **The projects tab groups by jeet project** (the one a task was started
  from with `--project`), not by repository. A manager is listed under its own
  project and under each project an active task below it was started from.
  Tasks with no project are left out. `A` shows the other projects: those
  with only archived tasks, and those with no tasks at all.
- **The project column shows the task's project**, or nothing when it has
  none, instead of the repository.
- **`jeet manager new --project <name>`** stores the project on the manager,
  copies its skills, and copies its AGENTS.md in as `PROJECT.md`, linked from
  the manager's own AGENTS.md. Tasks and managers made under it default to
  its project. An explicit `--project` names another. The agents tab's `n`
  form preselects the parent's project. Managers get no worktrees of the
  project's repositories and none of its `[agent] args`.
- **Agents that `jeet watch` starts are anchored.** From launchd or systemd,
  with no UTF-8 locale, tmux garbled the ids jeet reads back. The window
  started, but jeet never recorded it, so it showed as unanchored and its
  jeet commands were refused. Every tmux call now passes `-u`.

**Upgrading to v0.12.5:** no schema change (stays v7), no config change,
nothing to regenerate.

- **Upgrade the binary the watch job runs.** On macOS that is
  `brew upgrade jeet`, which updates the `/opt/homebrew/bin/jeet` that
  launchd runs.
- **Then `jeet task restart <task>` each unanchored window**, the
  assistant's included (`jeet task restart assistant`). This keeps the
  conversation. Windows that were already unanchored stay that way until
  restarted.

## What's new in v0.11

The wake channel: phase 2a of [hierarchical
agents](docs/design/agent-hierarchy.md). Messages reach an agent while it
works, or wake it when idle, instead of waiting for its turn to end.

- **The wake channel** (Claude Code): each agent session starts a
  background watcher, `jeet inbox watch`, from its own session-start hook.
  It hands the agent what is queued for it through Claude Code's messaging
  socket as soon as it is queued: an idle agent wakes, a busy one gets it
  at its next tool call, one waiting on a prompt gets it once the prompt
  is answered. Such messages arrive from another session, which Claude
  Code tells the agent cannot approve anything ([Messaging an
  agent](#messaging-an-agent)).
- **Routing by sender**: only what you send yourself, with `jeet task
  send` from your own terminal, to an idle agent is still typed in as your
  turn (unless its input box holds unsent text; then it goes through the
  channel, and the CLI says so). Messages from the assistant and managers,
  reports, answers, escalations, watch events and briefs go through the
  channel. If Claude Code refuses or drops a message, jeet falls back to
  the queue and typing for that session.
- **`[harness] wake = false`** turns the channel off, from each agent's
  next start ([The coding agent's harness](#the-coding-agents-harness)).
- **A question stays open through progress**: a `question` or `blocked`
  report stays open until it is answered or the task reports done,
  blocked or another question; a `progress` report no longer closes it
  ([Reporting to the parent](#reporting-to-the-parent)).

**Upgrading to v0.11.0:** the index stays at schema v6, so there is no
migration and v0.10.0 still opens it. The one new config key is
`[harness] wake` (default true), which v0.10.0 refuses, so leave it out of
config.toml until every jeet you run is v0.11.

- Running agents keep working, with messages delivered as before. An
  agent gets its watcher hook at its next start: restart each once it is
  idle with `jeet task restart <task>` and `jeet task restart assistant`;
  both keep the conversation. An agent `jeet task ls` marks `(unanchored)`
  needs `jeet task restart`, or `jeet task adopt` from a terminal outside
  jeet's tmux sessions, as before.
- The channel was built and checked on Claude Code 2.1.296, the harness's
  tested version. It needs the session's messaging socket
  (`CLAUDE_CODE_MESSAGING_SOCKET`); a session without one, such as
  `claude --bare`, gets no watcher and messages are queued and typed as
  before. A repo's `crossSessionInbound: "refuse"` makes jeet fall back the
  same way, after about 20 seconds.
- A question a task asked and then followed with only `progress` reports
  is open again, and its parent holds it: `jeet report ls` lists the ones
  you hold.
- The assistant's and managers' `AGENTS.md` and skills, and a task's own
  skill, changed only in how they word when a question closes; jeet never
  rewrites them, and nothing needs regenerating for the wake channel. To
  get the new wording, delete the unedited files as for v0.10.

## What's new in v0.10

The agents pane, and questions answered up the tree: the third and fourth
phases of [hierarchical agents](docs/design/agent-hierarchy.md).

- **The agents tab** in the tasks list (`⇥` from *active*) shows the tree
  under the assistant, each node with the most urgent state in its subtree
  (needs you, needs input, blocked, budget, running, idle, done), its budget
  and notes, and a header counting live agents against `max_live` and what
  waits on you. `l`, `i` and `b` read a node's worklog, inbox and reports,
  and brief ([The agents tab](#the-agents-tab)).
- **Steering from the agents tab**: `p` / `P` pause and resume a node and
  everything below it, `x` stops it after a confirm, and `n` makes a task
  or manager under the assistant or a manager. These act as you, so they
  work only from your own terminal, not an agent's pane.
- **Questions go up the tree and get answered**: a `question` or `blocked`
  report stays open until it is answered or the task reports done, blocked
  or another question; `progress` does not close it. Its parent holds it
  and either answers it, `jeet report answer <id> "…"`, or passes it up,
  `jeet report escalate <id>`; `jeet report ls` lists the ones you hold. One the assistant passes up is yours, with a desktop
  notification. The task that asked gets the answer as `<jeet-answer>`,
  and each manager in between a copy ([Reporting to the
  parent](#reporting-to-the-parent)).
- **`a` in the agents tab** answers the open question at a node or nearest
  below it, those passed to you first; the needs-you counts include them.
- **`jeet watch` rules can hand events to a manager**: `action =
  "manager:<slug>"` sends a rule's GitHub events to the active manager with
  that slug, else to the assistant ([Watching GitHub](#watching-github)).
- **`jeet task delete <manager> --reparent`** deletes a manager alone and
  moves the tasks under it, with their own, to its parent, keeping what
  each may do; the parent is told and takes over the questions it held
  ([Archiving and deleting](#archiving-and-deleting)).

**Upgrading to v0.10.0:** the index stays at schema v6, so there is no
migration and v0.9.0 still opens it. There are no new config keys; the one
new config value is a rule's `action = "manager:<slug>"`, which v0.9.0
refuses, so keep such rules out of config.toml until every jeet you run is
v0.10.

- A task's last `question` or `blocked` report from before the upgrade, if
  it has not reported done, blocked or another question since, is now an
  open question its parent holds. `jeet report ls` at your terminal lists
  them; answer each, or leave it until the task reports done, blocked or
  another question.
- jeet never writes over a file in the assistant's or a manager's home, so
  they keep the `AGENTS.md` and skills they were first given, which do not
  know about `jeet report answer`, `escalate` and `ls`, `<jeet-answer>`, or
  watch events sent to a manager. If you have not edited them, delete
  `$JEET_HOME/assistant/AGENTS.md`,
  `$JEET_HOME/assistant/.claude/skills/jeet-assistant/` and
  `$JEET_HOME/assistant/.claude/skills/jeet-managers/`, and in each
  manager's home (`$JEET_HOME/tasks/<slug>/`) `AGENTS.md` and
  `.claude/skills/jeet-manager/`; the next
  start writes the current ones. A task's own skill is unchanged.
- Running agents keep working. To give them the new files, restart each
  once it is idle with `jeet task restart <task>` and `jeet task restart
  assistant`; both keep the conversation. An agent `jeet task ls` marks
  `(unanchored)` needs `jeet task restart`, or `jeet task adopt` from a
  terminal outside jeet's tmux sessions, as in v0.9.

## What's new in v0.9

Managers, the second phase of [hierarchical
agents](docs/design/agent-hierarchy.md).

- **`jeet manager new`** makes a manager, an agent that takes an area of
  work, plans it and hands it to tasks (and managers) of its own under it,
  which report to it as tasks report to the assistant. It gets a home with
  its role, a read-only `BRIEF.md`, a worklog and a todo list, and starts at
  once. `jeet task new --parent <manager>`, `--allow` and `--budget` place
  and limit a task; `jeet manager ls`, `show`, `grant` and `budget` read
  and steer the tree ([Reporting to the parent](#reporting-to-the-parent)).
- **Managers run locally in tmux, as you, unconfined**, as every task
  does. Their limits (what they may reach, what they are granted, their
  budgets, `max_live`, read-only worktrees and the git denies) are
  cooperative: they stop honest mistakes, not an agent that sets out to get
  around jeet. A confined placement may come later.
- **Budgets, pause and stop** for every task under the assistant: a budget
  of busy time and turns (no cost yet), `jeet manager pause`, `resume` and
  `stop` on a task and everything below it, and `jeet manager stop --all`,
  whose stops only you can undo.
- **`[managers] max_live` (default 8) now applies to the assistant's own
  tasks:** at most that many tasks under the assistant run at once, and a
  start past it is queued, with the parent told, until one ends. The
  assistant's ninth running task used to start; now it waits. Raise
  `max_live` to keep the old behaviour. Your own `jeet task open` starts
  one anyway, with a warning.
- **Cloning into the store needs `repo.clone`:** `jeet worktree add` and
  `jeet task new --repo` naming a repository jeet does not have clone it,
  for the assistant and a task with a parent, only when they hold
  `repo.clone`, as `jeet clone` does. Add it to `[assistant] allow` if the
  assistant should keep cloning that way. A clone URL that would land
  outside the store is refused for everyone.
- `--recursive` (`-r`) on `jeet task archive`, `unarchive` and `delete` takes
  a task and every task under it, with one confirm.
- `jeet task restart <task>` ends a task's agent and resumes its latest
  session in a new one, keeping its conversation; refused while it is busy
  unless `--force`. `jeet task restart assistant` does the same for the
  assistant's windows.
- `jeet task adopt <task>` lets you vouch, at a terminal, for an agent an
  older jeet started, so its jeet commands work without a restart.
- `jeet task ls` and the tasks list mark agents jeet has no record of as
  unanchored, with the command that fixes it.
- **Reviews**: opening a review task resumes the review's session instead of
  starting a blank one, review tasks show as `review <repo>#<n>` in the
  tasks list and `jeet task ls`, and `v` opens any task's pull request in
  the review viewer.

**Upgrading to v0.9.0:** the index stays at schema v6, so there is no
migration and v0.8.1 still opens it. `[managers]` is checked when it is
used, so a mistake there refuses work under the assistant, not jeet.

- If the assistant should keep cloning repositories it does not have through
  `jeet worktree add` or `jeet task new --repo`, add `repo.clone` to
  `[assistant] allow`.
- If the assistant may run more than 8 tasks at once, raise `[managers]
  max_live`, or starts past it are queued.
- jeet never writes over a file in the assistant's home, so the assistant
  keeps the `jeet-assistant` skill it was first given. If you have not
  edited it, delete `$JEET_HOME/assistant/.claude/skills/jeet-assistant/`
  and the next start writes the current one, which knows about reports,
  reviews and managers. The new `jeet-managers` skill is added on its own.
- Running agents keep working. To give them v0.9's agent settings and
  skills, restart each once it is idle with `jeet task restart <task>` and
  `jeet task restart assistant`; both keep the conversation. An agent `jeet task ls` marks `(unanchored)`, started by a
  jeet older than v0.8.1, needs `jeet task restart`, or `jeet task adopt`
  from a terminal outside jeet's tmux sessions, before its `jeet report`
  and `jeet worktree add` work.

## What's new in v0.8

- **Harnesses**: the coding agent now runs behind a harness interface, so
  agents other than Claude Code can be supported. `harness` picks it
  (`claude-code` by default, or `generic`). jeet detects the agent's
  folder-trust prompt, and with `auto_trust` on, answers it for you.
- **AGENTS.md**: projects and the assistant keep agent instructions in
  `AGENTS.md`, with a `CLAUDE.md` that imports it.
- **Projects form**: the new-project form picks repos, and the task and
  project pickers can add repos the store does not have yet.
- `jeet pr merge` and the assistant's `pr.merge` capability merge a PR once
  it is ready.
- `jeet task last` shows what a task's agent last said and how its turn
  ended.
- **tmux**: Shift+Enter reaches agents running in tmux, and
  `jeet task send` pastes long messages whole, never onto a draft.

v0.8.1: the first steps toward [hierarchical agents](docs/design/agent-hierarchy.md).

- **Tasks report to the assistant that made them.** `jeet report
  progress|done|blocked|question`, automatic status reports when a task
  waits on you, idles or ends, and a "Your tasks" section in the
  assistant's context. Tasks with a parent are gated to what their parent
  allows ([Reporting to the parent](#reporting-to-the-parent)).
- `jeet task interrupt` ends an agent's current turn and keeps its session.
  The assistant and tasks under it are denied force-pushes, pushes to a
  default branch and PR merges.
- jeet knows which agent is calling by the process it launched, not by
  `$JEET_TASK_ID` alone. Claude Code's settings now go in a private file
  under `$JEET_HOME/run/`.
- Deleting a task with children, or archiving one with active children, is
  refused.
- `auto_trust` waits for the trust prompt to redraw before pressing Enter.

**Upgrading to v0.8.1:** jeet migrates the index to schema v6 the first
time it runs, and an older jeet refuses it after that (copy
`$JEET_HOME/index.db` first if you may go back). Agents started by an older
jeet were never recorded, so their `jeet report` and `jeet worktree add` are
refused; `jeet task ls` marks them `(unanchored)`. Bring each onto the new
jeet without losing its conversation: `jeet task restart <task>` once it is
idle (and `jeet task restart assistant` for the assistant's windows), or,
to leave work in progress running, `jeet task adopt <task>` (and `jeet task
adopt assistant`) from a terminal outside jeet's tmux sessions, then restart
it later for the new hooks.

## What's new in v0.7

jeet is now built around **tasks**. This is a breaking release: run
`jeet import` to bring existing repos, branches and worktrees across.

- **Tasks** (`jeet`, `jeet tasks`): a task is a folder that holds any number
  of worktrees from any repo, added as needed with `jeet worktree add`. Each
  task's agent runs in tmux, and the tasks list shows its live status, size
  on disk, worktrees and PR history. Archiving a task frees its disk space
  and keeps its state; deleting it removes everything it owns.
- **Projects**: templates with repos, a CLAUDE.md and skills, applied when a
  task is created.
- **Store sync**: trunks live only in the store and stay current in the
  background. `jeet sync` does the same on demand.
- **Review viewer**: a GitHub-dark diff viewer with syntax highlighting,
  inline comments, replies, reactions and verdicts. `jeet review <pr>` now
  opens a review task.
- **Explorer commands**: type `:mv`, `:cp`, `:mkdir`, `:rm` or `:touch` into
  the filter.
- **Assistant** (`jeet chat`): a global organiser with its own sessions,
  `jeet todo`, and task steering with `jeet task send`. It acts only within
  `[assistant] allow` and notifies you of what it did. `jeet watch` routes
  GitHub events to it.
- **Removed**: `adopt`, `scan`, `checkout`, `sessions` and
  `exec --ephemeral`. `jeet worktree` now works only inside a task.

v0.7.1: notifications never hold jeet up. `terminal-notifier` waits until
macOS grants it permission, which stalled `jeet notify`, the assistant's
actions and `jeet watch`; jeet now gives it two seconds and moves on.

v0.7.2: **Ctrl-Q** goes back to jeet from a task's agent and leaves it
running (quitting Claude ends it). The tmux status line now says so.

## What's new in v0.6

- `jeet prs [repo]` lists a repository's open pull requests — size, review
  status, and which ones want your review, are checked out, or have been
  reviewed in the background.
- `ctrl-o` in the explorer shows the same list; `⏎` checks a PR out and
  switches to it (without starting the review command).
- `jeet review --repo` checks a PR out from outside its repository.

## What's new in v0.5

- `jeet review <pr>` checks a pull request out into its own review task, opens
  the review viewer there, and can start a review command (Claude Code, say) in the
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
scan_roots = ["~/Projects", "~/code"]   # where `jeet import` looks by default
```

Two optional keys are not written by default; add them yourself to override:

```toml
editor = "vim"            # opened by ⏎ in the explorer; defaults to $VISUAL/$EDITOR
harness = "claude-code"   # the coding agent; see below
```

`editor` accepts arguments (`editor = "code --wait"`) and is overridden by
`JEET_EDITOR`.

### The coding agent's harness

jeet runs your coding agent through a *harness*: what it knows about that
agent, from how to start and resume its sessions to what its hooks report.
Claude Code (`claude-code`) is the default, and for now the only one;
`generic` runs any other command with none of that (no status, no resume, no
first prompt, no assistant, no deny rules, no interrupt, no wake channel).
Without deny rules, tasks with a parent are not stopped from force-pushing,
pushing to a default branch or merging (see the deny rules under the
assistant's permissions below, and
[docs/harnesses.md](docs/harnesses.md#deny-rules)), and `jeet task
interrupt` refuses.

```toml
harness = "claude-code"
```

or, to run something other than the harness's own binary, or to have jeet
answer the folder-trust prompt:

```toml
[harness]
name = "claude-code"
command = "/opt/bin/claude --model opus"   # program and arguments (default: claude)
auto_trust = true                          # default: false
auto_trust_reviews = false                 # default: false
wake = true                                # default: true
wake_repair = "auto"                       # default: "auto"; "off" for no repairs
```

The command is taken from `$JEET_AGENT` first, then `[harness] command`, then
the older `agent = "…"` key, which keeps working; without a `name`, the
harness is told by the program (`claude` is Claude Code, anything else
`generic`).

jeet supports each harness's latest version only. The first time a run of
jeet starts an agent, it checks `<command> --version` against the version
jeet was last tested with (Claude Code 2.1.296) and warns, without
refusing, when yours is older. `JEET_HARNESS_VERSION_CHECK=0` skips the
check.

**`auto_trust`** has jeet accept Claude Code's "Is this a project you created
or one you trust?" for task directories under `~/.jeet/tasks`, so a new task
starts straight into its brief. It selects "Yes, I trust this folder", checks
that it shows selected, and only then presses Enter (Enter on the default
"No, exit" would quit). Trusting a folder lets Claude Code act on what is in
it: its `.claude/settings.json` hooks and `.mcp.json` servers run. In your own
tasks that is your own code; in a **review** it is someone else's PR
branch, so reviews are left to you unless `auto_trust_reviews` is on as
well. Some per-folder settings, such as `.claude/settings.local.json` and
`.mcp.json`, reportedly need the exact folder trusted, not just a parent.
The assistant's session is never answered for.

**`wake`** gives each agent its wake channel ([Messaging an
agent](#messaging-an-agent)): a background `jeet inbox watch` started by
its session-start hook. `wake = false` installs none, from each agent's
next start (`jeet task restart` one that is running), and messages are
queued and typed in as without one.

To add a harness, see [docs/harnesses.md](docs/harnesses.md).

### Quiet messages for the assistant and managers

```toml
[messages]
render = { assistant = "quiet", managers = "quiet" }   # each "quiet" or "full"; default "quiet"
digest_mins = 15                                      # default: 15
```

What is queued for the assistant, and for every manager, is posted by class, from the kind of each
item, never from who sent it. A one-line header comes first
(`jeet: 1 question, 2 updates (jeet show <ref> for any)`), then:

- **full**: `question` and `blocked` reports, answers, and anything jeet has
  no kind for (your own words) in their whole fenced text;
- **line**: a `done` report, an agent's `jeet task send` to the assistant and
  a `jeet watch` batch, one line each with the start of its text and a reference;
- **digest**: `progress` and `status` reports, and a note that jeet resumed a
  session to repair its wake channel, one line per task. A queue of nothing
  else is held for `digest_mins`; a full or line item sends it with it.

The nonce stays last, and a Stop's 2 KB cap applies to the rendered post.
Work tasks always get the full text, as does the assistant with
`assistant = "full"` and every manager with `managers = "full"` (one key for
all managers). What a manager's parent or you send it stays whole. Typing into
an idle pane stays full text.

`jeet show r123` prints report 123 and `jeet show m45` a message, whole;
`m` messages are kept 30 days in the task's `.jeet/delivered.jsonl`. It only
reads, and shows you what you reach: everything for you and the assistant, and
for a task with a parent only the reports and messages of itself and the tasks
below it. A line in a task's inbox that cannot be read is moved to
`.jeet/inbox.rejected.jsonl` rather than dropped, and the task's parent gets
a `status` report about it (the assistant's log says so).

A `[review]` section sets the review agent's first prompt and its limits —
see [The review agent](#the-review-agent) and [Review limits](#review-limits).

### Notifications

`jeet notify "<title>" [--body B] [--url U]` raises a desktop notification:
through `terminal-notifier` on macOS when it is installed (a click opens
`--url`), else Notification Center via `osascript`, and `notify-send`
elsewhere. Review jobs use it to say they are done. `JEET_NOTIFY_CMD` runs a
command of your own instead, with the notification in `JEET_NOTIFY_TITLE`,
`JEET_NOTIFY_SUBTITLE`, `JEET_NOTIFY_BODY` and `JEET_NOTIFY_URL`.

### The assistant's permissions

jeet commands run by the assistant's agent may only do what `[assistant]
allow` lists; anything else fails with a message saying
what is missing. Reads (`jeet ls`, `jeet task ls`, `jeet task last`, `jeet prs`, …),
`jeet notify` and `jeet todo` are always allowed; screens and shells (`jeet`,
`jeet explore`, `jeet exec`, `jeet chat`) need a person at a terminal.

```toml
[assistant]
allow = ["task.create", "task.send", "task.archive", "review.start", "worktree.add", "project.read"]
notify = true   # a notification for each action it takes
```

The capabilities: `task.create`, `task.open`, `task.send`, `task.archive`,
`task.unarchive`, `task.delete`, `task.resync`, `worktree.add`, `worktree.rm`,
`review.start`, `project.read`, `project.create`, `project.delete`,
`repo.clone`, `repo.import`, `repo.sync`, `shell.install`, `watch.install`,
`pr.comment`, `pr.review`, `pr.merge` (`jeet pr merge`), `manager.grant`
(`jeet manager grant` and `budget`), `manager.create` (`jeet manager new`)
and `manager.stop` (`jeet manager pause`, `resume` and `stop`). Nothing is
allowed until listed. The assistant's Claude settings
pre-approve exactly the matching `jeet` commands, plus the `gh` commands of
`pr.comment` (`gh pr comment`, `gh issue comment`) and `pr.review` (`gh pr
review`), so allowed actions do not prompt. Every action it takes, and every one refused, is appended to
`~/.jeet/assistant/actions.log`.

A task with a parent (one the assistant made, or one made with `--parent
assistant`; `--parent` also takes a manager by its exact name) is gated
more narrowly. It is granted `worktree.add`
when made (`[managers] worker_default`, or what `--allow` names), and may
use what its grant names only while the assistant may too: add
`worktree.add` to `[assistant] allow` to let such tasks add worktrees.
Granting a held capability (`pr.merge`, `pr.comment`, `pr.review`,
`task.delete` and `[managers] held`) is written to `actions.log` and
announced, whatever `[assistant] notify` says, unless you grant it
yourself. It reaches only itself: it may run `jeet report`, add
worktrees to itself and read itself (`jeet task last`, `task path`, `task
prs`, `worktree ls`/`path`), and `jeet task ls` lists only it. The reads
that name no task (`jeet ls`, `jeet prs`, …) are free. Anything else is
refused, whatever the assistant holds, `project.read` (`jeet projects ls`)
included, as are `jeet notify`, `jeet todo` and review jobs; the refusal
tells it to ask with `jeet report`.

A manager (`jeet manager new`) is gated the same way, by its own grant
(`--allow`, or `[managers] manager_default`) cut by every list above it,
but it reaches its subtree rather than only itself: it makes tasks and
managers under itself (`--parent` names itself or a manager below it), and
the `jeet task` and `jeet manager` commands its grant allows act on tasks
below it, never on its parent, a sibling or another subtree. `jeet manager
stop --all` is yours alone. Its settings pre-approve the `jeet` commands of
what it holds, read at each start, since nobody sits at its pane. Its
worktrees are read-only, and on a harness with deny rules it is also denied
`git commit`, `git push`, `git checkout -b` and `git switch -c`: it hands
code changes to a worker. When the assistant reads what a task
said with `jeet task last`, it gets it fenced as untrusted, as it gets
reports, and the status beside it (which the task's agent can write) unable
to close or open a fence. The assistant's own
reach is unchanged: all tasks, yours included. Reviews it starts with
`jeet review` report to it too, though they are not under it: their review
agent has the fixed gate every review agent has ([The review
agent](#the-review-agent)), plus `jeet report` to the assistant. A legacy
`[review] command` runs as the review too, and jeet tells the assistant when
it exits without reporting done.
No harness launches that command, so it gets none of
the settings jeet gives Claude Code: if it runs Claude Code with its sandbox
on, exclude `jeet` from the sandbox yourself (`--settings
'{"sandbox":{"excludedCommands":["jeet"]}}'`), or its `jeet report` and
`jeet worktree add` are refused as unconfirmed.

jeet knows which agent is calling by the process it launched: each task's
agent, each assistant window and each review job is recorded when jeet
starts it, and a jeet command is that agent's when it runs below that
process. `$JEET_TASK_ID` and `JEET_ASSISTANT=1` are only hints: one that
disagrees is refused and written to `actions.log`, and one that claims a task
with a parent, or the assistant, with no recorded agent behind it is refused
as unconfirmed. So a shell you open inside such a task's or the assistant's
tmux session is refused for commands that need a capability or name a
task (`jeet task last`, `jeet worktree ls` and the like); use one outside
it. This stops mistakes and casual forging, not an agent that sets
out to escape. Every agent, managers below the assistant included, runs
locally in tmux as you, cooperative and unconfined: nothing sandboxes it.

**Upgrading to this version:** agents started by an older jeet were never
recorded, so their `jeet report` and `jeet worktree add` are refused, and
their hooks still write status but no longer report to the assistant.
`jeet task ls` marks them `(unanchored)`. Restart each once it is idle with
`jeet task restart <task>` (`assistant` for the assistant's windows), which
resumes its conversation; or adopt it with `jeet task adopt <task>` from a
terminal outside jeet's tmux sessions to keep work in progress running, and
restart it later for the new hooks ([Restarting or adopting an
agent](#restarting-or-adopting-an-agent)). `jeet task open` on such a task
warns instead of adopting it. After
upgrading, an older jeet refuses the index.

On a harness with deny rules (Claude Code), the assistant and every task
with a parent are also denied, even with your approval at the prompt:
force-pushes (`git push --force…`, `-f`, `+branch`, `--mirror`), `git push
--all` (and its synonym `--branches`), pushes naming a default branch
(`main`, `master` and those of your indexed repositories, the eight in all
most of them use; a repo's is `origin/HEAD` when jeet indexed it, or else
the branch then checked out, so that branch name is denied everywhere), and merging
other than through `jeet pr merge` (`gh pr merge`, and the REST and GraphQL
merge endpoints through `gh api` and `curl`, when the command names them). A task with a parent is also denied `gh pr
comment`, `gh issue comment` and `gh pr review` unless it holds
`pr.comment`/`pr.review` (only those commands: the same through `gh api`
still reaches your prompt); the assistant holding them runs them without a
prompt. Top-level tasks get no deny rules. The rules match the start of a
command only (`git -C . push -f` or `sh -c '…'` get past them), and a push
whose destination is implied (`git push origin HEAD` while on `main`), a
commit through the API (the contents endpoint, `createCommitOnBranch`) or a
merge mutation read from a file gets past them too: they catch
mistakes, and branch protection on GitHub is the real control. A task with
a parent on a harness without deny rules starts with a warning, as does the
legacy `[review] command` of a review the assistant starts: no harness
launches it, so it gets no deny rules.

```toml
[managers]
idle_report_mins = 10   # tell the assistant when a task of its sits idle this long
max_depth = 3           # a task at most 3 below the assistant, a manager 2
max_children = 6        # active tasks under one manager (the assistant has no limit)
default_budget = "4h"   # for a manager's task when `--budget` names none
max_live = 8            # tasks under the assistant running at once, reviews apart; a start past it waits
worker_default = ["worktree.add"]   # a new task's grant when `--allow` names none, less what `held` names
manager_default = []    # a new manager's
held = []               # added to pr.merge, pr.comment, pr.review, task.delete
```

The tree's settings are checked when they are used, not when config.toml
is read: a mistake in them (an unknown capability, a held one in a default
you set, a limit of 0, a `$` budget) refuses making tasks under the
assistant, starting one, `jeet manager grant` and unarchiving a task under
a manager, and nothing else.

### Keeping the store fetched

Commands that go to the network anyway (`jeet clone`, `jeet prs`,
`jeet review`), and the tasks list and the explorer on launch, start a background fetch of every
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
  watch.json, watch.log              # `jeet watch`: what it has seen, what it said
  watch-events.jsonl                 # recent GitHub events and where each went
  store/
    github.com/acme/widget/          # trunk (default branch)
  tasks/
    fix-login/                       # a task: the agent's working directory
      .jeet/task-id                  # marks it, so jeet finds the task from below
      .jeet/inbox.jsonl              # messages queued for its busy agent
      .jeet/wait/                    # its wake channel's lock and the batch in flight
      .claude/skills/jeet/SKILL.md   # how an agent adds worktrees
      widget/  widget-2/             # its worktrees, added on demand
    backend/                         # a manager: AGENTS.md, notes/, todo.md,
      BRIEF.md                       #   its brief, read-only, written by jeet
      .claude/skills/jeet-manager/   #   the jeet commands it uses
  projects/<name>/                   # templates: AGENTS.md and skills/ copied into new tasks
    project.toml                     # its repositories and agent arguments
    AGENTS.md                        # instructions for the agent
    CLAUDE.md                        # `@AGENTS.md`: the same, for Claude Code
    skills/                          # copied to the task's .claude/skills/
  assistant/                         # the assistant's home (`jeet chat`)
    AGENTS.md                        # its role; yours to edit
    CLAUDE.md                        # `@AGENTS.md`
    .claude/skills/jeet-assistant/   # the jeet commands it uses
    .claude/skills/jeet-managers/    # how it hands an area of work to a manager
    notes/worklog.md                 # its running memory
    todo.md                          # the todo list (`jeet todo`)
    actions.log                      # what it did, one JSON object per line
    .jeet/                           # its status files and inbox, as a task's
```

Before tasks, worktrees lived under `worktrees/` and `ephemeral/`; `jeet import`
moves anything left there into tasks.

Repo ids look like `github.com/acme/widget`. Filters accept full ids or unique suffixes such as `acme/widget`.

## Packaging

Release tags build prebuilt binaries, `.deb` packages, and update the Homebrew tap automatically.

## Development

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

[docs/harnesses.md](docs/harnesses.md) says how to add support for another
coding agent.

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
