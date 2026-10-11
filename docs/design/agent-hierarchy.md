# Hierarchical agents

Status: **draft, for review; all open questions decided**. Nothing here is implemented.
Designed against `main` at v0.7.2, plus the open PRs #21 (projects form), #22
(AGENTS.md) and #23 (`Harness` trait, neutral hook events, trust watcher,
`starting` / `needs input` states).

## Summary

Today the assistant manages a flat list of tasks. This RFC lets it hand a
whole area of work to a **manager** (an "agent" in the UI): a node like the
assistant, with its own home, worklog, todo list, inbox and allow-list, that
in turn creates and steers its own tasks and managers and reports back to
whoever made it. The
assistant becomes the root of a tree. An **agents pane** shows the tree with
status rolled up, so one glance says where the user is needed.

The key rules:

- **One concept.** A manager is a task whose role is *manage*, not a new kind
  of thing. Everything tasks already have (tmux session, status hooks,
  inbox, `task send`, archive) works for managers unchanged.
- **Authority flows down, information flows up.** A child's allow-list is
  always a subset of its parent's, checked by jeet at every call. Reports go
  up as fenced, untrusted messages. Only jeet, on the root's behalf, talks to
  the user.
- **A manager acts only on its own subtree.**
- **Small first step.** Phase 1 adds a parent link and reporting up, with the
  assistant as the only manager.

## 1. Model

### What exists

`tasks` already has a `kind`: `work`, `review` and `assistant` (one row,
enforced by `tasks_one_assistant`). The assistant is a task row with a special
home (`~/.jeet/assistant`), several sessions, manager files (`AGENTS.md` role,
`notes/worklog.md`, `todo.md`, `actions.log`), a fenced inbox, and a gate in
`permissions.rs` that is switched on by `JEET_ASSISTANT=1`.

### Decision: one concept, two roles

A node in the tree is a **task row**. Its `kind` gives its role:

| kind | role | holds worktrees | has children | manager files |
|---|---|---|---|---|
| `work`, `review` | **worker** (as today) | yes | no | no |
| `manager` (new) | **manager** | read-only only | yes | yes |
| `assistant` | **root manager** | read-only only | yes | yes |

**Naming (decided):** the code and the data model say *manager* (`kind =
'manager'`, `src/manager/`, `jeet manager …`, `[managers]`, `manager.*`
capabilities), because "agent" already means a task's coding agent
(`agent.rs`, `AgentSpec`, `agent = "claude"`). UI text may say *agent* where
that is unambiguous: the **agents pane**, "needs you" rows.

A manager is "the assistant, but made by a parent, with a brief, a budget and a
narrower allow-list". The code that is assistant-specific today (`ensure_files`,
`session_context`, `fence_message`, multi-session windows, `actions.log`)
becomes manager code, and the assistant is the one manager with no parent and
the user's `[assistant] allow`.

Why one concept rather than two:

- Status, tmux, `messages::deliver`, the inbox, archive, PR history and the
  tasks list are all keyed on task rows. A second table would duplicate or
  abstract all of it.
- #23's trust watcher already scopes by "a task directory under
  `~/.jeet/tasks`", which manager homes satisfy (but see 5.7).
- The assistant is already a task row; this generalises what is there.

