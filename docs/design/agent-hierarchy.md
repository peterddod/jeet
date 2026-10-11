# Hierarchical agents

Status: **draft, for review**. Phase 1 (#31), #32 and #33 are built; the
notes after §10 say where they departed from this text. Every node runs
locally in tmux (5.7, §11); a confined or cloud placement is future work
(§13), not planned.
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

**Phase 2 (as built).** `tasks::add_worktree` refuses `--branch` for a
manager ("a manager holds read-only worktrees; delegate the change to a
worker (`jeet task new`)"), detaches at `origin/<default>` and records
`read_only`; `reconcile` adopts a worktree a manager made itself as
read-only and warns, through `task ls` and `worktree ls`, when a read-only
worktree is on a branch; `worktree ls` shows "(read-only)". A manager's deny
rules add `git commit`, `git push`, `git checkout -b` and `git switch -c`.
There is no chmod and no write deny, and managers run unconfined: this is a
lint that keeps an honest manager from writing code, not a guarantee (5.7).

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

### Schema (v6)

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
were to move to v5. #33 took v5 instead, also additive: `tasks.placement`
(only `local` for now, the default, checked by a `CHECK`)
and the `task_anchors` table (5.7). The `manager` kind, `run_state`,
`task_grants`, `task_budgets` and `read_only` move to v6, whose rebuild of
`tasks` must copy `parent_id` and `placement` and re-create
`tasks_one_assistant` and `tasks_parent`.

**Phase 2 (as built).** v6 rebuilds `tasks` with the `manager` kind and
keeps v5's `CHECK (placement IN ('local'))` unchanged; a confined placement
widens it in a migration of its own. It adds three columns to `tasks`:
`run_state`, `run_state_by` (who set a state other than `run`: `user`,
`root`, `budget` or a task id; NULL exactly while it runs, enforced by a
`CHECK`; no FK, since a node setter is an ancestor of the row; a state
never drops in severity and keeps its highest-ranked setter even as it
rises, so a node's stop does not take over a user's pause) and
`max_children` (a manager's own fan-out limit). `task_grants` and
`task_budgets` are as above, with `CHECK`s that a limit is positive and a
`warned` column (0, 80 or 100) recording the last threshold told. A
`task_start_queue` table (task id, queued at) holds starts waiting for a
`max_live` slot, and `task_worktrees.read_only` is added. v6 backfills a
`worktree.add` grant for every parented `work` and `review` task, so phase-1
children keep what they could do; nothing else is granted by default. Until
creation moves to `--allow` and the `[managers]` defaults, a parented `work`
or `review` task made the phase-1 way is written with that same grant, so no
child made after the migration is left without it.

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

**Phase 2 (as built).** The refusals live in `tasks::{archive, unarchive,
delete}`, which the CLI, the TUI and import share, and apply to any task
with children; the CLI and the TUI also check before asking. Delete is refused while any child exists, before anything is
stopped or removed (the `RESTRICT` key would otherwise refuse only the row,
after the disk work).
Archive is refused while a child is active, and unarchive under an archived
parent or when the parent already has its `max_children` active children
(the root has no limit). Each of these two checks once before any work and
again with the state change, in one IMMEDIATE transaction, so a child made
meanwhile cannot slip past. `--recursive` comes later in phase 2;
`--reparent` is phase 4.

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

- **The caller's identity** is the task whose pane or review job jeet
  launched, found by walking the caller's process ancestry (5.7, #33).
  `JEET_TASK_ID`, which jeet sets in each node's tmux environment, and the
  root's `JEET_ASSISTANT=1` are hints that must agree with it.
  `permissions::authorize` changes from "is assistant mode on" to "which
  node is calling".
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
error. Left unset, `worker_default` is the built-in `["worktree.add"]` less
whatever `held` names, so holding `worktree.add` needs no second key.

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
started, and the harness's `interrupt_keys` (#32; Esc for Claude Code) end the
current turn. jeet presses them once, and only while the agent is running or
waiting on a prompt: in a waiting agent Esc refuses the pending tool call and
ends the turn, while at an idle prompt it would rewind or clear a draft, and
at the trust dialog it would exit. A node whose pane holds unsent input the
user is typing is left alone. `resume` hands the queue over. A manager may pause or stop its
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
They are gated by the harness permission rules jeet writes for the root and
every parented node (`--settings` for Claude Code). Top-level tasks the user
made get no deny rules, by design: they stay ungated (D1, D23), and a deny
would also refuse the user's own `--force-with-lease` with no prompt to
approve it.

| Action | Default |
|---|---|
| push own task branch | workers with `git.push` (needed to open PRs) |
| `git push --force`, push to a default branch | denied to the root and every parented node |
| `gh pr create` | workers with `git.push` |
| `gh pr comment` / `review` | the holder of `pr.comment` / `pr.review` (the root by default); denied to a parented node that does not hold it |
| merge | only through `jeet pr merge` (#25), by the holder of `pr.merge`: the root by default, or a node a parent granted it to by name. `gh pr merge` and the merge API are denied to the root and every parented node |

`jeet pr merge` refuses a PR that is a draft, conflicted, failing or pending
checks, or awaiting changes or review, and merges only the head it checked
(#25). So the capability is the only control that needs the hierarchy.

This needs a *deny* rule list from the harness, not just allow rules (§7, #32).

Deny rules are a lint, not a gate. They are matched against the start of a
command, so `git -C . push -f`, `sh -c '…'`, aliases, scripts, MCP tools and
`!` commands get past them (`docs/harnesses.md`, "Deny rules"). They stop
honest mistakes. GitHub branch protection or a ruleset is the real control
on a default branch. Nothing confines a node yet (5.7).

### 5.7 What this does not protect against

Every node, managers below the root included, runs locally in a tmux pane,
as the user, with a shell: cooperative and unconfined (§11). An agent that
sets out to bypass jeet can detach from its pane's process tree with the
env stripped, spawn through `tmux`, or edit `config.toml` or `index.db`
(the table below lists what is open and closed). The gate stops honest mistakes and agents
steered through jeet's own commands. jeet writes no confining rules (no
write limits outside the node's own directory, no refusal of `tmux`,
`sqlite3` or edits to `~/.jeet/config.toml`): its deny rules cover only
pushes and merges (5.6). Confinement would come from a confined placement,
which is future work (§13). It goes the other way for jeet itself: the
settings jeet gives Claude Code list `jeet` in `sandbox.excludedCommands`,
since the ancestry walk cannot see past a sandbox's pid namespace (Linux)
and jeet writes under `~/.jeet`, so with the user's sandbox on, jeet, and
possibly whatever a Bash command chains with it, runs unconfined. The
`auto_trust` scope from #23 covers managers as it covers workers: a manager
is started by its parent with nobody at its pane, so a trust prompt would
stall it, and `[harness] auto_trust` is the user's opt-in. The assistant's
home stays excluded.

**Placement (#33).** Where a node runs is its *placement*, recorded per node
in the index (`tasks.placement`). There is one, `local`: tmux panes on the
user's machine. Starts and interrupts go through `placement::runtime_for`,
the seam a confined placement (a local sandbox such as sandvault, or a
cloud runtime) would plug into later (§11). No node is refused for where it
sits in the tree.

**Local identity.** A local caller is the task bound to the nearest
`(pid, start time)` in its process ancestry that jeet recorded when it
launched that pane or review job. The record is written only by the
launching jeet, never re-bound, and ignored after a reboot. `JEET_TASK_ID`
and `JEET_ASSISTANT` are hints. A hint that disagrees with the anchor is
refused and logged. A hint that claims a parented task or the root with no
anchor behind it is refused as unconfirmed. Hooks carry their task in argv
(`--task`), and report up only when that agrees with the anchor. These
anchors fail closed against mistakes and casual forging. They are not a
boundary against a local agent that sets out to escape:

| Escape | Status |
|---|---|
| Forging `JEET_TASK_ID` / `JEET_ASSISTANT` | closed (mismatch / unconfirmed) |
| Stripping the env inside the pane | closed (the anchor still binds); open for a pane whose anchor could not be recorded (jeet warns at start) |
| A Node claiming Root | closed (mismatch) |
| Pid reuse | closed (start time; on Linux tagged with the boot id) |
| A pre-created `jeet-<slug>` session | closed (not adopted; warning) |
| Calling `review-job` directly | closed (go byte plus job anchor at the process itself) |
| Forged `task-hook --task <other>` | closed for reports up; status files remain writable |
| Orphaning (`nohup … & exit`, `setsid`, double fork) with the env stripped | **open locally**: resolves as the user |
| Spawning through tmux (`new-window`, `split-window`, `new-session`, or `send-keys` into another pane, including the assistant's) | **open locally**: the process has no anchor, or acts as the other pane |
| Driving jeet's interactive screens (`jeet tasks`, `projects`, `chat`) through a fake terminal (`script`, a pty) | **open locally**: an interactive command needs only a terminal, kept so a person at a pane's own terminal still gets the screen; the screen then acts ungated |
| Direct edits to `index.db` or `config.toml`; a forged `JEET_HOME` (acts only on a fake index) | **open locally** / harmless |
| User windows opened inside a parented task's or the assistant's session | refused for gated commands (inherited hint, no anchor), by design |
| jeet's own helpers (`watch --once`, `task-startup`) while their parent is alive | may resolve as the task; they are Free and must never ask for identity |

The open rows close only when a confined placement exists, which is future
work (§13); until then they stay open for every node. Agents started before
#33 have no anchor and must be reopened.

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
| **deny rules** | yes (#32): `enforcement() >= Rules`; Claude Code only; a lint, not a gate (nothing confines a node yet, 5.7) | yes for children (5.6) and read-only worktrees (§1) |
| **interrupt** a running turn | yes (#32): `interrupt_keys`; Claude Code only (Esc) | yes (pause) |
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

**Gate before phase 2 ships: met by #33.**
- Local callers are identified by process ancestry and fail closed (5.7).
- Every node has a placement (`local`), and starts and interrupts go
  through `placement::runtime_for` (§11).
- Phase 2a, an optional watcher wake channel for local Claude Code nodes
  (§12), runs its spike first and does not block phase 2.

**Phase 2: managers.** (#34; needs #32 and #33)
- Managers just work locally: every node, managers below the root included,
  runs in a local tmux pane like any other task (5.7), with budgets,
  pause/stop and interrupt as designed for local panes (5.3, 5.4).
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
  `startup.rs` (#23 trust scope: managers follow `auto_trust` as workers do), `cli.rs`.

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

None of the questions this RFC first raised are still open. The user decided
(2026-10-11). The questions a cloud placement would raise are parked with it
in §13.

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
10. **Identity and placement (#33).** Every node, managers below the root
    included, runs locally in tmux, cooperative and unconfined, through a
    per-node placement that is `local` only for now; a confined placement
    (sandvault is a candidate) can plug into the seam later, and the cloud
    design is parked as future work (§13). Local callers are identified by
    process ancestry to a pane or job jeet recorded, and fail closed; the
    env is a hint. `libc` is approved as a direct dependency for macOS pid
    lookups (5.7, §11, #33). This replaces an earlier #33 decision that
    managers below the root must run behind a boundary (I22).

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
  A hook whose cwd is the assistant's home but whose `$JEET_TASK_ID`
  names another task is that task's: no assistant context, no handover
  of the assistant's inbox.
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

### #32 implementation notes

Where #32 (interrupt and deny rules on the harness trait) settled what this
RFC left open, or departed from it:

- **E0** `Harness::enforcement() -> Enforcement { None, Rules }`, not a
  `supports_deny()` bool: a bool probe cannot tell a harness that drops
  `deny` from one that applies it, and #33 only adds `Sandbox`.
- **E1** `HookSetup.deny: Vec<String>` of neutral command patterns
  (`HookSetup` derives `Default`). No write denies and no `config.toml`
  edit deny yet: those wait for phase 2's read-only worktrees and #33's
  confinement.
- **E2** Pattern syntax: words from the start of the command, `*` for any
  run of characters, further arguments implied. Claude Code renders
  `Bash(<p> *)`, or `Bash(<p>)` when `p` ends in `*`; allow rules keep
  `Bash(x:*)`. `deny` is left out when empty, so a launch without denies
  is unchanged.
- **E3** Patterns are anchored at the start: an unanchored deny would
  refuse `git commit -m "…gh pr merge…"` with no way to approve it. The
  gaps this leaves are pinned by a test (`KNOWN_GAPS`).
- **E4** Deny rules go to the root and every parented node. Top-level
  tasks get none (D1, D23), so 5.6's "every agent" now reads "the root and
  every parented node". The exception is a parented review task's
  `[review] command`: the user's own command, run by `review-job`, not
  launched through a harness, so jeet has no settings to put the rules in.
  `jeet review` warns when it starts one, as E13 does.
- **E5** Default branches are every indexed repo's `default_branch` plus
  `main` and `master`, deduplicated, snapshot at launch, and capped at
  eight (`MAX_DEFAULT_BRANCHES`): `main`, `master`, then the ones most
  repos use. Each costs three patterns, rendered for Claude Code as two
  rules each, with and without a trailing ` *`, since its trailing ` *`
  matches the bare command only in a rule with no other `*`. #32 also
  capped a name at 24 bytes, for tmux's 16KB command limit, which the
  inline settings shared with the first prompt, and #33's `--task <id>`
  briefly cut it to 16; with the settings in a file (E9) there is no
  length cap. A repo's `default_branch` is
  `origin/HEAD` at clone or import, or else the branch then checked out,
  and is not refreshed, so a wrong guess denies pushing to that branch
  name everywhere; harnesses.md says so.
- **E6** `Capability.external` names the `gh` commands a capability
  stands for (`pr.comment`: `gh pr comment`, `gh issue comment`;
  `pr.review`: `gh pr review`). A held one is pre-approved for the root
  (`allowed_commands`); a parented node holding one is only not denied
  it, and still asks at the user's prompt (node allow lists stay
  `ALLOWED`, E8). A parented node is
  denied the ones it does not hold. The root is never denied them: it
  holds them by default (5.1), and a deny would refuse them even with the
  user's approval. The root's denies are force pushes, default-branch
  pushes and merging. No `gh api -X/-f/-F` write denies: they would break
  `gh api graphql -f query=` reads. So a node without `pr.comment` or
  `pr.review` can still ask the user to approve the same through `gh api`
  (`issues/*/comments`, `pulls/*/reviews`, `addComment`,
  `addPullRequestReview`): a documented gap, pinned in `KNOWN_GAPS`, as
  are commits through the contents endpoint and `createCommitOnBranch`.
- **E7** Merging is denied to the root and nodes: `gh pr merge`, the REST
  merge endpoints through `gh api` or `curl`, and the GraphQL
  `mergePullRequest`, `enablePullRequestAutoMerge`, `mergeBranch` and
  `enqueuePullRequest` mutations, matched anywhere after `gh api` or
  `curl`. `git push --branches` is denied with `--all`, its synonym.
  `jeet pr merge` runs `gh` from jeet's process and is unaffected.
- **E8** A node's allow list is unchanged; narrowing it is phase 2's.
- **E9** #32 kept the settings inline on argv; #33 writes them to a 0600
  file, `$JEET_HOME/run/<task id>/<session>.json` (directory 0700), and
  passes `--settings <path>` (`Harness::hook_file`, `runtime::hook_args`).
  It is written to a new file renamed into place at each launch, so a
  running agent never reads a half-written one. Each launch removes the
  task's other session files but for running sessions' (the assistant's
  live windows) and any written in the last minute, so the assistant's
  chats do not pile them up; the rest go at archive and delete. It is outside the task and every worktree, so an agent editing
  it still meets a permission prompt; as the same user it can, like
  `index.db` (5.7). A test checks the first prompt fits one tmux command
  with only the path on it, where the inline settings no longer would.
- **E10** `runtime::interrupt` returns `Interrupted { Sent,
  NotWorking(activity), UserTyping, Watched, AtStartup }` and does not fail
  when there is nothing to interrupt, so phase 2's pause can walk a subtree
  with idle nodes. Under the assistant, an interrupt that sent nothing is
  logged as `skipped` (with why) and not announced, rather than as done.
- **E11** Keys are pressed once, only while `Running` or `Waiting`, and
  never over the user's draft. Not at a startup prompt either (Esc at the
  trust dialog exits): a Waiting no hook reported, synthesised for an agent
  with no status or written by the startup watcher, returns
  `Interrupted::AtStartup`. Nor at `Waiting` while a client is attached to
  the session (`Interrupted::Watched`): the user may be typing into the
  dialog's own text field, which `has_draft` does not see. The same goes
  for a `Running` status the harness says is a dialog of its own
  (`Harness::in_dialog`; Claude Code reports `AskUserQuestion` and
  `ExitPlanMode` only through PreToolUse, and no hook reports the answer,
  so an answered dialog still counts until the next tool call or the
  turn's end: a refusal, the safe direction). Known gap: a permission prompt
  reads as `Running` until Claude's notification, which it sends only
  once the user has left the prompt alone for some seconds, so an
  attached user typing feedback into one may still have it discarded. No retry and no confirmation. Since
  2-ii an interrupt that sent its keys charges the spell so far
  (`run::charge_interrupted`, reporting `blocked: budget` if that uses the
  budget up) and writes an idle status (`Interrupted`, or `Paused` for a
  pause), as no Stop hook follows; a manual Esc still reads `running`
  until the next hook. The assistant is refused (5.4).
  The draft check and the keys aim at the agent's own pane, by the id
  (`%3`) jeet notes in the session option `@jeet-agent-pane` when it
  starts the session, not the session's active pane, which may be a pane
  the user split off the agent's window or a window they opened there; a
  session started before the id was noted falls back to the active pane
  of its first window (`=<session>:^`). `jeet task send` and the startup
  watcher aim there too.
- **E12** `jeet task interrupt <name>` (new CLI), gated on `task.send`
  (whose pre-approved commands include it, so an allowed interrupt does
  not prompt);
  nodes are refused because it is not in their grant.
- **E13** A parented task launched on a harness with `Enforcement::None`
  gets a warning that its agent is not stopped from force-pushing or
  merging, and only jeet's own commands are gated (§7, degrade, don't
  fail).
- **E14** The manager refusal, `missing(h, &[Feature]) -> Vec<Feature>`
  with `Feature::label` naming the piece ("deny rules", "interrupt"),
  lands with phase 2's `manager new`; it has no caller in #32.
- **E15** Deny rules are a lint, not a gate (5.6). They cover only the
  agent's Bash and file tools, not MCP tools, `!` commands or scripts.
  Branch protection is the real control; #33 is the boundary.

### #33 implementation notes

Where #33 (identity that is hard to fake, and the placement seam) settled
what this RFC left open, or departed from it:

- **I0** Scope: ancestry identity, `Placement`, a `NodeRuntime` trait with
  only `LocalTmux`, and this text. No cloud code, no `[cloud]` config, no
  `Delivered::Sent`/`Watched`, no MAC crate.
- **I1** The anchor is the identity: a `(pid, start)` row in
  `task_anchors`, kind `pane` or `job`, recorded only by the jeet that
  spawned that pid, at one of three launch paths: a task's tmux session
  (`runtime`), an assistant window (`assistant::open`) and a review job's
  runner (`jeet review`). A process never records its own anchor.
- **I2** The walk starts at jeet's own pid and goes up through the kernel:
  `/proc/<pid>/stat` on Linux (split after the last `)`), `proc_pidinfo`
  (`PROC_PIDTBSDINFO`) through `libc` on macOS. It stops at pid ≤ 1, a
  failed lookup, a zombie, a cycle or depth 64. Other unix targets find
  nothing and fail closed. jeet never runs `ps` for identity. macOS reads
  only the caller's own processes in full, so a walk from a terminal stops
  at the root-owned `login` (or `sshd`) above the shell; a failed lookup of
  another user's process (its owner read with `PROC_PIDT_SHORTBSDINFO`,
  which anyone may) ends the chain like init, and only one of the caller's
  own, or one it cannot even name the owner of, is reported as unreadable
  (the refusal then blames a sandbox).
- **I3** Start times are re-read on every check. On Linux, where they count
  clock ticks since boot, the start is tagged with a hash of
  `/proc/sys/kernel/random/boot_id`, so a row from an earlier boot matches
  no process and is pruned. Not the boot time, which moves when the clock
  is stepped (deviation: the plan compared rows' `created_at` with
  `btime`). macOS start times are absolute.
- **I4** Resolution, nearest anchor first: a job anchor at the process
  itself is that job; an anchor whose task disagrees with `JEET_TASK_ID`,
  or a non-root anchor under `JEET_ASSISTANT`, is a mismatch; an anchor is
  that task; no anchor but a hint naming a parented task, the root or a
  task the index does not know is unconfirmed; anything else is the user.
  Deviation: the plan took an unknown task for the user, but a review job
  outliving its deleted task would then run ungated; `task delete` also
  ends the task's review job (its runner's process group), and so does
  `task archive`: the runner recorded as the task's, and an unfinished one
  in the task's worktrees whose recorded runner is still live, which is how
  a job the assistant started for a top-level review task (recorded as the
  assistant's) is found. `jeet review` writes the runner's pid and start
  into the job's file before the go byte, so the job is found from the
  moment it is launched. Both wait for the job's processes to exit
  (killing what outlives the grace period) before anything is saved or
  removed, and both are refused from a jeet running under that job, which
  ending it would end part-way.
- **I5** An anchored assistant is Root, an anchored task with a parent is a
  Node, anything else the user. Supersedes D13 and D23: a user shell in a
  parented task's session is refused for gated commands, not gated as the
  task.
- **I6** Mismatch and unconfirmed are errors; a mismatch is also logged as
  Denied in `actions.log`, wherever identity is resolved. An index error
  fails closed.
- **I7** `authorize` resolves identity for every gated or node-only
  command, whatever the env says, and never opens the index for hooks and
  other free commands; those that act on who the caller is resolve it
  themselves (I21).
- **I8** Anchors are a plain `INSERT`, never re-bound. The process is
  already running when its row goes in, so the insert comes first and
  dead rows are pruned after it (or before a retry, when a dead row
  holds the same pid and start). If a start time cannot be read, nothing is
  recorded and jeet warns that the agent cannot be identified. Archiving a
  task deletes its pane anchors; deleting it cascades. A launch's
  settings file is kept while a window launched as that session runs,
  whatever session the window has gone on to (`@jeet_launch`).
- **I9** No backfill: matching sessions by name is spoofable. A live session
  with no live anchor is not adopted; `task open` warns to stop and reopen
  it. `jeet chat` going back to an assistant window whose pane has no live
  anchor warns the same way (exit it there and `jeet chat` again). **Reopen running tasks after upgrading:** older agents are refused for
  `report` and `worktree add`, and old hooks no longer report up.
- **I10** `jeet review` starts the runner with stdin piped, records its job
  anchor, then writes one go byte `g`. The anchor names the review task when
  it has a parent, else the assistant when the caller is the root (a
  top-level review task the root reviews again is never moved under it),
  else the top-level review task itself, which classifies as the user (I5)
  and lets archive and delete find the job; that runner starts without the
  `JEET_TASK_ID` and `JEET_ASSISTANT` it would inherit, which would
  otherwise disagree with its anchor. Deviation: the plan recorded nothing
  for the user, which left a job the user started running after its task's
  archive or delete. The runner's pid and start go in the job file before
  the go byte, so archive and delete match it from the first. If the anchor
  cannot be recorded, the runner is killed and the job ends with 127, except
  the user's runner (top-level review task): the user needs no anchor, so
  its job runs on with a warning that archive and delete cannot end it. A runner refused after the go byte
  records its job as ended with 126, so it never reads as running; so does
  one that gets EOF before the byte, its launcher having died. `review-job` refuses to run on a
  terminal, on EOF or on any other byte ("review-job is started only by
  `jeet review`"), before `authorize`. Every caller of `review-job` is then
  held to the reviews-dir check ("review jobs run only from jeet's reviews
  dir"), and a Node is refused it by `node_only` even with the go byte.
- **I11** Hooks run `jeet task-hook --task <id> <event>`. `--task` locates
  the task first; a `--task` the index does not know writes nothing and does
  not fall back. Without `--task`, today's env-then-cwd lookup still writes
  status, but nothing reports up and no brief is sent. A hook reports up
  only when `--task` is the located task and agrees with the anchor.
  Supersedes D14: a worker whose cwd is a sibling's directory is attributed
  to itself through `--task`. Hooks that carry `--task` always open the
  index.
- **I12** `Placement { Local }` on `tasks.placement` (§11).
- **I13** `NodeRuntime` has `start` and `interrupt` only; `runtime_for` is
  the one fail-closed choke point. The five start sites and the interrupt
  site route through `runtime::ensure_running` and `runtime::interrupt`
  unchanged.
- **I14** No placement rule runs on start or interrupt: `runtime_for` only
  picks the runtime, and fails closed on a placement it does not know.
- **I15** A new task's placement is `local`, wherever it sits.
- **I16** There is no `[managers] placement` key.
- **I17** The env stays the source for self-protection (not killing the
  session you are in) and the `[assistant] notify` opt-out, where a wrong
  hint cannot widen anything. Message sender labels use the identified
  task and fall back to the env, then the cwd; they never fail a send.
- **I18** No `TMUX` or `jeet-` session check: jeet uses the user's default
  tmux server, and an orphan can unset `TMUX`.
- **I19** v5 is additive; the `tasks` rebuild is v6 (§2).
- **I20** `libc` is a macOS-only direct dependency, already in the lock
  file as a transitive one.
- **I21** As built, beyond the decisions above:
  - The refusals read "jeet can't confirm this is task X's agent: reopen
    the task (agents started before this jeet must be restarted), or run
    this outside the agent's tmux session" (or "… this is the assistant:
    exit the assistant in its window and run `jeet chat` again …"), and "this jeet runs under task X's agent (<id>), but
    $JEET_TASK_ID says it is task Y (<id>); jeet refuses a caller that
    claims to be another" (or "… $JEET_ASSISTANT says it is the
    assistant …"). `identify` itself logs a mismatch, so every command
    that resolves identity does (gated and node-only ones through
    `authorize`, `report`, `task new`, `worktree ls`/`path`/`add` through
    `tasks::current`, hooks reporting up). The record is `denied` under
    capability `identity`, in jeet's words only: "task X's agent (<id>)
    claimed to be another task or the assistant". The claimed id and the
    command's text are the caller's and stay out of the log the assistant
    reads as its own.
  - `identify` returns a typed `identity::Refused` (`Mismatch`,
    `Unconfirmed`).
  - `notify` and `todo`, which only the root may run, and every gated
    command open the index and walk the process tree for any caller from
    `authorize`. `report`, `worktree ls`/`path`/`add` without `--task`, and
    hooks with `--task` walk it themselves. `task-startup`, `watch --once`
    and the other free commands do not.
  - `tasks::current` (`worktree ls`, `path` and `add` without `--task`)
    takes the identified task first, then `$JEET_TASK_ID`, then the cwd. An
    identity error propagates, so these are refused as unconfirmed in a
    user window inside a parented task's or the assistant's session.
  - `tmux::new_session` reads `#{pane_id}\t#{pane_pid}` back and fails if
    tmux reports no pane. The pane id it reads is also the one noted in
    `@jeet-agent-pane` (E11), so the anchor and the pane interrupt and send
    aim at are the same pane. A failed record warns "task X's agent cannot be
    identified (<cause>): its jeet commands that name the task are
    refused, but one run without $JEET_TASK_ID and $JEET_ASSISTANT runs as
    the user; stop it and open the task again". The agent is still
    started (I8). A live session with no live anchor warns "tmux session S
    was started by an older jeet or outside jeet, so jeet cannot confirm
    its agent is task X's; stop it and open the task again".
  - The prune after each insert removes rows of dead processes and rows
    whose start differs (on Linux, every row from an earlier boot).
  - The #32 notes' forward references to "#33's sandbox" (E0, E1, E9,
    E15) are superseded: #33 built no sandbox, and nothing confines a
    node; `Enforcement::Sandbox` and write denies wait for a confined
    placement (§13). The 0600 settings file is built (E9):
    `--task <id>` on every hook had filled the inline budget and cut the
    longest default branch denied from 24 bytes to 16.
- **I22** Reversed by user decision. #33 first decided, and built, that
  managers below the root, and every node under them, must run behind a
  boundary (a child never less confined than its parent), with Claude Code
  cloud sessions as the first: `Placement { Local, Cloud, LocalSandbox }`,
  a `Boundary` per placement, `[managers] placement` (default `cloud`,
  `local` an error), and `runtime_for` refusing a local node below a
  non-root parent and any placement not built. The user then dropped the
  requirement: managers run locally in tmux like any other task, the
  placement seam stays (local only), and the cloud design moved to §13 as
  future work, with no probes run and no `hmac`/`sha2`.

### Phase 2 implementation notes

Where phase 2 (#34) settled what this RFC left open, or departed from it.
Managers run locally in tmux, as the user, like any other task (I22).

- **P1** Schema v6 (§2 "Phase 2 (as built)"): the `manager` kind, run
  state with its setter, `max_children`, grants, budgets, the start queue
  and `task_worktrees.read_only`, with the placement `CHECK` still
  `local` only. The data layer (tree walks, grants, budgets, run state,
  the queue) lands with it and is used by later steps.
- **P2** Teardown refusals (§2 "Creation and teardown") land before any
  manager can be made, and apply to every task with children.
- **P3** Every `Manager` arm that restricts a manager lands before
  `jeet manager new` can make one: teardown refusals (P2), read-only
  worktrees and git denies (P4) and the trust scope (P5). Until then no
  manager row exists for a report or a `--parent` to reach (P11, P14).
- **P4** Read-only worktrees (§1 "Phase 2 (as built)") are lint, not the
  #32 write denies §1 anticipated: there are no `Edit(…)` denies and no
  chmod, since a manager runs unconfined anyway. `git branch` is not
  denied: a prefix rule would also deny `git branch -a`, `-r` and
  `--show-current`, which a manager reads with, and a branch made in a
  detached worktree carries nothing without `git commit`, which is
  denied. `reconcile` now returns its warnings; `task ls` and
  `worktree ls` print them, the TUI and teardown do not.
- **P5** A manager follows `[harness] auto_trust` as a worker does (5.7):
  it is started by its parent with nobody at its pane. The assistant
  stays excluded.
- **P6** `[managers]` (5.2) is checked where it is used, not when
  config.toml is read, so a mistake there breaks only the tree. `held`
  adds to a floor of `pr.merge`, `pr.comment`, `pr.review` and
  `task.delete` rather than replacing it. An unknown capability, a held one
  in a default, a limit of 0, and a budget that does not parse or names `$`
  are errors. `default_budget` takes the parts `--budget` does
  (`"2h,40turns"`). A manager without its own
  `max_children` is held to `[managers] max_children` on unarchive too.
- **P7** Effective lists (5.1) come from stored grants: a node's grant cut
  by each ancestor's own list (a manager's grant, the assistant's
  `[assistant] allow`; a worker passes nothing on), read at every check and
  never cached. A chain of parents that does not end at the assistant
  holds nothing, and a broken one is an error. A new child stores its
  kind's default (`worker_default`) uncut, so widening `[assistant] allow`
  later reaches existing children, as in phase 1; a grant someone names
  must be within the parent's list and, for a node granting, its own.
  New capabilities: `manager.create`, `manager.grant` (grant and budget)
  and `manager.stop` (pause, resume, stop). `git.push` is
  deferred (§13).
- **P8** Scope (5.1) is one table, `permissions::node_scope`, saying for
  each command what a node may name: itself (`worktree add`, `ls`,
  `path`), itself or below (`task last`, `path`, `prs`), strictly below
  (`task send`, `open`, `archive`, `delete`, …, `manager show`, `grant`,
  `budget`, `pause`, `resume`, `stop`), a
  manager at or below it as `--parent`, or no task at all (`jeet ls`,
  `jeet prs`, `report`, hooks, screens). A name is resolved within the
  node's subtree, so a prefix cannot reach outside it; delete and
  `--parent` take exact names. A command the table leaves out is refused
  to every node, reads included (`worktree rm`, `projects`, `clone`, …).
  `jeet pr merge` names no task: `pr.merge` is held, so only a grant naming
  it lets a node merge. The root keeps D1; the `jeet manager` verbs act
  only on a task under the assistant, for anyone, and `manager stop --all`
  is refused to all but the user (M11). `task ls` lists a node's
  subtree only. `task send assistant` and `task interrupt assistant` mean
  the assistant to the user and the root only; to a node, `assistant` is a
  name like any other, found in its subtree, so a child named for it is
  that child, never the root.
- **P9** A node's allowed actions go to its own `actions.log`,
  unannounced. A held capability granted is logged by the granter (the
  assistant's log, a manager's own, or the assistant's marked `by user`)
  and announced whatever `[assistant] notify` says, except the user's own,
  once the task is fully made (its worktrees added), so no entry names a
  task that was undone.
  `jeet task last` fences what it prints for any caller but the user: an
  agent's words may carry text it read (5.5). The status beside them and
  an API error, which its agent can write too, reach an agent unable to
  close a fence or open one (as `jeet task ls`'s STATUS column does), and
  everyone without control characters. So does `jeet manager show`,
  for the children's reports and for the brief's goal and done-when, which
  a manager writes from what it has read.
- **P10** A manager's harness settings pre-approve the commands of what it
  holds (§7 "allow rules from capabilities"), read at each start, since it
  is started with nobody at its pane. Never `jeet *`.
- **P11** Making a child (§3, 5.1) is planned, then made: the plan checks,
  writing nothing, the parent (`assistant` or an exact name, within a
  node's subtree), that the caller may make tasks under it, that it is
  active and the assistant or a manager, the depth (a worker at most
  `max_depth` below the assistant, a manager one less), the fan-out (the
  assistant has none), the grant, the budget and, for a manager, the
  harness. Making it writes the row, grant, budget and brief in one
  transaction that refuses a parent archived or filled meanwhile. The
  parent defaults to the assistant for the assistant and to itself for a
  manager; the user's task has none unless given. A brief names who made
  the task, not its parent. `--budget` must fit what is left under the
  parent and every ancestor; `[managers] default_budget`, given to a
  manager and to a manager's child when unasked, is cut to what is left
  rather than refused. Nothing is made under a parent with either unit
  used up, whatever `--budget` names. A worker of the assistant's gets no budget unless
  asked. Every command naming a task finds it as the permission check did,
  then checks its reach again.
- **P12** A manager needs a harness with hook events, continue-turn
  output, `interrupt_keys` and deny rules (§7), and is refused, naming what
  is missing, on one without. A worker starts without them; only a
  worker with a parent is warned, and only of missing deny rules.
  There is no `--harness` flag: the configured harness is checked.
- **P13** `jeet manager ls [--json]` is the tree (a flat list with each
  task's parent and depth), in place of `manager tree --json`. `jeet
  manager show` lists, as open, the children's last reports that are
  `blocked` or `question`. `jeet manager grant` warns of each running task
  below that lost a capability with commands outside jeet (`pr.comment`,
  `pr.review`, which jeet never sees run): its agent may still run them
  until it next starts, when its harness deny rules change.
- **P14** Reports (5.5) go to a manager parent through its pane, as a
  message does: typed in when it sits idle, queued while it is busy, and,
  when it is not running, started only by a `done`, `blocked` or
  `question` from a caller that may start it; anything else is held in its
  inbox. While the parent is held the report waits in its inbox, typed
  into nothing (P18); a start it makes passes the start gate (P17), and is
  queued, with the report in its inbox, over `max_live`.
  `<jeet-report>` names the sender as `task <slug>`
  or `manager <slug>`, with its tree path; `<jeet-brief>` names the parent
  (`the assistant`, `manager <slug>`) with the task's tree path, and its
  last line says whom to report to. A broken chain that hides what a task
  may do costs the brief its list, with a warning, and its path (the bare
  slug), not the brief. `jeet report` names the parent it reported to.
- **P15** A task is named by its kind wherever jeet names one in the tree:
  `manager <slug>` or `task <slug>` in a refusal's "cut by", in `task new`'s
  "reports to" line, in a report's sender and `jeet report`'s output, and
  (with its tree path) in the fences of `task last` and `manager show`. `jeet task ls` marks a
  manager `(manager)` after its name (M29); `jeet manager ls` has a kind
  column.
- **P16** Run state (5.4) is changed by `manager/run.rs`. A pause writes
  the target's row, then interrupts its subtree leaves first, through a
  runtime lookup and an own-session check passed in (production:
  `placement::runtime_for` and `runtime::is_own_session`, which
  `runtime::interrupt` and `refuse_inside_session` share), so the walk is
  tested with a fake runtime and no tmux. It skips the assistant and the
  node this jeet runs in, turns each refusal or error into a line and goes
  on, and marks an interrupted node idle (`Paused`), since an interrupted
  turn has no Stop hook. Each node is interrupted under its inbox lock, so
  a delivery that found it not held yet has typed its message in first.
  A node handed input in the last `STARTING_SECS` (launched with hooks,
  its brief on its command line, or typed a message: `TmuxAgent::type_in`
  touches `.jeet/typed` first) that no hook has reported on since
  (`status::input_unreported`; a status in the same second does not
  count, nor does a session start's, written before the prompt on its
  command line is submitted however late it boots, and a startup-prompt
  status ends the wait) is about to start a
  turn its status does not show: while the interrupt finds it starting,
  idle or at startup, it is tried again every 250 ms, with the inbox lock
  let go in between, until a hook reports or the window passes.
  A pause and a stop take the start lock once, after writing the row and
  before the walk, so a start admitted before the row was written has made
  its session by then, and one after sees the hold. A pause or stop by a
  caller who outranks the setter takes the hold over, even when it is less
  severe than the state it finds (which stays). `resume` is refused under a held ancestor, while
  the target's own subtree has used its budget, and to a caller ranked
  below the setter: the user, the assistant, a node above the caller, the
  caller, a node below it, then `budget`. The resume is written only if
  the target's run state and setter are still the ones checked
  (`Database::resume_run_state`), so a pause or stop made meanwhile, by
  someone who may outrank the caller, stays. It then hands what queued to
  each live, unheld node below that a delivery would type into (idle, or
  running with no hooks: `messages::route` is `Type`), reading the hold
  again under its inbox lock, then runs a drain, which starts each node
  a held message queued the start of whose agent is not running. A
  report to a node that is not running (`may_start` off) queues no start,
  held or not, so a resume never starts a node for reports alone. A stop ends every node's
  session in the subtree, live or not, and forgets its pane, through an
  end function and own-session check passed in as for a pause
  (`run::stop_with`; production: `run::end_agent`). It also ends the
  review jobs running as the node or in its directory, as an archive does
  (a review task's agent is its review command, with no session to end or
  interrupt), and `jeet review` refuses to start a held review task's job. `max_live` counts the
  active tasks below the assistant whose `jeet-<slug>` session one `tmux
  list-sessions` lists and whose pane is alive; a session the user's
  `remain-on-exit` keeps after its agent exits does not count. The start gate is a
  blocking lock beside the watch's: a held task is refused before any
  count, and one over the limit is queued, keeping its place. A queued
  task leaves the queue once its agent starts, however it was admitted
  (`ensure_running_admitted` unqueues on `Started::Now`), so one that
  fails to start stays queued, one the user opened past the gate is not
  started again by a drain after it ends, and a drain from a background job with no tmux
  server starts nothing. A budget check tells each threshold once, by
  `task_budgets.warned` alone: 80% as a `budget` status report to the
  parent and a note in the task's inbox; 100% pauses the task with the
  `budget` setter, which leaves anyone else's pause or stop as it is, and
  is reported to the parent for a task above the one charged (the charged
  node's own Stop hook reports it). A budget change that does not fit what
  is left above is refused, not cut.
- **P17** Every start of a task's agent but the assistant's is admitted by
  `runtime::ensure_running` before `runtime_for` (M22), unless the agent
  is running already: a live pane is attached to, held or not, and
  nothing is started from that path (one gone meanwhile is not restarted
  unadmitted). A held
  task is refused ("`jeet manager resume <holder>` first") for every
  caller. Over `max_live` a start is queued (`Started::Queued`, the
  prompt handed back) for whoever asked, except the user's `jeet task
  open` and the TUI's open, which start it anyway and warn "over
  max_live". A start with no prompt and no session named (`task open`,
  the TUI's open) takes the task's inbox lock before the start lock and
  starts it with what waits there, as a drain would
  (`runtime::start_with_inbox`): a brief queued on `max_live` is not left
  behind by an open that starts it first. Under a harness that takes no
  first prompt from jeet (`prompt_args` is `None`), it starts with none and
  leaves the inbox for a delivery to type in, rather than fail every open.
  A drain skips, unqueues and warns of a queued task whose directory is
  gone, before its inbox lock would make it again. A start made holding the start lock (a drain, or a delivery
  admitted already) goes through `ensure_running_admitted` with the
  guard, as the lock is not re-entrant. `Agent::start` takes that guard
  and says how the start went (`Started`); a delivery whose start is
  queued keeps the whole message queue in the inbox, in order. `task new`
  takes its inbox lock before the start lock, puts a queued brief in the
  inbox and reports `queued, n/max live` to the parent (`StatusEvent::
  Queued`); under a held parent it puts the brief in the inbox, queues the
  start for the resume's drain and succeeds, whether the hold is seen
  before the start or only by the start gate (a pause in between). A drain returns what a start
  warns of rather than print it (the TUI drains too). Unit tests never run tmux: under `cfg(test)`, `tmux::binary`
  names one that does not exist, whatever `$JEET_TMUX` says.
- **P18** A held task (5.4) is handed nothing, and `jeet task-startup`
  accepts no trust prompt for it (asked before each key), which would let
  a brief given before the pause run: the prompt is left for the user. `messages::deliver` asks
  `Agent::held` first, under the inbox lock, and queues for a held agent
  whatever it is doing (`Delivered::Paused`), queueing its start
  (`Agent::queue_held_start`) when the message may start it, whether or
  not its agent runs then (it may end while held; a drain that finds it
  running only unqueues it), so the drain a resume runs starts it with
  its queue if need be; only then, for a start, does
  it ask `Agent::admit`, which takes the start lock after the inbox lock
  and is `None` for an agent the gate does not count (the assistant's).
  Over `max_live` the message is queued with the start
  (`Delivered::StartQueued { live, max }`, as `Delivered::Queued` already
  means queued for a busy agent), and, once the inbox is unlocked, the
  task's parent is told `queued, n/max live` (`Agent::queued`) when the
  start was not queued already, so a run of messages tells it once. `jeet watch` notifies the user of a held
  delivery only when the agent could not be started (`Held`), not when it
  is paused or waits on `max_live`, and drains the start queue each poll.
- **P19** Budgets (5.3) are charged in the hook, before it acts on the
  event, by one helper (`status::hook_ctx`), which opens the index for
  every hook of a task, found by `--task` or by its directory. A status
  that is running or waiting keeps `busy_since`, when its busy spell
  began. An attributed `Stopped` charges a turn and the spell's time, an
  attributed `Ended`, or `Idle` after a spell no `Stopped` closed (Escape,
  a refused permission), the time left; then the budgets are checked, and only
  then is the run state read, so a turn that uses a budget up is held at
  its own end: no `continue_output`, its queue left in the inbox, and its
  parent told `blocked: budget` from the hook itself, with no `jeet`
  spawned. The Stop hook and `idle_prompt` read the hold again once they
  hold the inbox lock, before handing anything over, so a pause written
  in between, whose walk interrupts under that lock, holds them too. How far a session's time is charged is kept beside its status
  (`.<session>.charged`, under a lock), so a sweep (`jeet watch`, the
  assistant's turns), a pause's interrupt and a stop, which each charge a
  spell still going, never charge the same time as the hook that ends it.
  A sweep and a stop charge only an agent whose pane is alive: one that
  died with no `Ended` (a crash, a reboot) leaves a status that still says
  busy, which is no busy time, nor is it once a new agent is launched and
  has not reported yet (a status older than `.jeet/launched`). Archiving
  or deleting a task in the tree charges its live agent's spell so far
  before its session ends, as a stop does, since no hook or sweep charges
  it after. A sweep, and a pause's or `task interrupt`'s interrupt, that use a budget up report `blocked: budget` themselves; a stop's does not, as a stopped node's end is not reported. A status report is debounced against the last one of the same event (its word), so a `budget` or `queued` report between two sweeps of one idle spell does not make it news again. Deleting a task adds what it used to its parent's use, in the same transaction, so its spend still counts toward every budget above it. An
  `Ended` that no new session replaces starts a detached `jeet manager
  drain` (output to `drain.log` in jeet's home) when a start waits on
  `max_live`, as a hook has seconds to run; a stopped task's `Ended` is not
  reported to its parent. A harness with no hooks, or no `Ended`, has
  nothing drain when its agent exits: a queued start waits for the next
  drain from `run_once`, the TUI refresh, a stop or a resume (M22's list).
- **P20** `jeet manager budget <task> +…|=…` (`manager.grant`) raises
  a limit the task has (`+` on a unit with no limit is refused: there is
  no cap to raise) or sets one, keeping the unit not given; the result
  must fit what is left above the task and, for a node, what it has left
  itself, and the 80%/100% reports start over, then the budgets are
  checked at once (`budget::enforce`), so a limit set at or below what
  the task has used pauses it now, not after a turn past it. Otherwise it
  leaves the run state alone: a task its budget paused runs again only once resumed, and the
  command says so. There is no `BRIEF.md` to rewrite yet (2-iii). `jeet
  manager pause`, `resume` and `stop` (`manager.stop`) print one line per
  node the walk touched. `stop --all` is the user's alone, refused before
  any scope check to the assistant and to a node (M11); the assistant may
  still stop one task. `manager show` names a node that set a run state
  by its slug. The hidden `jeet manager drain` takes `--after <task id>`:
  the `Ended` hook runs before its agent exits, so the drain it leaves
  behind waits up to ten seconds for that task's pane to go before it
  counts, or the ending session would still fill the slot it frees. A
  drain returns what it warns of instead of printing it, so the TUI,
  which drains on a full refresh (opening, and `r`), shows the first in
  its status line rather than on the screen it draws.

## 11. Placement

A node's placement says where its agent runs. There is one today:

```rust
pub enum Placement { Local }   // tasks.placement
```

`local` is a tmux pane on the user's machine, running as the user,
cooperative and unconfined (5.7). Every node runs there, managers below the
root included. A new task gets `local` wherever it sits in the tree
(`placement::for_new`), and no node is refused for where it sits.

**The runtime seam.**

```rust
pub trait NodeRuntime {
    fn start(&self, app: &App, task: &TaskRecord, session: Option<&str>, prompt: Option<&str>) -> Result<Running>;
    fn interrupt(&self, app: &App, task: &TaskRecord) -> Result<Interrupted>;
}
pub fn runtime_for(task: &TaskRecord) -> Result<&'static dyn NodeRuntime>;
```

`runtime_for` returns `LocalTmux` for `local`. It is the one way a node is
started or interrupted: `runtime::ensure_running` and `runtime::interrupt`
dispatch through it, and so does typing into a live session
(`TmuxAgent::type_in`). An unknown placement cannot reach it: the
`tasks.placement` `CHECK` allows only `local`, and a new placement widens it
in a migration that rebuilds `tasks` (v6 rebuilds it anyway). That raises
the schema version, so this jeet refuses a later jeet's index as a whole
("upgrade jeet") before any task is read. Only a hand edit that bypasses the
`CHECK` stores one, and then reading the task fails, so nothing reaches tmux
for it; every read of the tasks table fails with it (listing, archive,
delete) until the row is fixed.

**Plugging in a confined placement later.** A local sandbox (sandvault is
a candidate) or a cloud runtime (§13) is a new `Placement` variant, a
`NodeRuntime` for it and an arm in `runtime_for`; nothing else routes around
the seam. A confined placement is what closes the open rows of 5.7. When the
first one lands, the rule decided and dropped in #33 (I22) can come back with
it: a child is never less confined than its parent.

Left off the trait, each for a reason:

- **`stop`**: archive and delete must always work, and `tmux::kill` is
  already a no-op when there is no session.
- **`send`**: `messages::Agent` is already the local abstraction. Typing into
  a live session goes through `runtime_for` too, so what starting a task
  would refuse, typing into it does too; a send that only queues still
  queues.
- **`activity`, `attach`, `collect`**: no caller until a second runtime.

**With a second runtime** (a sketch, not built), the trait gains `send`,
`stop`, `activity`, `attach` and, for a runtime that reports through a
mailbox, `collect`, and a `Support` struct says what each placement provides
(interrupt, stop, live status, turn count, usage, read-only worktrees), so
phase 2–4 code can degrade per placement rather than per runtime.

## 12. Message delivery through a watcher (phase 2a)

Today jeet types messages into an agent's pane. A watcher would instead let
a background process, started by a hook, hand messages to the agent.

| Goal | Meaning |
|---|---|
| G1: seen once | the user does not see a message twice, typed and then fenced |
| G2: any time | an agent can be messaged while busy, not only idle or at Stop |
| G3: no typing | jeet never types into an input box: no draft check, no risk of answering a trust or permission prompt |
| G4: no pane | the same path could serve nodes without a pane |

**Verdict: adopt partly.** An optional *wake channel* for local Claude Code
nodes, as PR 2a of phase 2, after a spike. It meets G1 and G3, and G2 at
tool-call boundaries. It needs no daemon and falls back to today's path
wherever it is missing. Do not:

- make it the transport: `.jeet/inbox.jsonl` stays the queue,
  `Inbox::lock` the one place a message is taken, and `task_reports` the
  record;
- require it of any harness;
- adopt a long-lived `asyncRewake` waiter: its timeout is enforced (600 s by
  default, 30 s on `UserPromptSubmit`), so an idle manager's waiter would
  expire.

**Candidates, in order of preference** (the spike picks one):

| | W1: hook-started poster | W2: the harness watches the inbox | W3: jeet posts directly |
|---|---|---|---|
| Started by | a `SessionStart` hook entry with `async: true`, in jeet's `--settings` | `SessionStart` returns `watchPaths: [<task>/.jeet/inbox.jsonl]`; a `FileChanged` hook with `asyncRewake: true` | nothing; `SessionStart` stores the socket path and token |
| Reaches the model | posts to `CLAUDE_CODE_MESSAGING_SOCKET` as the agent's own child, so no `crossSessionInbound` setting is needed | exit 2 with the messages on stderr, as a system reminder | `deliver` posts to the socket; needs `crossSessionInbound: "accept"` |
| Mid-turn | documented: the next tool-call boundary | unverified | documented |
| Main unknown | the message wire format; own-child status of a long-lived hook child | `FileChanged` with `asyncRewake`; the watch surviving `Inbox::replace` | the wire format; a token outside the session; repo `refuse` |

W1 is preferred: it wakes an idle session and lands mid-turn by documented
paths, needs no settings change, and its process dies with the agent. W2 is
the fallback; W3 is a last resort, as it keeps a session token in jeet (0600,
out of logs and the worklog, if chosen).

**The spike** runs on the user's machine with local Claude Code sessions, in
a scratch directory outside `~/.jeet` and on a test tmux server, and settles:
the message wire format; whether a long-lived hook child keeps own-child
status; how a refused post (`crossSessionInbound: "refuse"`, or a `--bare`
session) is detected; whether an `asyncRewake` lands mid-turn and its watch
survives a rename and a delete; that a wake never answers an open permission
prompt or AskUserQuestion; whether `--bg` sessions run hooks and wake; async
hook lifetime on `/exit`, tmux kill and `-p` teardown; whether a child
complies with a manager's command sent as a peer message; and throttling
under a burst.

**How W1 works.** `jeet inbox watch` takes a non-blocking lock on
`.jeet/wait/<session>.lock` (held: exit 0, so compaction does not start a
second). It finds its task from `--task` in argv, as hooks do (#33), and its
session from the hook's stdin. It writes nothing to stdout. On each inbox
change it takes this session's messages under `Inbox::lock`, posts them as
one fenced batch with a nonce, and notes the delivery in the worklog. It
exits when the harness pid it recorded is gone, when its status says
`Ended`, or when `run_state` is paused or stopped (paused: it takes
nothing).

**How it fits with today's path.**

- `deliver` computes the route as today; under `Inbox::lock`, a live watcher
  means queue and return `Delivered::Watched`, otherwise `Type`, `Queue` or
  `Hold` as before. The watcher checks the queue once more under the lock
  before it releases its own, closing the race with `deliver`.
- The Stop hand-over stays; Stop and the watcher take under the same lock,
  so each message is taken once. Idle typing is skipped while a watcher is
  live.
- Taken messages go to `.jeet/wait/<session>.inflight` with their nonce,
  cleared when the nonce appears in the transcript; a new watcher pushes
  stale ones back to the front of the queue.
- The watcher writes a `Running` status before it posts, since a wake may
  start a turn jeet does not otherwise see.
- The pane shows only a preview line, so the full text is read in Ctrl+O,
  the phase 3 `i` view and `jeet report ls`. Fencing is unchanged.

**Framing.** A node with a wake channel receives messages as peer messages
from another session, which cannot approve anything; typed messages and the
brief stay user turns. 5.5 must say which framing each direction gets.

**Harnesses without it** degrade: with hooks and continue-turn but no async
wake, today's queue, Stop hand-over and idle typing; with no hooks, typing.
§7 gains an optional row, "background wake (`wake_hook`)".

**Required safeguards.** A cap on wakes per session (for example 6 a
minute; the rest wait for Stop), designed with Claude Code's cap of 8 Stop
continuations in a row; a wake counts as a turn against 5.3 budgets; pause
means the watcher takes nothing; batching and nonces against Claude Code's
inbound throttling (50 queued, per-sender limits, identical repeats
dropped).

**Risks.** Claude Code only. A peer message "can't approve anything", which
suits reports going up but may let a child ignore a manager's command; W2's
system reminder is trusted more than user text, a larger injection risk. A
repo's `crossSessionInbound: "refuse"` or a `--bare` session turns it off, so
jeet must detect a failed post and fall back to typing. Async hook lifetime
on interactive exit or tmux kill is unverified; the pid check bounds an
orphan to one poll. In the assistant's shared inbox, a message with no
session wakes whichever watcher wins the lock. `@internal` fields
(`rewakeSummary`, `rewakeMessage`) are used for appearance only.

| Phase | What lands |
|---|---|
| Before 2a | the spike; no code |
| 2a (#34) | `Harness::wake_hook`, `HookSetup.wake`, the settings entry, `src/waiter.rs`, `Delivered::Watched` routing in `messages.rs`, the idle skip and ack in `status.rs`, hidden `jeet inbox watch --task`/`take`. It does not block phase 2 |
| 3 (#35) | the `i` view becomes required; rows show `wake: armed/none` and inbox depth |
| 4 (#36) | answers, copies up the path and watch rules to managers may arrive mid-turn; the wake cap and turn accounting must exist first |

§9 gains: "a per-agent watcher replacing typing: accepted as an optional
harness wake capability; the inbox, its lock and the Stop hand-over stay."

## 13. Future work: confined and cloud placements

**Not planned.** Nothing here is built or scheduled, no probes have been
run, and no dependency (no `hmac`/`sha2`) has been added for it. #33 first
decided that managers below the root must run behind a boundary, with Claude
Code cloud sessions as the first; the user then dropped that requirement
(I22). This section keeps what is needed to pick the idea up again. A local
sandbox placement (sandvault is a candidate) is the other route, and is
not designed beyond §11's seam; it would also need the ancestry walk to see
through the sandbox (macOS `proc_pidinfo` under Seatbelt, Linux pid
namespaces). A probe plan for the cloud runtime and the identity checks was
drafted in `docs/cloud-probes.md` on the #33 branch and deleted unrun; it is
in that branch's history.

### 13.1 What a cloud boundary would be

A boundary *around the user's machine*: a cloud node cannot reach
`index.db`, `config.toml`, tmux, other nodes' homes, the user's `gh` token
or the user's shell. Every jeet action it takes goes through one broker, the
local jeet process, which re-checks identity, scope and the effective
allow-list on every call. It is *not* a boundary between cloud nodes, and it
does not limit what a node does directly through git or GitHub: all nodes
push and comment as one GitHub identity, share one cloud environment's
egress and environment variables, and hold their jeet identity as a key in
their own conversation. Local agents stay cooperative (5.7), and if they can
read the cloud login they can drive cloud nodes: it protects the machine
from cloud nodes, not cloud nodes from local agents. Upward prompt injection
remains the main risk.

### 13.2 Running a node

- **Start.** Push the node's branch `jeet/<node-id>/base` from a store
  worktree, then run `claude --cloud "<brief>"` there with
  `CLAUDE_CONFIG_DIR=[cloud].config_dir`, and store the session id and URL.
  If the CLI prints no parseable session id without a terminal, fall back to
  a routine `/fire` per node tier. One repo per session, so a cloud node is
  single-repo.
- **Send.** `claude -p "<fenced msg>" --cloud <session-id> --output-format
  json`. The server queues it; it fails on archived sessions, which is how
  jeet notices archiving. This is the cloud node's wake channel (no watcher).
- **Brief.** The first prompt carries the node id, goal and done text, the
  mailbox protocol and the node key. jeet relies on no repo hooks there, as
  the node can edit them.
- **Shim.** An environment setup script embeds a small untrusted `jeet` shim
  (POSIX sh plus python3) that turns `jeet report …`, `jeet task new …` and
  the like into signed envelopes on the mailbox branch.
- **Message-driven managers.** Idle VMs are reclaimed and background
  commands die after about 30 minutes, so a manager ends each turn with its
  state in its conversation and branch, and wakes on the next result or
  report sent down. It never polls.

### 13.3 The mailbox: the callback up from a node

- A node writes one file per envelope (`<seq>.json`) to `jeet-mail/<node-id>`
  in its session's repo, pushed through the proxy: `{v, node_id,
  session_id, seq, ts, argv[], body}` plus `mac = HMAC-SHA256(K_node,
  canonical_json_without_mac)`. At most 8 KiB each and 32 per node per
  tick; an issue or PR comment is never a command.
- `collect()` runs `git fetch origin '+refs/heads/jeet-mail/*:refs/jeet-mail/*'`
  in the store (`core.hooksPath=/dev/null`, `core.fsmonitor=false`,
  `GIT_CONFIG_NOSYSTEM=1`) and reads new blobs with `git cat-file`: no HTTP
  client. It runs on the `jeet watch` tick, the TUI refresh thread every
  `poll_secs`, and `jeet cloud sync`, under `WatchLock`. Each envelope is
  claimed by a unique `(node_id, seq)` row before it runs.
- An envelope is accepted only if the MAC verifies, `node_id` matches the
  branch, `session_id` the bound session, `seq` is above the last seen, `ts`
  is within 15 minutes, and the branch history includes the last verified
  commit. `K_node` is 32 random bytes in `index.db`, sent only in the node's
  first message. Anything that reads the node's transcript can forge it, so
  rekeys carry a MAC under the current key, a node that loses its key is
  replaced, and held capabilities are never granted to cloud nodes.
- Commands run in-process through `authorize_as(Caller::Node(id), …)` and
  must name their target: `report`, `task new` under itself, `task send` to
  a descendant, `manager …` on a descendant (pause and stop mean fence-off),
  its own subtree's `tree`/`task ls`, `pr create` with a head in its own
  namespace. `pr comment|review|merge` are refused.
- Results go down batched per tick, `<jeet-result seq=N ok=…>` with a MAC.
  Parent messages and GitHub events are fenced, and a GitHub event goes down
  as a pointer only. A cloud `-p` message carries full user authority, so no
  event text may look like a result or a rekey.

### 13.4 Credentials

- **GitHub.** The Claude GitHub App on the attested repos only, not
  `/web-setup`. The proxy does not limit which branches a push updates, so
  no-bypass rulesets must protect default branches and `jeet-mail/**`; they
  bind the user too. Branch namespaces are enforced only on brokered
  actions. A repo is attested only when no secrets are reachable from
  `jeet/**` and `jeet-mail/**` workflows and there is no comment-triggered
  automation.
- **Anthropic account.** A dedicated claude.ai login with no connectors, as
  connectors reach every cloud session. Not available to Bedrock, Vertex,
  Foundry, ZDR or HIPAA users.
- **Secrets.** Environment variables are shared by every session in an
  environment, so the only per-node secret is `K_node`, in the conversation.
- **Config sketch:** `[cloud] claude`, `config_dir`, `repos`, `poll_secs`,
  `mail_prefix`, `branch_prefix`; `[managers] worker_placement`.

### 13.5 What a cloud placement could not provide

*Fence-off* means: jeet sets `run_state = 'stopped'|'paused'`, rejects all
of the node's envelopes, sends it nothing more, fences off its subtree the
same way, and notifies the user with each session URL to stop or archive on
claude.ai. Each gap degrades to "jeet stops brokering for the node and tells
the user".

| Requirement (§) | Cloud placement | How, or why not |
|---|---|---|
| Managers below the root (§1, ph2) | **Supported** | cloud session plus mailbox, message-driven |
| Effective allow-lists, intersection, scope, no sideways sends (5.1) | **Supported for jeet commands** | enforced at the local broker on every envelope |
| Same, for direct git/`gh` actions (5.6) | **Not supported per node** | one GitHub identity. Repo-wide no-bypass rulesets plus attestation only |
| Held capabilities (5.1) | **Supported by refusal** | never grantable to cloud nodes. Merge is root-only through `jeet pr merge` |
| Depth, fan-out (5.2) | **Supported** | every spawn goes through the broker, if nodes cannot create sessions themselves |
| `max_live` and queueing (5.2) | **Degraded** | jeet cannot see whether a session is running or idle |
| Time budget (5.3) | **Degraded** | wall clock since start, enforced by fence-off. The node is not paused |
| Turn budget (5.3), cost budget (#26) | **Not supported** | no turn events or usage reach jeet |
| Pause / resume (5.4) | **Degraded** | fence-off plus holding the down-queue. The current turn keeps running |
| Stop, `stop --all` (5.4) | **Degraded** | fence-off. Real stop or archive is UI-only |
| Harness interrupt (#32) | **Not supported** | no CLI or API |
| Message delivered mid-turn | **Not supported** | a cloud message queues until Claude reads it |
| Read-only worktrees (§1) | **Not supported** | the node has a writable clone and push access |
| `worktree add` mid-session, multi-repo node | **Not supported** | one repo per session, fixed at start. Use a child node |
| Recursive archive/delete (ph2) | **Degraded** | index bookkeeping only; sessions are archived by the user in the UI |
| Agents pane rollup, worklog, inbox (ph3) | **Degraded** | from reports, verified envelopes and jeet's sent log; no live status |
| Attach `⏎` (ph3) | **Degraded** | opens the claude.ai URL; `--teleport` gives a one-way local copy |
| Escalation, watch rules, reparent (ph4) | **Supported, with latency** | through the mailbox and `-p`, one poll tick a hop. Answers on claude.ai bypass jeet |
| Tree works while the Mac sleeps | **Not supported** | brokering, spawns, answers and budget checks stop |
| Non-Claude harnesses | **Not supported** | cloud runs Claude Code only |

**Managed Agents** would close most of these gaps (custom tools as a
synchronous, session-bound callback with no key in the conversation,
`user.interrupt`, per-session usage and repo tokens) but is another product:
another agent loop without Claude Code hooks, Console API billing, a
workspace and key, and an HTTP client in jeet. Rejected for a first cloud
runtime: a jeet remote MCP server (a daemon and a public tunnel), transcript
polling, Claude Code Projects (UI only), and self-hosted environments (Team
or Enterprise and a runner).

### 13.6 Questions to answer before building it

1. **The runtime.** Claude Code cloud sessions, with the gaps above, or
   Managed Agents?
2. **The account.** A dedicated claude.ai account with no connectors, and
   on which plan (Pro/Max: network secrets; Team/Enterprise: self-hosted
   environments with verifiable session identity)?
3. **MACs.** `hmac` and `sha2` as new dependencies, or `/usr/bin/openssl`
   by absolute path?
4. **GitHub.** The App limited to the nodes' repos, no-bypass rulesets that
   bind the user too, the automation attestation, and optionally a
   jeet-owned App whose required check keeps merges with the local root.
5. **Probes.** Permission to run live, billed probes of the CLI, the VM's
   credentials, reclaimed VMs, push limits, the GitHub actor, connectors,
   permission mode and the base image.
6. **Liveness.** Accept that a cloud tree stops brokering while the Mac
   sleeps, and that a round trip is one poll tick?
