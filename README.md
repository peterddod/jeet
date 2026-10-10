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
cargo install --git https://github.com/peterddod/jeet --tag v0.7.0
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
jeet task archive fix-login              # free the disk, keep the work
jeet task unarchive fix-login
jeet task delete fix-login [--force]
jeet task prs fix-login                  # the PRs that came out of the task

# The assistant — one organiser for everything, in tmux
jeet chat                                # resume its latest session (set up on first run)
jeet chat --new "plan the week"          # a fresh session beside the others
jeet chat --pick                         # choose, rename, end or mark one primary
jeet todo add "ship the fix" -p 1 --task fix-login
jeet todo                                # its todo list; done, rm, --json
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
worktree has the PR's branch already, or else the PR's review task, which it
makes just as `jeet review` would. It is a way in, not `jeet review`: no
review command is started.

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

From anywhere inside a repository, `jeet review` finds or makes the PR's
**review task**, `review-<repo>-<number>`, checks the PR out in it (or, when
the task already has it, brings that checkout up to date if it is clean, and
brings an archived task back), opens it in the review viewer, and `cd`s you
into the worktree when you leave. The review task is a task like any other: it
shows in the tasks list, where `v` opens the viewer again, and archiving or
deleting it frees the disk. A PR from a fork is checked out as `pr/<number>`,
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
`Esc` goes back. `d` in the explorer's `ctrl-p` panel opens it too.

On a line of code, `c` writes an inline comment, which joins your pending
review (starting one if need be) until a verdict submits it. On a comment, `R`
replies to its thread, `+` picks a reaction, and — on your own comments only —
`e` edits it and `d` deletes it, after asking. The viewer checks the PR every
30 seconds; when someone else has pushed or commented, the header shows
`⟳ updates – r`, and `r` fetches it all again with the cursor kept on the line
it was on.

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
opens its output. A Claude Code review also leaves a session behind, which
`claude --resume` in the review task's worktree picks up. Running `jeet review` again for
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

### The tasks list

```bash
jeet          # or `jeet tasks`
```

```text
┌ jeet · tasks ─────────────────────────────────────────────────────────────┐
│ active 3   archived 1    / filter  ⇥ switch                               │
└───────────────────────────────────────────────────────────────────────────┘
 ✦ assistant  2 sessions  ● *plan the week · inbox 1 · todo 4 open
┌ 3 active tasks ───────────────────────────────────────────────────────────┐
│● running – Edit        Fix login          2 wt: api, web    1.2G  just now│
│◆ needs input – Bash    api-v2             1 wt: api         640M    5m ago│
│○ idle 1h               notes              empty              12K    1h ago│
└───────────────────────────────────────────────────────────────────────────┘
```

Every task, most recently active first: what its agent is doing, its name, the
worktrees it holds (as of the last look at the disk, since the agent adds them
as it goes), its size and when anything last happened in it. Statuses refresh
every two seconds and sizes are measured in the background, so the list never
waits on either. `⇥` switches to the archived tasks and back; `/` filters by
name the way the explorer does.