**Managers hold read-only worktrees only (decided).** A manager may check a
repository out to read it while planning, but never to change it. Workers do
the changing, so code is changed in exactly one place and a manager's context
stays small. For a manager, `jeet worktree add`:
- always detaches at the default branch, and refuses `--branch`;
- records the worktree as read-only, so the harness gets deny rules for
  writes under it (#32) and jeet refuses to commit or push from it
  (`git.push` never applies there).

Before #32 lands, read-only is cooperative, like the rest of phase 1
(5.7).

### Layout

A manager's home is a task directory, so every task path keeps working:

```
~/.jeet/tasks/backend/            # kind = 'manager'
  .jeet/task-id
  .jeet/inbox.jsonl               # queued messages and reports, as today
  .jeet/status/<session>.json     # hooks, as today
  AGENTS.md  CLAUDE.md            # role (manager template) + #22's shim
  BRIEF.md                        # written by jeet from the parent's brief; read-only
  notes/worklog.md
  todo.md                         # `jeet todo` works on the caller's own list
  actions.log
  .claude/skills/jeet-manager/SKILL.md
  api/                            # optional read-only worktree, detached
```

## 2. Tree

### Schema (v4)

`MIGRATIONS` gains one step. SQLite cannot change a `CHECK` in place, so
`tasks` is rebuilt, as v3 did.

```sql
-- v4: hierarchy. A task may have a parent; 'manager' is a new kind.
CREATE TABLE tasks_v4 (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    project TEXT,
    kind TEXT NOT NULL DEFAULT 'work'
        CHECK (kind IN ('work', 'review', 'assistant', 'manager')),
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'archived')),
    parent_id TEXT REFERENCES tasks(id) ON DELETE RESTRICT,  -- NULL: top level
    run_state TEXT NOT NULL DEFAULT 'run'
        CHECK (run_state IN ('run', 'paused', 'stopped')),
    created_at INTEGER NOT NULL,
    last_activity INTEGER NOT NULL
);
INSERT INTO tasks_v4 (id, slug, name, project, kind, state, created_at, last_activity)
    SELECT id, slug, name, project, kind, state, created_at, last_activity FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_v4 RENAME TO tasks;
CREATE UNIQUE INDEX tasks_one_assistant ON tasks(kind) WHERE kind = 'assistant';
CREATE INDEX tasks_parent ON tasks(parent_id);

-- What the parent asked for. One row per delegated task or manager.
CREATE TABLE task_briefs (
    task_id    TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    goal       TEXT NOT NULL,
    done_when  TEXT,            -- definition of done
    brief      TEXT,            -- longer free text; also written to BRIEF.md
    created_by TEXT NOT NULL    -- the parent's task id, or 'user'
);

-- A node's own grant. Its effective allow-list is this ∩ its parent's
-- effective list, computed at check time (see 5.1). Absent for the root,
-- whose grant is `[assistant] allow`.
CREATE TABLE task_grants (
    task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    PRIMARY KEY (task_id, capability)
);

-- Limits and what has been used of them. NULL: no limit of its own
-- (the parent's still applies). No cost columns until a harness can report
-- usage (#26); they are added by a later migration then.
CREATE TABLE task_budgets (
    task_id    TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    max_secs   INTEGER,   -- wall clock while running
    max_turns  INTEGER,   -- counted from Stopped events
    used_secs  INTEGER NOT NULL DEFAULT 0,
    used_turns INTEGER NOT NULL DEFAULT 0
);

-- Everything a child said to its parent, and questions on their way up.
-- worktree read-only-ness lives on task_worktrees:
--   ALTER TABLE task_worktrees ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0;
CREATE TABLE task_reports (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, -- who it is about
    to_id       TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, -- who holds it now
    kind        TEXT NOT NULL CHECK (kind IN
                  ('progress', 'done', 'blocked', 'question', 'status')),
    body        TEXT NOT NULL,
    at          INTEGER NOT NULL,
    needs_user  INTEGER NOT NULL DEFAULT 0,  -- escalated to the user
    answer      TEXT,
    answered_by TEXT                         -- a task id, or 'user'
);
CREATE INDEX task_reports_to ON task_reports(to_id, answer);
```

`ON DELETE RESTRICT` on `parent_id` means the database refuses to orphan a
child. jeet removes subtrees leaves first.

The inbox stays the delivery mechanism, and `task_reports` is the record: the
pane, a manager's session context and `jeet report ls` read it, and they need
it to survive a hand-over that empties the inbox.

**Phase 1 (as built, D0).** v4 is additive only: `ALTER TABLE tasks ADD
COLUMN parent_id … REFERENCES tasks(id) ON DELETE RESTRICT`, the
`tasks_parent` index, `task_briefs`, and `task_reports` as above (with the
phase-4 columns `needs_user`/`answer`/`answered_by`, unread for now). The
`manager` kind, `run_state`, `task_grants`, `task_budgets` and `read_only`
move to v5, whose rebuild of `tasks` must copy `parent_id` and re-create
`tasks_one_assistant` and `tasks_parent`.

### Creation and teardown

| Operation on a manager | Children |
|---|---|
| create | none; it makes its own |
| **pause** | the whole subtree pauses (5.4) |
| **stop** | every coding agent in the subtree is ended; rows and work kept |
| **archive** | refused while it has active children, unless `--recursive`. With it, jeet lists everything the subtree would lose (the existing per-task archive check, for each worker), asks once, then archives leaves first |
| **unarchive** | only itself; `--recursive` brings the subtree back. A child cannot be unarchived under an archived parent |
| **delete** | refused while it has children (active or archived), unless `--recursive` (same listing, leaves first) or `--reparent` (children move to the grandparent, keeping grants ∩ the new parent's) |

A manager can archive or delete only its own descendants. Archiving a worker
is unchanged.

## 3. Delegation

### Commands

```bash
# The assistant makes a manager for an area of work
jeet manager new backend \
  --goal "Ship the v2 auth API" \
  --done "PRs open for api#120 and web#88, CI green, summary in worklog" \
  --brief-file /tmp/backend-brief.md \
  --allow task.create,task.send,task.archive,worktree.add \
  --budget 6h,400turns \
  --max-children 4

# The user can do the same at a terminal; the manager is parented to the assistant
jeet manager new frontend --goal "Migrate web to the new design system" --budget 4h

# The manager delegates a worker under itself (the parent defaults to the caller)
jeet task new fix-token-refresh --repo api \
  --prompt "Fix token refresh races; add a test" \
  --done "PR open against api main with a passing test" \
  --budget 2h

# Or places it lower in its own subtree
jeet task new api-docs --parent backend/docs --prompt "…"

# Read the tree
jeet manager ls                  # the caller's subtree (the whole tree for the user)
jeet manager tree --json
jeet manager show backend         # brief, grant, effective allow, budget, children, open reports
```

- `--parent` defaults to the caller: the node whose `JEET_TASK_ID` jeet sees.
  A parent outside the caller's subtree is refused.
- **The user** may make a manager directly: `jeet manager new` at a terminal,
  outside any agent, is parented to the assistant (or to any `--parent` the
  user names). Its grant is still capped by the assistant's effective list,
  so the tree has one root and one place to narrow everything. `jeet task new`
  at a terminal stays top level, as today, unless the user passes `--parent`.
- `--allow` defaults to **none** beyond the free commands. A capability the
  parent's effective list lacks is refused with the missing name, not dropped
  silently.
- `--budget` takes `<n>h`, `<n>m` and `<n>turns`, defaulting to
  `[managers] default_budget` (4h). A child's budget is capped at what remains
  of its parent's. There are no cost budgets until #26 (5.3).
- `jeet task new --prompt` keeps working as today. `--goal`/`--done` add a
  brief row for any parented task.

### What the child sees

At every session start (the `SessionStarted` → additional-context path that
the assistant uses today, via #23), the child gets a block jeet writes:

```
<jeet-brief from="manager backend" path="assistant/backend">
Goal: Fix token refresh races; add a test
Done when: PR open against api main with a passing test
Budget: 2h wall clock (0h40 used), no turn limit
You may: worktree.add, report
Report with `jeet report done|progress|blocked|question "…"`.
</jeet-brief>
```

`BRIEF.md` holds the same, so a harness without context injection, or the
agent after `/clear`, can reread it.

The brief is the child's instructions, and it is not fenced as untrusted: a
child must act on it. Its **authority** comes from the allow-list, not from
the text. A brief written by a parent that read something hostile can at most
ask for what the parent itself could do.

**Phase 1 (as built).** `--parent` accepts only `assistant` (D12); the
assistant's tasks get it without asking, the user's only when they pass it.
`--goal`/`--done` need a resulting parent, and `--done` needs `--goal`. The
brief is not a `BRIEF.md` (that is a manager file, phase 2): at each
`SessionStarted`, `/clear` included, a parented task gets `<jeet-brief
from="the assistant" path="assistant/<slug>">` with Goal, Done when, `You
may: <effective>, report` and how to report, through
`session_context_output`. A harness without context injection gets no brief
(D15).

## 4. Reporting

### Up: `jeet report`

```bash
jeet report progress "token refresh fixed; writing the test"
jeet report done "api#131 open, CI green" --link https://github.com/acme/api/pull/131
jeet report blocked "api tests need a Postgres 16 container; none available"
jeet report question "Keep the v1 refresh endpoint for one release, or drop it?" \
  --option keep --option drop
```

`report` is free for any node with a parent, and is the only way up. It:

1. writes a `task_reports` row with `to_id` = the parent;
2. delivers to the parent through `messages::deliver`, by the existing
   idle/busy/not-running rules, fenced:

```
<jeet-report id="42" from="task fix-token-refresh" path="assistant/backend/fix-token-refresh"
             kind="done" untrusted="true">
api#131 open, CI green
</jeet-report>
```

`from` and `path` come from the row jeet resolved the caller to, not from the
text. The body is cleaned by `fence_message`'s rules. A manager relays a
child's report upward by writing its own report, never by forwarding the
child's text as its own. So every hop is fenced once, with its true origin.

### Up, automatically: hook events

The neutral events from #23 feed the parent without the child cooperating.
They are debounced, and at most one `status` report per child per state
change:

| Child event | Parent gets |
|---|---|
| `Stopped`, after a turn with no report since the last hand-over | nothing (avoids chatter); it counts one turn toward the budget |
| `Idle` for longer than `[managers] idle_report_mins` (10), with no `done` | `status: idle 10m; last report #N (kind)` (jeet's words only, not the task's text) |
| `NeedsInput` (a permission prompt, or #23's `trust folder`) | `status: waiting on a prompt only the user can answer`, which also rolls up as **needs you** (no agent may type into a prompt; `messages.rs` already refuses to) |
| `Ended` without `done` | `status: ended without reporting done` |
| budget at 80% / 100% | `status: budget …` (5.3) |

### Questions and the user

Only the **root** talks to the user, and in practice only jeet does it, on the
root's behalf:

1. A child sends `jeet report question`. Its parent answers it if the
   question is within the parent's own brief:
   `jeet report answer 42 "drop it"`. jeet delivers the answer to the asker
   as `<jeet-answer to="42" from="manager backend">`.
2. Otherwise the parent escalates: `jeet report escalate 42 [--note "…"]`.
   `to_id` moves to the grandparent, and the original row keeps its origin.
   The root's only escalation is to the user: `jeet report escalate 42` at the
   root sets `needs_user = 1`.
3. With `needs_user = 1`, every ancestor shows **needs you** in the pane, and
   jeet sends one desktop notification (via `notify.rs`, as `jeet watch` does
   today).
4. The user answers in the pane (`a`) or with `jeet report answer 42 "drop
   it"` at a terminal, outside any agent. That answer goes **straight to the
   asker**, fenced as `<jeet-answer from="user">`. It is the one message an
   agent may treat as the user's, because jeet only stamps it when the
   command ran at a terminal with no `JEET_TASK_ID`. **Each manager on the
   path gets a copy (decided)**, `<jeet-answer to="42" from="user"
   copy="true">`, so it can update its own plan. A copy is for information:
   the asker acts on the answer, and a manager that disagrees reports up to
   the user rather than countermanding it.

Managers and workers below the root lose `jeet notify`. For them it becomes a report.

**Phase 1 (as built).** `jeet report` takes no `--link`/`--option` (D18).
A report holds at most 2000 bytes once cleaned, so even fenced it fits a
message (D16). The row is kept when delivery fails, and the command says so
and exits 0 (D17). `progress` and `status` reports never start an assistant
session: typed into an idle one, queued for a busy one, held otherwise (D4).
No event fires at N minutes idle and jeet has no daemon, so the idle report
is a sweep from `jeet watch` and the assistant's own Stop hook, debounced in
the index (D9). An end by `/clear` is not reported (D10), nor one of a task
archived or deleted; the race with `runtime::stop` before archiving is
accepted (D21). A hook reports only when `$JEET_TASK_ID` is the task it
located, so a worker in its sibling's directory reports as neither (D14).

## 5. Permissions and safety

### 5.1 Allow-lists

- **The caller's identity** comes from `JEET_TASK_ID`, which jeet sets in each
  node's tmux environment. The root also keeps `JEET_ASSISTANT=1` for
  compatibility. `permissions::authorize` changes from "is assistant mode on"
  to "which node is calling".
- **The effective allow-list** of a node is
  `grant(node) ∩ effective(parent)`, and the root's is `[assistant] allow`.
  It is computed on every check, so removing a capability from a parent (or
  from config.toml) removes it from the whole subtree at once. There is no
  copy to drift.
- Granting checks the subset at creation and at `jeet manager grant`.
  Escalating privilege is impossible by construction: no command writes a
  grant outside `effective(caller)`.
- **Scope.** Every mutating command that names a task (`task send`,
  `archive`, `delete`, `manager stop`, …) is refused unless the target is a
  strict descendant of the caller. The only way to address anything else is
  `jeet report`, which only goes to the parent.
- **No sideways messages.** Siblings, cousins and other subtrees cannot
  message each other: `jeet task send` to any node that is not a descendant is
  refused. Anything two siblings need to share goes up to their common parent
  in a report, and the parent decides what to send down.
- **Workers with a parent are gated too (decided).** Top-level tasks the user
  made stay ungated, as today. A parented worker's default grant is
  `worktree.add` plus `report`, from phase 1.
- **Held capabilities.** `pr.merge`, `pr.comment`, `pr.review` and
  `task.delete` are never part of a default grant. They stay with the root
  unless a parent that holds one names it in `--allow` (or `jeet manager
  grant`) for a child. jeet logs each such grant to the granter's
  `actions.log` and notifies the user.
- **New capabilities:** `manager.create`, `manager.stop`, `manager.grant` and
  `git.push`. `pr.merge` comes from #25 (below). `bash_allow` emits each
  node's harness rules from its effective list.

**Phase 1 (as built).** The root keeps today's reach: any task, the user's
top-level ones included, limited only by `[assistant] allow` (D1). A Node's
effective list is `{worktree.add} ∩ [assistant] allow`, computed at each
check, and empty when its parent is not the assistant (D2); `report` is free,
not a capability (D7). A Node's scope is itself: `worktree add` resolves its
target the way the command does and compares ids (D3). Nodes lose `notify`,
`todo` and `review-job` (D6). Identity: a task with a parent named by
`$JEET_TASK_ID` is a Node even under `JEET_ASSISTANT=1`; otherwise the
assistant is the root and anyone else the user; the working directory never
decides, and an index error fails closed (D13). A user's shell inside a
worker's tmux session inherits its id and is gated as it (D23).

### 5.2 Depth, fan-out, concurrency

Defaults (decided):

```toml
[managers]
max_depth = 3            # assistant = 0; managers at 1-2, workers down to 3
max_children = 6         # per manager; also --max-children per manager, ≤ this
max_live = 8             # running coding agents in the whole tree, managers and workers
idle_report_mins = 10
default_budget = "4h"    # wall clock; no cost budgets until #26
worker_default = ["worktree.add"]   # a parented worker's grant without --allow (`report` is always free)
manager_default = []                # a manager's grant without --allow
held = ["pr.merge", "pr.comment", "pr.review", "task.delete"]
                         # never in a default; passed down only when named, with a notification
```

A `held` capability in `worker_default` or `manager_default` is a config
error.

`max_live` is checked when jeet starts an agent (`task new --prompt`,
`task open`, a message to a stopped agent). Over the limit, the start is
queued: the row is made, its first prompt waits in its inbox, and the parent
is told `status: queued, 8/8 live`. It starts when a slot frees.

### 5.3 Budgets

- **Time:** wall-clock seconds while the agent's pane is alive and not paused,
  from status files.
- **Turns:** one per `Stopped` event.
- **Cost: none for now (decided).** No harness reports usage yet. Cost
  budgets wait for #26 (a `usage()` on the #23 trait). Until then jeet does
  not accept or show them.

A child's spend counts toward every ancestor. At 80% the node and its parent
get a `status` report. At 100% jeet **pauses** the node's subtree and reports
`blocked: budget`. The parent can `jeet manager budget fix-x +1h` within its
own remaining budget, or stop it. (`jeet manager budget` works on any
descendant, workers included.)

### 5.4 Stopping a runaway subtree

```bash
jeet manager pause backend    # subtree: interrupt each running turn, set run_state = 'paused'
jeet manager resume backend
jeet manager stop backend     # subtree: end every tmux session, run_state = 'stopped'
jeet manager stop --all       # the user's panic button: everything below the root
```

`paused` means: `messages::deliver` queues instead of typing, nothing is
started, and the harness's `interrupt` (new, #32; Esc for Claude Code) ends the
current turn. `resume` hands the queue over. A manager may pause or stop its
descendants. Only the user may `--all` or act on the root.

### 5.5 Prompt injection

The threat: GitHub text (an issue body, a PR comment, a CI log) reaches a
worker, gets summarised into a report, and steers a manager that has more
power.

- Untrusted data is fenced at every boundary it crosses: `<github-event
  untrusted>` (today), `<jeet-message untrusted>` (today) and `<jeet-report
  untrusted>` (new). The manager template says plainly that a report is a
  claim to check, never an instruction.
- **The real control is the allow-list, not the fence.** A worker that was
  fooled can at most report. A manager that was fooled can at most do what
  its effective list allows, and only to its subtree.
- `jeet watch` keeps routing only to the root and to the task a PR belongs to.
  A rule may target a manager (`action = "manager:backend"`, phase 4), and
  the event lands fenced in that manager's inbox like any other.
- Dangerous actions are kept out of reach by default: the `held`
  capabilities stay at the root unless a parent grants one by name, and the
  user hears about every such grant (5.1).

### 5.6 Merge, push, comment

These go through `git`/`gh`, not jeet, so jeet cannot gate them in-process.
They are gated by the harness permission rules jeet writes for each node
(`--settings` for Claude Code):

| Action | Default |
|---|---|
| push own task branch | workers with `git.push` (needed to open PRs) |
| `git push --force`, push to a default branch | denied to every agent |
| `gh pr create` | workers with `git.push` |
| `gh pr comment` / `review` | the holder of `pr.comment` / `pr.review` (the root by default) |
| merge | only through `jeet pr merge` (#25), by the holder of `pr.merge`: the root by default, or a node a parent granted it to by name. `gh pr merge` and the merge API are denied to every node |

`jeet pr merge` refuses a PR that is a draft, conflicted, failing or pending
checks, or awaiting changes or review, and merges only the head it checked
(#25). So the capability is the only control that needs the hierarchy.

This needs a *deny* rule list from the harness, not just allow rules (§7, #32).

### 5.7 What this does not protect against

Every agent runs as the user, with a shell. An agent that sets out to bypass
jeet can unset `JEET_TASK_ID`, edit `config.toml`, or attach to another tmux
session. The gate stops honest mistakes and agents steered through jeet's own
commands. A hard boundary needs the harness's sandbox and deny rules: no
writes outside the node's own directory, and no `tmux`, `sqlite3` or edits to
`~/.jeet/config.toml`. jeet writes those rules where the harness supports
them. The `auto_trust` scope from #23 must exclude manager homes, as it
already excludes the assistant's, unless the user opts in.

**Decision:** this cooperative model is accepted for phase 1. There the only
manager is the assistant, and it already runs under it. The model must be
reviewed again before phase 2 ships (see the gate in §8, tracked in #33).

## 6. Agents pane (TUI)

**A third tab view in the tasks list (decided):** `⇥` cycles active → agents
→ archived. In the agents view, the assistant row moves from pinned to the
tree's root. The tab is called *agents*, which is the UI text that decision 1
allows.

```text
┌ jeet · agents ──────────────────────────────────────────────────────────────┐
│ live 5/8   needs you 1   / filter  ⇥ switch                                 │
└─────────────────────────────────────────────────────────────────────────────┘
 ✦ assistant           ● running              3 children   inbox 2  todo 4
 ├─ ▾ backend          ◆ needs you (1)        2h10/6h      inbox 1
 │  ├─ fix-token-ref…  ◆ needs input – Bash   0h40/2h      api#131
 │  └─ api-docs        ● running – Edit       0h05/1h
 ├─ ▸ frontend         ⏸ paused (2 below)     3h50/4h      budget 96%
 └─ fix-login          ✓ done                              web#88
```

**Rollup**: a manager shows the most urgent state in its subtree, in the
order needs you > needs input > blocked > budget > running > idle > done. A
count is shown when that state comes from below. Rollup is computed from
status files and `task_reports` in the existing two-second refresh thread.

| Key | |
|---|---|
| `⏎` | attach to the node's coding agent (the assistant picks a session, as `C`) |
| `→` / `←` | expand / collapse |
| `l` | the worklog (`notes/worklog.md`), read-only, scrollable |
| `i` | the node's inbox and reports, newest first; `⏎` opens a linked PR |
| `a` | answer the open question nearest this node (§4, "Questions and the user") |
| `b` | brief, grant, effective allow-list, budget |
| `p` / `P` | pause / resume the subtree |
| `x` | stop the subtree (asks first, listing what is running) |
| `n` | new task or manager under this node (#21's form, plus parent, goal, done, budget, allow) |

## 7. Harness neutrality

Managers run on any harness that implements the #23 trait. A harness that
lacks a **required** item refuses `jeet manager new --harness <id>` with the
missing piece named. Workers keep #23's degrade-don't-fail rule.

| Need | #23 trait today | Required for a manager? |
|---|---|---|
| launch with a first prompt; resume | yes | yes |
| `SessionStarted`, `Stopped`, `Idle`, `NeedsInput`, `Ended` events | yes | yes (rollup, budgets, auto reports) |
| add context at session start | yes | no: falls back to `BRIEF.md` + `AGENTS.md` telling it to read it |
| continue-turn output (to hand the inbox over at Stop) | yes | yes (else reports wait for the idle typing path) |
| allow rules from capabilities | yes (command prefixes) | yes (else every action prompts the user, which is not autonomous) |
| **deny rules** | no, **new** (#32) | yes for children (5.6) and read-only worktrees (§1) |
| **interrupt** a running turn | no, **new** (#32) | yes (pause) |
| **usage** (tokens/cost) | no, **new** (#26), optional | no; cost budgets wait for it |
| instructions file | yes (`instructions_file`, #22) | yes (manager role template) |

Goose (per #23's survey; #28) would qualify once it can write hook files (#27). Its lack
of a `NeedsInput` event means a prompt it shows is seen only as `idle`, so
it is acceptable only as a worker until then.

## 8. Phasing

**Phase 1: parent link and reporting up. The assistant is the only manager.** (#31)
- `tasks.parent_id` (v4 migration, without the `manager` kind or the other
  tables except `task_reports` and `task_briefs`). A task the assistant makes gets
  `parent_id = assistant`.
- `jeet report progress|done|blocked|question`, and the automatic `status`
  reports from #23 events.
- The assistant's session context lists its children with their last report.
- Parented workers are gated (default `worktree.add` + `report`), and scope
  checks apply.
- Touch points: `db/mod.rs` (v4), `db/tasks.rs` (parent, children,
  descendants), `cli.rs` + `commands/task.rs` (`--parent`, `--goal`, `--done`,
  `report`), `permissions.rs` (identity by `JEET_TASK_ID`, scope, gating
  parented workers), `assistant/mod.rs` (`fence_report`, children in
  `context_text`), the hook path in `status.rs`/`runtime.rs` (events →
  parent), `messages.rs` (deliver to parent), the task `jeet` skill in
  `tasks.rs` (teach `jeet report`), and `tui/tasks_ui.rs` (a `↳ assistant`
  marker and the last report in the row).

Phase 1 as built: review tasks the root makes with `jeet review` are
parented; the TUI's `browse_pr` stays top level (D5; both call sites run in
the foreground). The `[review] command` of a parented review task runs with
that task's `JEET_TASK_ID` and without `JEET_ASSISTANT`, so it is the task's
Node and reports; the `review-job` runner around it stays the assistant's,
and, as the command has no hooks, sends the `ended` status report when it
exits without having reported done. No tree helpers and no delete/archive pre-checks yet, since
only the assistant can be a parent and both already refuse it; FK RESTRICT is
the backstop (D19). Only `jeet watch`'s background job drops `JEET_TASK_ID`
(D20). The TUI marks a parented row ` ↳ assistant` and dims its last report
in what the name leaves of its column (D22; the columns after it fill the
row, so past the last column it would never show).

**Gate before phase 2 ships: review the identity model (5.7, #33).** Phase 1 accepts
the cooperative model. Phase 2 lets managers below the root hold real
capabilities, so before it ships, decide whether the cooperative model is
still enough. The alternative is to require harness enforcement (deny rules,
or sandboxing that keeps a node out of `tmux`, `index.db`, `config.toml` and
other nodes' homes) for any harness that runs a manager below the root.
Record the outcome in this doc.

**Phase 2: managers.** (#34; needs #32 and #33)
- `kind = 'manager'`, `jeet manager new|ls|show|grant|budget|pause|resume|stop`,
  `task_grants`, effective allow-lists, held capabilities, depth/fan-out/
  `max_live`, time and turn budgets, read-only worktrees, and recursive
  archive/delete.
- Generalise `assistant/` into `manager/`: `ensure_files` with a manager
  template, `session_context`, multi-session windows. The assistant becomes
  `manager::root()`.
- Touch points: new `src/manager/` (from `assistant/mod.rs`),
  `permissions.rs`, `config.rs` (`[managers]`), `tasks.rs`
  (archive/delete/unarchive cascade), `worktrees.rs` and `db/tasks.rs`
  (`read_only`), `harness/` (`interrupt`, deny rules),
  `startup.rs` (#23 trust scope excludes manager homes), `cli.rs`.

**Phase 3: the agents pane.** (#35)
- The agents tab, rollup, `l`/`i`/`b`/`a`, pause/stop keys, and `n` with
  parent fields.
- Touch points: `tui/tasks.rs`, `tui/tasks_ui.rs`, `tui/state.rs`, a new
  `tui/managers.rs`, and #21's form (`tui/tasks.rs` new-task form).

**Phase 4: escalation to the user and the rest.** (#36)
- `needs_user`, `jeet report answer|escalate`, answers from the user (with
  copies to each manager on the path), `jeet watch` rules targeting
  managers, and `--reparent`.
- Touch points: `commands/` (report), `notify.rs`, `watch.rs` (routing), and
  the TUI answer flow.

**Later: cost budgets**, once #26 gives a harness `usage()`. They need a
migration for the cost columns, `$<n>` in `--budget`, and a cost column in the
pane.

Each phase is useful on its own. Phase 1 alone fixes the commonest pain:
the assistant hears when a task finishes or gets stuck, without polling
`jeet task ls`.

## 9. Alternatives considered

- **Two concepts (a `managers` table beside `tasks`).** Cleaner names, but it
  doubles status, inbox, tmux, archive and TUI code, and the assistant is
  already a task row. Rejected.
- **The harness's own subagents (e.g. Claude Code's Task tool).** Free and
  fast, but invisible to jeet: no worktrees, no status, no allow-list, no
  pane, and not harness-neutral. These stay fine *inside* a worker.
- **A flat list with groups/tags.** Gives grouping in the TUI but no
  delegation of authority or reporting. It does not answer the request.
- **Managers that poll (`jeet task ls` on a timer) instead of reports
  pushed.** Simpler, but it burns turns, misses `needs input` until the next
  poll, and leaves no record. Pushed reports reuse the inbox, which already
  handles idle/busy/not running.
- **A jeet daemon owning the tree.** It would make budgets and rollup exact,
  but jeet has deliberately stayed daemon-free (`jeet watch` is a launchd
  job). The hooks plus the TUI's refresh thread are enough.
- **Copying the parent's allow-list into the child at creation.** It goes
  stale when the user narrows the root. Intersection at check time does not.

## 10. Decisions

None of the questions this RFC raised are still open. The user decided
(2026-10-11):

1. **Name.** The code and data model say *manager*; UI text may say *agent*
   where that is unambiguous (§1).
2. **Worktrees.** Managers may hold read-only, detached worktrees for
   planning, but no writable ones (§1).
3. **Gating.** Parented workers are gated, with `worktree.add` + `report` by
   default from phase 1 (5.1).
4. **Merge.** `pr.merge` comes from #25 (`jeet pr merge`). The root holds it
   by default, and a parent may grant it down only by naming it. It is never
   in a default grant (5.1, 5.6).
5. **Answers.** The user's answers go straight to the asker and are copied
   to each manager on the path (§4).
6. **Defaults.** `max_depth` 3, `max_children` 6, `max_live` 8, a 4h time
   budget. No cost budgets until a harness can report usage (#26) (5.2,
   5.3).
7. **Pane.** The agents pane is a third tab view (§6).
8. **The user may make managers directly.** `jeet manager new` at a
   terminal makes one parented to the assistant (§3).
9. **No sideways messaging.** Siblings never message each other; everything
   goes through the common parent (5.1).
10. **The cooperative identity model is accepted for phase 1.** It must be
    reviewed again before phase 2 ships, which is a gate in §8 (5.7, #33).

### Phase 1 implementation notes

Where phase 1 (#31) settled what this RFC left open, or departed from it:

- **D0** v4 is additive (ADD COLUMN `parent_id`, `task_briefs`,
  `task_reports`); the rest moves to v5, whose rebuild keeps `parent_id`.
  A rebuild gained nothing and put `tasks_one_assistant` at risk.
- **D1** The root's scope is all tasks, as today: narrowing it would break
  the assistant's work and watch routing.
- **D2** Node effective = `{worktree.add}` ∩ `[assistant] allow`, checked
  each time; empty under a non-assistant parent. A child never exceeds its
  parent.
- **D3** A Node may `worktree add` only to itself, resolved as the command
  does and compared by id.
- **D4** `deliver(.., may_start)`: progress and status reports never start
  an assistant session.
- **D5** Review tasks the root starts are parented; the TUI's stay top level.
- **D6** Nodes may not `notify`, `todo` or run `review-job`.
- **D7** `report` is free, not a capability, so an empty allow list still
  hears from its tasks.
- **D8** `[managers] idle_report_mins` (default 10) in `config.rs`.
- **D9** Idle reports come from a sweep in `jeet watch` and the assistant's
  Stop hook, debounced in the index; idle `since` holds still.
- **D10** `Ended { cleared }`: a `/clear` is not reported.
- **D11** Fencing stays in `assistant/mod.rs` (`fence_body`, `attr`,
  `fence_report`, `fence_brief`).
- **D12** `--parent` takes only `assistant`.
- **D13** Caller is User, Root or Node, by `$JEET_TASK_ID` and
  `JEET_ASSISTANT` only; the narrower reading wins; errors fail closed.
- **D14** Hooks report only when `$JEET_TASK_ID` is the located task.
- **D15** `--goal`/`--done` are stored in `task_briefs` and shown as
  `<jeet-brief>` at session start; no `BRIEF.md` yet.
- **D16** Reports are capped at 2000 bytes.
- **D17** A report is kept even when it cannot be delivered.
- **D18** No `--link`/`--option` yet.
- **D19** No tree helpers or delete/archive pre-checks yet.
- **D20** Only `jeet watch`'s background job drops `JEET_TASK_ID`.
- **D21** No ended report for an archived or missing task; the archive race
  is accepted.
- **D22** TUI: ` ↳ assistant` and the last report, dimmed.
- **D23** A user shell in a worker's tmux session is gated as the worker.