The [assistant](#the-assistant) is pinned above the list: how many sessions it
has, what each running one is doing (`*` marks the primary one), how many
messages and events are queued for it, and how many todo items are open. It is
read from its files on a thread of its own, like the statuses.

| Key | |
|---|---|
| `⏎` | attach to the task's agent in tmux, starting it if need be — with a choice of session when there is more than one to resume. `C-b d` comes back here |
| `e` | the explorer, on the task's only worktree, or a choice of its worktrees and the task directory itself; `esc` comes back |
| `w` | the task's worktrees, with branch, ahead/behind and diff: `n` adds one, `D` removes one, `⏎` explores it |
| `n` | a new task: a name, and optionally a project, repositories and a branch. Left blank, an empty canvas |
| `a` | archive the task (asks first if its agent is running) — or, in the archived view, bring it back |
| `D` | delete the task, after listing any work that would be lost |
| `P` | projects: `n` new, `e` edit `project.toml`, `D` delete, `⏎` start a task from one |
| `S` | copy the task's project files in again |
| `p` | the task's pull requests: state, size and title (a link); `⏎` or `o` opens one in the browser |
| `v` | on a review task, its pull request in the review viewer |
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

### Pull request history

A task remembers the pull requests that came out of it. When the tasks list
opens (and on `r`, and on `p` for the one task), each worktree is looked up on
GitHub in the background through `gh`: PRs from its branch, open or not; for a
detached checkout with commits of its own, the PRs that hold its HEAD; for a
review task, the PR it reviews. What comes back is kept, so the history
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

### The agent

```bash
jeet task open fix-login                 # attach to the task's agent, starting it if needed
jeet task open fix-login --session <id>  # resume that session instead of the latest
```

Each task's coding agent runs in its own tmux session, `jeet-<slug>`, in the
task directory, so it keeps working when you detach (`C-b d`) or close the
terminal. tmux is required (`brew install tmux`, `apt install tmux`); without
it `jeet task open` says so. Opening a task attaches to its session, starting
the agent first if it is not running: Claude Code resumes the task's latest
session, or starts a new one. Without a terminal, `open` leaves the agent
running detached and prints the task directory.

jeet starts Claude Code with hooks (passed with `--settings`; nothing is
written into the task or a repository) that record what the agent is doing in
`.jeet/status/<session>.json`, and with permission to run `jeet worktree` and
`jeet ls` without asking. `jeet task ls` shows it: `running – <tool>`,
`needs input – <question>`, `idle 5m`, or `not running` once the tmux session
is gone. Tasks are listed most recently active first, counting the agent's
activity. Another agent (`agent = "aider"`) runs in tmux the same way, with no
status. Archiving or deleting a task ends its tmux session.

### Messaging an agent

```bash
jeet task send fix-login "also add tests"
jeet task new fix-x --repo api --prompt "Fix X; add a test for it"
```

`jeet task send` gets a message to the task's agent however it is doing. An
idle agent has it typed in (`tmux send-keys -l`) and submitted. A busy one, or
one waiting on you, has it queued in `.jeet/inbox.jsonl`; when the agent
finishes, its Stop hook hands the queue over and it carries straight on. A
turn you interrupt with Esc has no Stop, so the queue is also typed in once
Claude reports it has sat idle at its prompt (after about a minute). A message
is never typed into a permission prompt, or into the dialogs Claude shows
before its first hook runs, which it would answer. An agent that is not
running is started with the message as its first prompt. `jeet task new
--prompt` starts the new task's agent on a brief the same way.

A message holds at most 8,000 bytes, which keeps what is typed in under
tmux's limit on a command; queued messages that do not fit with it wait for
the next hand-over. Text starting with `!`, `/` or `#`, which Claude would run
as a shell command, a slash command or a memory, is sent as `Message: …`.

### Archiving and deleting

```bash
jeet task archive fix-login          # -f: without asking about ignored files
jeet task unarchive fix-login
jeet task delete fix-login           # -f: without asking about what is lost
```

**Archive** frees the disk and keeps the work: each worktree's uncommitted
changes — staged, unstaged and untracked — are saved under
`refs/jeet/stash/<task-id>/<dir>`, its HEAD is pinned under
`refs/jeet/head/<task-id>/<dir>` so not even a commit on a detached HEAD is lost
to gc, and only then is the worktree removed. Files git ignores (build output,
`.env`, local config) are not kept: removing the worktree deletes them. So
archive names them first and asks — the tasks list in its confirm, the CLI on
the terminal — and without a terminal it needs `--force`. The task directory itself
stays, so an agent's sessions still find it. **Unarchive** puts every worktree
back on its branch (or detached at its commit) with its changes reapplied, then
drops the refs.

**Delete** removes everything the task owns: its worktrees, the branches it
created (never ones that existed before), its `refs/jeet` refs and its
directory, with everything else on its canvas. It first lists what would be
lost — uncommitted changes, commits that would be left on no branch or remote,
and any git checkout on the canvas that is none of the task's worktrees (a
clone made there, or a worktree of a repository outside the store) — and asks;
without a terminal it needs `--force`.

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
`~/.jeet/assistant`, set up by the first `jeet chat`: a `CLAUDE.md` on its
role, a `jeet-assistant` skill with the commands it uses, `notes/worklog.md`
and `todo.md`. They are written only when missing, so your edits and its own
survive.

One home, any number of sessions. Continuity lives in those files rather than
in any one conversation, so a fresh session or a `/clear` loses nothing that
matters, and two can run at once — one planning your week, one triaging
reviews. Each runs in a window of its own in the tmux session
`jeet-assistant`, with the same hooks and status as a task's agent, and with
`JEET_ASSISTANT=1`. Every session starts with a summary of the active tasks,
their open pull requests and your own that `jeet watch` follows, with their
checks, the GitHub events the watcher saw in the last week, such as review
requests (all fenced as data written by others), the todo list and any
messages queued for it.

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
is there, so turning the watcher on is not a flood.

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
on = ["mention"]
action = "notify"      # just a notification
```

`on` and `repos` may be left out to match anything. Events for the assistant
go to its sessions by the rules above (primary, else most recent, else a new
`events <date>` session), several to a message; an event for a task goes to
its agent the way `jeet task send` would send it. Either way the event is
fenced as `<github-event untrusted="true">`, since titles and comments are
written by other people: data to weigh, never instructions. One that cannot be
delivered is notified instead. Each event, and where it went, is appended to
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
editor = "vim"       # opened by ⏎ in the explorer; defaults to $VISUAL/$EDITOR
agent = "claude"     # launched by `ctrl-a`; may include arguments
```

Both accept arguments (`editor = "code --wait"`) and are overridden by
`JEET_EDITOR` / `JEET_AGENT`.

A `[review]` section sets what `jeet review` runs in the background — see
[Reviewing](#reviewing).

### Notifications

`jeet notify "<title>" [--body B] [--url U]` raises a desktop notification:
through `terminal-notifier` on macOS when it is installed (a click opens
`--url`), else Notification Center via `osascript`, and `notify-send`
elsewhere. Review jobs use it to say they are done. `JEET_NOTIFY_CMD` runs a
command of your own instead, with the notification in `JEET_NOTIFY_TITLE`,
`JEET_NOTIFY_SUBTITLE`, `JEET_NOTIFY_BODY` and `JEET_NOTIFY_URL`.

### The assistant's permissions

jeet commands run with `JEET_ASSISTANT=1` (the assistant's session) may only
do what `[assistant] allow` lists; anything else fails with a message saying
what is missing. Reads (`jeet ls`, `jeet task ls`, `jeet prs`, …),
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
`pr.comment` and `pr.review`. Nothing is allowed until listed. The assistant's Claude settings
pre-approve exactly the matching `jeet` commands, so allowed actions do not
prompt. Every action it takes, and every one refused, is appended to
`~/.jeet/assistant/actions.log`.

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
      .claude/skills/jeet/SKILL.md   # how an agent adds worktrees
      widget/  widget-2/             # its worktrees, added on demand
  projects/<name>/                   # templates: CLAUDE.md and skills/ copied into new tasks
  assistant/                         # the assistant's home (`jeet chat`)
    CLAUDE.md                        # its role; yours to edit
    .claude/skills/jeet-assistant/   # the jeet commands it uses
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
