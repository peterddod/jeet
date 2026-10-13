# Hierarchical agents

Status: **draft, for review**. Phase 1 (#31), #32, #33 and phase 2 (#34,
managers) are built; the notes after §10 say where they departed from this
text. Every node runs
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
the user's `[assistant] allow`. (As built, only what a manager started in
one tmux session needs moved; the assistant keeps its windows: P24.)

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
  todo.md                         # edited by hand; `jeet todo` stays the assistant's (P21)
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
phase-4 columns `needs_user`/`answer`/`answered_by`, read from phase 4 on,
H2). The
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
| **delete** | refused while it has children (active or archived), unless `--recursive` (same listing, leaves first) or `--reparent` (children move to the grandparent, each grant in the moved subtrees cut to what it may use there) |

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
meanwhile cannot slip past. `--recursive` takes the tree instead (P23);
`--reparent`, which deletes the manager alone and moves its children to its
parent, came with phase 4 (H21-H30).

## 3. Delegation

### Commands

```bash
# The assistant makes a manager for an area of work
jeet manager new backend \
  --goal "Ship the v2 auth API" \
  --done "PRs open for api#120 and web#88, CI green, summary in worklog" \
  --brief-file backend-brief.md \
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

# Or places it lower in its own subtree, under a manager below it named by slug
jeet task new api-docs --parent docs --prompt "…"

# Read the tree
jeet manager ls                  # the caller's subtree (the whole tree for the user)
jeet manager ls --json            # the same, each task with its parent and depth
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

**Phase 2 (as built).** `manager tree --json` is `manager ls --json` (P13).
`--parent` takes `assistant` or a manager's exact slug, never an `a/b` path;
a node may name itself or a manager below it (P8, P11). The parent
defaults to the assistant for the assistant, to itself for a manager, and,
for the user, to the assistant on `manager new` and to none on `task new`.
`--allow` unasked is `[managers] worker_default` or `manager_default`, not
none, and `--budget` unasked is `default_budget` only for a manager and a
manager's child: the assistant's and the user's workers get none (P11).
`manager new` needs `--goal` or `--brief-file` (its text follows the goal;
there is no `brief` column) and starts the manager at once, through the
start gate (P22). `manager show [--json]` prints the brief, the stored grant,
the effective list with what cut each named capability it lacks, the
budget's limits and use (a review job, with no hooks, is not counted), the
run state with who set it and which task holds it, the children with their
state and last report, and the open reports (P13). The brief is fenced as
`<jeet-brief>`, unable to close or open a fence, and not marked untrusted:
the child still acts on it (P21).

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
2. Otherwise the parent escalates: `jeet report escalate 42`. `to_id`
   moves to the grandparent, and the original row keeps its origin. There
   is no `--note`: an escalating manager that wants to add context sends
   its own `jeet report progress "re #42: …"` (H6).
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

Until the agents pane reads `needs_user` (the TUI half of phase 4, still to
come), the only *needs you* it shows (§6) is a prompt: one in the
assistant's own pane, or one rolled up from below.

**Phase 4 (as built, without the TUI).**
- An ask is a `question` or `blocked` report (`blocked` is answered and
  escalated like a question). It is *open* while it is unanswered and is
  the asker's last report other than jeet's own `status`: a task that
  reports again, asks again included, has moved on and the old ask closes.
  An answer closes it at once (H3), and so does archiving the asker.
- Only the holder (`to_id`) answers or escalates, and never once it has
  passed the ask to the user. The user may answer any open ask in the tree,
  even one a manager still holds. A worker holds nothing, and a node never
  answers sideways or above itself (H5, H7).
- The answer reaches the asker by today's delivery and may start it. A copy,
  the question (still untrusted, cut so the whole fits a message) then the
  answer, goes to each manager on the path the ask took, from the asker's
  parent up to and including its holder, but the answerer. Managers above a
  holder the user answered past never saw the question and get nothing
  (H10).
- `answered_by` is the answerer's task id, or `user` (§2); a deleted
  answerer shows as "a deleted task" (H9).
- The needs-you notification is sent whatever `[assistant] notify` says:
  that flag mutes the assistant's own `jeet notify` (H11).
- `jeet report ls` lists the open asks the caller holds; for the user, at
  their own terminal, every one in the tree, those with the user first
  (H12).
- An answer is kept even when its delivery fails, and is not sent again
  (H10).

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
  `git.push` (deferred, P7). `pr.merge` comes from #25 (below). `bash_allow` emits each
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

**Phase 2 (as built).** Time is busy time only, while the agent runs or
waits on a prompt (P19), not while its pane is merely alive: an idle
manager waiting on reports spends nothing. At 100% jeet pauses the node and
each ancestor whose own subtree is at or over its limit, not the subtree:
running descendants finish their turn and get nothing new (P16). A review
job runs no hooks, so nothing is charged against it.

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
- An answer is fenced as `<jeet-answer to="42" from="…"[ copy="true"]>`. It
  is the asker's own chain speaking, or the user, so it is not marked
  untrusted, but its body still cannot close the fence; `from` is `user` or
  the answerer's slug-based label, as jeet resolved it. An ask passed on is
  `<jeet-report … via="manager backend" untrusted="true">` (H9).
- No agent can forge one: `jeet task send` and `jeet task new --prompt` by
  anyone but the user at a terminal turn every `<jeet-` and `</jeet-`, in
  any case, into `‹jeet-` and `‹/jeet-` (`manager::fence::defang`, H14).
- "At a terminal" is `task adopt`'s bar, in one helper
  (`permissions::at_terminal`): no task found for the caller, no
  `JEET_TASK_ID`, not `JEET_ASSISTANT`, and no pane of jeet's own tmux
  sessions above it. A top-level task's agent is `Caller::User` too, and
  fails it (H8).
- `jeet watch` routes to the root, to the task a PR belongs to, and to a
  manager a rule names (`action = "manager:backend"`, phase 4). The slug
  is resolved as each poll sends, by slug alone (a manager writes names);
  with no active manager of that slug, the event goes to the assistant. The
  manager gets it fenced, under a line in jeet's words, and may be started
  for it through the start gate (H18-H20).
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
launching jeet, or by the user adopting an agent an older jeet launched
(`jeet task adopt`, below), never re-bound, and ignored after a reboot. `JEET_TASK_ID`
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
| A pre-created `jeet-<slug>` session | closed (not adopted; warning), unless the user adopts it at a terminal outside jeet's panes, shown its pane's process first |
| An agent adopting itself or another (`jeet task adopt`) | closed (user only, refused under any pane of jeet's sessions, typed confirmation at a terminal); open through the orphan, tmux and fake-terminal rows, which already run as the user |
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
#33 have no anchor: the user restarts them (`jeet task restart`, which
resumes their session) or adopts them (`jeet task adopt`); see the
restart and adopt notes in §10.

## 6. Agents pane (TUI)

**A third tab view in the tasks list (decided):** `⇥` cycles active → agents
→ archived. In the agents view, the assistant row moves from pinned to the
tree's root. The tab is called *agents*, which is the UI text that decision 1
allows.

```text
┌ jeet · agents ──────────────────────────────────────────────────────────────┐
│ live 5/8   needs you 1   / filter  ⇥ switch                                 │
└─────────────────────────────────────────────────────────────────────────────┘
 ✦ assistant           ◆ needs you (1)        3 children   inbox 2  todo 4
 ├─ ▾ backend          ◆ needs you (1)        2h10/6h      inbox 1
 │  ├─ api-docs        ● running – Edit       0h05/1h
 │  └─ fix-token-ref…  ◆ needs input – Bash   0h40/2h      api#131
 ├─ fix-login          ✓ done                              web#88
 └─ ▸ frontend         ⏸ paused (2 below)     3h50/4h      budget 96%
```

Siblings are in slug order, as `jeet manager ls` has them. The view holds
the assistant and its **active** descendants only: top-level tasks are in
*active*, archived ones in *archived*. A manager always shows `▾` or `▸`,
so its kind is visible; everything starts open and nothing is persisted.

**Rollup**: a manager shows the most urgent state in its subtree, in the
order needs you > needs input > blocked > budget > running > idle > done. A
count is shown when that state comes from below. A node's own prompt is
*needs input*; above it, it becomes *needs you*, as only the user can answer
one (§4). A question passed to the user is *needs you* on its asker too,
and above it (H16). The assistant's own urgency comes from its sessions, so
a prompt in its pane needs you too. A held node shows `⏸ paused (n below)` or
`■ stopped (n below)`, *n* the nodes it holds, and appends what rolled up
from below when that is blocked or worse: a pause answers no prompt. Rollup
is computed on the screen's thread from what agents are doing (status files,
read by the existing status poller) and from the index (`task_reports`,
budgets, run states, inboxes, PRs, the start queue and the live set), read
by a tree thread of its own every two seconds while the view is shown. There
is no cost column (#26) and no wake cell (§12).

| Key | |
|---|---|
| `⏎` | attach to the node's coding agent (the assistant picks a session, as `C`) |
| `→` / `←` | expand / collapse; on a node with nothing to open or close, to its first child / its parent; collapse is ignored while a filter is set |
| `l` | the worklog (`notes/worklog.md`) of the assistant or a manager, read-only, scrollable |
| `i` | the node's open ask, inbox and reports by and to it, newest first; `⏎` opens the node's PRs (a report carries no link, D18) |
| `a` | answer the open question at this node or nearest below it, one with the user first (§4, "Questions and the user"; H15), as `jeet report answer` does as the user |
| `b` | brief, grant, effective allow-list, budget, run state and its own open ask |
| `p` / `P` | pause / resume the subtree; refused on the assistant, as `jeet manager pause` refuses it |
| `x` | stop the subtree (asks first, listing what is running); on the assistant, every task under it (`stop --all`, P8) |
| `n` | new task or manager under the assistant or a manager (#21's form, plus kind, goal, done, budget, allow; a manager has no repositories). A manager starts at once; a worker is made but not started, as `n` in the active view starts none, and `⏎` starts it. On a worker `n` is refused |

The pane acts as the user: `l`, `i` and `b` (and the keys that change
something, `a` among them) refuse unless jeet runs at the user's own
terminal (H8, H17): not inside an agent's or a top-level task's pane, nor a
window of jeet's tmux sessions, nor when who runs it cannot be told. `c`,
`C` and `t` are the assistant's, as in the other views; the worktree, PR,
archive and delete keys are the active view's alone.

## 7. Harness neutrality

Managers run on any harness that implements the #23 trait. A harness that
lacks a **required** item refuses `jeet manager new` with the missing piece
named. As built there is no `--harness` flag: the configured harness is
checked, as the manager is made and at each start (P12). Workers keep #23's degrade-don't-fail rule.

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
  `manager::root()`. As built, the move is minimal (P24): a manager is a
  single-session node, so multi-session windows and the assistant's session
  machinery stay in `assistant/`.
- Touch points: new `src/manager/` (from `assistant/mod.rs`),
  `permissions.rs`, `config.rs` (`[managers]`), `tasks.rs`
  (archive/delete/unarchive cascade), `worktrees.rs` and `db/tasks.rs`
  (`read_only`), `harness/` (`interrupt`, deny rules),
  `startup.rs` (`auto_trust` covers manager homes, P5), `cli.rs`.

**Phase 3: the agents pane.** (#35)
- The agents tab, rollup, `l`/`i`/`b`, pause/stop keys, and `n` with
  parent fields. `a` moved to phase 4, which builds what it answers with.
- Touch points: `tui/tasks.rs` (with #21's new-task form),
  `tui/tasks_ui.rs`, a new `tui/managers.rs`, `tui/term.rs` (`blank`);
  `commands/task.rs` (`start`), `commands/manager.rs` (pub `shown`,
  `outcome_summary`); `manager/mod.rs` (`tree_order`, `open_ask`,
  `WORKLOG_MD`), `db/tasks.rs` (`reports_for`), `messages.rs` (`peek`,
  `try_peek`). As built: see the phase 3 implementation notes.

**Phase 4: escalation to the user and the rest.** (#36)
- `needs_user`, `jeet report answer|escalate`, answers from the user (with
  copies to each manager on the path), `jeet watch` rules targeting
  managers, and `--reparent`.
- `a` in the agents pane: answer the node's open ask, or the nearest one
  below it (moved from phase 3).
- Touch points: `db/tasks.rs`, `reports.rs`, `manager/{mod,fence,context}.rs`,
  `commands/{report,task,manager}.rs`, `permissions.rs`, `config.rs`,
  `watch.rs`, `tasks.rs`, `tui/{managers,tasks,tasks_ui}.rs` and the
  templates. Answers, copies and watch events use today's delivery (no
  phase 2a). As built so far: escalation and answers, with the agents
  pane's `a` and needs-you rollup; see the phase 4 implementation notes.
- Watch rules to a manager, as built: `config.rs` (`RuleAction::Manager`),
  `watch.rs` (dispatch) and the manager template (H18-H20).
- `--reparent`, as built: `db/tasks.rs` (`MoveRows`, `move_refusal_in`,
  `check_move`, `apply_move`), `tasks.rs` (`plan_move`, `plan_reparent`,
  `delete_reparenting`, `after_move`), `manager/mod.rs` (`write_briefs`,
  moved from `commands/manager.rs`), `reports.rs` (`StatusEvent::Moved`),
  `cli.rs`, `commands/task.rs` and `permissions.rs` (H21-H30).

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
- **E14** The manager refusal lands with phase 2's `manager new` (P12,
  P22) as `harness::manager_missing(&dyn Harness) -> Vec<&'static str>`,
  checked through `tasks::manager_harness`, naming "recorded sessions",
  "hook events", "continue-turn output", "interrupt_keys" and "deny rules"; it has no
  caller in #32.
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
  `report` and `worktree add`, and old hooks no longer report up
  (`jeet task restart` and `jeet task adopt` since, R1 and R2).
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

### Restart and adopt implementation notes

Bringing agents an older jeet started onto this one, which has no anchor
for them (I9):

- **R1** `jeet task restart <task>` ends the agent (`tmux kill-session`,
  which gives it `EXIT_GRACE` to exit, as a stop does), forgets its pane
  anchors and starts it again through `ensure_running` with the session
  `latest_session` picks, so it is resumed (or started afresh under the
  same id when its harness has no transcript of it) and anchored like any
  start. Its busy time is folded first, as a stop folds it. It is refused
  while the agent is mid-turn, waiting on someone, starting, or has unsent
  text in its prompt (`busy_reason`), unless `--force`; while the task is
  held (a resume would be needed to start it again); for a harness without
  sessions (the conversation would be lost); and inside the task's own
  session (`refuse_inside_session`). It needs `task.open` and reaches what
  `task open` reaches (`Verb::TaskOpen`). `restart assistant` does the same
  for each running assistant window through `assistant::end` and
  `assistant::open(Open::Session)`, user only, from outside its session;
  busy windows are left and the command fails. Background work the agent
  started (subagents, workflows, `/loop` schedules) ends with it, so adopt
  exists too.
- **R2** `jeet task adopt <task>` records the live process in the task's
  agent pane (`#{pane_pid}`, the process a launch would have recorded) as a
  `pane` anchor, on the user's word. It is sound against I1's rule that an
  agent never anchors itself because every way an agent could run it is
  already refused or already the user: the caller must resolve as the user
  and carry no `$JEET_TASK_ID`/`$JEET_ASSISTANT` (an unanchored agent with
  its hint is unconfirmed); it must not run under any live pane of jeet's
  tmux sessions, nor under the pane being adopted (`identity::adopt` walks
  the ancestry, which stripping the env does not change); it needs a
  terminal on stdin and stderr (an agent's tool calls have none), and the
  user types the task's name back after being shown the pane, its process
  and its program. It is `Gate::Interactive` (refused to the assistant
  without a terminal) and left out of `node_scope` (refused to every Node).
  What gets past these is an orphan with its env stripped, a process
  started through tmux outside jeet's sessions, or a fake terminal: the
  open rows of §5.7, which already run as the user and can do more than
  adopt. Adoption never re-binds: a task with a live pane anchor is not
  adopted again, and an assistant window only when its own process has
  none. A session pre-created under a task's name (I9's reason for no
  backfill) is adopted only if the user, shown its process, confirms it.
- **R3** An adopted agent keeps the hooks of the jeet that started it
  (from before `--task`, I11), so its hooks write status but do not report
  up until it is restarted; adopt says so.
- **R4** `runtime::unanchored` (a live agent pane with no live anchor at its
  pid) and `assistant::unanchored_windows` drive `jeet task ls`
  (`(unanchored)` after the status, the fix on stderr) and the tasks list
  (`unanchored ·` before the status, the fix on the status line for the
  selected row, a count on the assistant's line). The tasks list's poller
  threads open an index connection of their own for it, only when the index
  exists.

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
  worktrees and git denies (P4) and the trust scope (P5). Until it lands
  (P22) no manager row exists for a report or a `--parent` to reach (P11,
  P14).
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
  node's subtree, so a prefix cannot reach outside it; delete, `archive --recursive` and
  `--parent` take exact names. A command the table leaves out is refused
  to every node, reads included (`worktree rm`, `projects`, `sync`, …).
  `jeet clone` names no task, so a node holding `repo.clone` may run it;
  `worktree add` and `task new --repo` naming a repository the store lacks
  clone it only for a caller holding `repo.clone` (`permissions::may_clone`),
  so neither reaches the store around `clone`'s gate; the clone, made or
  refused, is logged (and the assistant's announced) as `jeet clone`'s is.
  Every route refuses a URL or id whose host, owner or repo has an empty,
  `.` or `..` segment, a control or invisible character, `<`, `>` or `\`
  (`remote::parse_remote_url`), so a clone stays under the store and its
  id prints as itself; and, since owners may nest, `clone_into_store`
  refuses a trunk inside or above an indexed repository's store trunk
  (compared case-folded), so a clone cannot plant `.cargo/` or the like in
  another checkout; a trunk registered in place before tasks (outside the
  store, maybe above it) is not compared, so it blocks no clone, its own
  `jeet import` included.
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
  a manager writes from what it has read. A task made under a parent has a
  name of at most 100 characters on one line with no `<` or `>` and no
  bidirectional, zero-width or other invisible format character, and the
  root's summary lists a task whose parent is neither the root nor none by
  its slug alone: a manager may name it from text it read. `jeet task ls`
  (NAME `-`) and `jeet task last --json` (`name` null) do the same for an
  agent, which sees names only of tasks with no parent, under the root, or
  under itself; `send`, `interrupt`, `archive`, `unarchive`, `delete`,
  `resync`, `prs`, `last` and `open`, and `jeet worktree ls`, `path`,
  `add` and `rm`, name any other by its slug in what they print, as do
  the start warnings and errors they pass on, `jeet review`'s, and every
  message that names the task holding a branch or the task whose
  workspace a command was run in. The root's summary, `jeet
  task ls` and `jeet task last` give such a task's status without its
  detail (the first line of its last prompt, which a manager wrote), as a
  manager's summary does for every task below its own children, and every
  task's status there unable to close a fence or open one.
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
- **P12** A manager needs a harness with recorded sessions (hooks, deny
  rules and the first prompt are installed only on one), hook events,
  continue-turn output, `interrupt_keys` and deny rules (§7), and is refused, naming what
  is missing, on one without, as it is made and again at each start
  (`runtime::start_tmux`), so one made before the configured harness
  changed is not started on one that lacks them. A worker starts without
  them; only a
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
  A node handed input in the last `STARTING_SECS` (launched with hooks
  and its brief on its command line, which `.jeet/launched` records, or
  typed a message since its launch: `TmuxAgent::type_in` touches
  `.jeet/typed` first; a launch with no first prompt hands it none, and
  its pause waits for nothing) that no hook has reported on since
  (`status::input_unreported`; a status in the same second does not
  count, nor does a session start's, written before the prompt on its
  command line is submitted however late it boots, and a startup-prompt
  status ends the wait) is about to start a
  turn its status does not show: while the interrupt finds it starting,
  idle or at startup, it is tried again every 250 ms, with the inbox lock
  let go in between, until a hook reports or the window passes.
  A pause and a stop take the start lock once, after writing the row and
  before the walk, so a start admitted before the row was written has made
  its session by then, and one after sees the hold. A parented review
  task's job is admitted the same way (`run::admit_job`, not counted
  against `max_live`), the lock held until its runner is recorded, so a
  stop meanwhile finds the job and ends it. A pause or stop by a
  caller who outranks the setter takes the hold over, even when it is less
  severe than the state it finds (which stays). `resume` is refused under a held ancestor, while
  the target's own subtree has used its budget, and to a caller ranked
  below the setter: the user, the assistant, a node above the caller, the
  caller, a node below it, then `budget`. The resume is written only if
  the target's run state and setter are still the ones checked, and its
  subtree is still within its budget (`Database::resume_run_state`), so a
  pause or stop made meanwhile, by someone who may outrank the caller,
  stays, and so does a budget's pause folded into a row a node holds
  already (the row is left as it was, so only the budget shows it). It then hands what queued to
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
  interrupt), and `jeet review` refuses to start a held review task's job, with a note,
  and still opens the review and hands off its worktree. `max_live` counts the
  active tasks below the assistant whose `jeet-<slug>` session one `tmux
  list-sessions` lists and whose agent's pane (the `@jeet-agent-pane` jeet
  notes when it starts the session; any pane, for a session with none
  noted) is alive; a session the user's `remain-on-exit`, or a window
  the user opened there, keeps after its agent exits does not count.
  Everywhere else a session with any live pane is still running, so a
  queued task whose session holds only such a window is left to it, not
  started. The start gate is a
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
  is reported to the parent for a task above the one charged, unless that
  task is stopped, itself or above, as a stop's own charge may use it up (the charged
  node's own Stop hook reports it, when the check newly holds it: its own
  budget used up, or one above while nothing held it already, so a node
  its own budget paused is not reported blocked again when its parent's
  follows). The row and its subtree's use are read,
  the pause written and the threshold claimed in one IMMEDIATE transaction
  (`Database::check_budget`), so a limit changed meanwhile is checked as it
  now is, and of two hooks crossing one threshold at once only one tells
  it. A budget change that does not fit what
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
  max_live". A start with no prompt (`task open`, the TUI's open, with a
  session named or not) takes the task's inbox lock before the start lock
  and starts it with what waits there, as a drain would, resuming the
  session named if any (`runtime::start_with_inbox`): a brief queued on `max_live` is not left
  behind by an open that starts it first. Under a harness that takes no
  first prompt from jeet (`prompt_args` is `None`), it starts with none and
  leaves the inbox for a delivery to type in, rather than fail every open.
  On such a harness a start with a prompt (`task new --prompt`, a delivery
  to an agent not running) is refused before it is held or queued
  (`runtime::first_prompt_refusal`), as it is when made at once, so no
  brief or message waits for a drain that would start it without one.
  A drain skips, unqueues and warns of a queued task whose directory is
  gone, before its inbox lock would make it again. A start made holding the start lock (a drain, or a delivery
  admitted already) goes through `ensure_running_admitted` with the
  guard, as the lock is not re-entrant. `Agent::start` takes that guard
  and says how the start went (`Started`); a delivery whose start is
  queued keeps the whole message queue in the inbox, in order. `task new`
  takes its inbox lock before the start lock, puts a queued brief in the
  inbox (`messages::brief` refuses one over `MAX_MESSAGE` once marked
  `Message: `, which the inbox would drop) and reports `queued, n/max live` to the parent (`StatusEvent::
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
  (`Agent::queue_held_start`) when the message may start it, and the
  harness takes a first prompt (on one that takes none, a start could not
  give it the message: a resume finding the agent running types it in, one
  finding it gone leaves it for a delivery, as `task open` does, and
  `task send` says so), whether or not its agent runs then (it may end while held; a drain that finds it
  running and not held only unqueues it, wherever it sits in the queue:
  once the tree is full a drain starts nothing more, but still goes on
  through the rows for such stale ones), so the drain a resume runs starts it with
  its queue if need be; only then, for a start, does
  it ask `Agent::admit`, which takes the start lock after the inbox lock
  and is `None` for an agent the gate does not count (the assistant's).
  Over `max_live` the message is queued with the start
  (`Delivered::StartQueued { live, max }`, as `Delivered::Queued` already
  means queued for a busy agent), and, once the inbox is unlocked, the
  task's parent is told `queued, n/max live` (`Agent::queued`) when the
  start was not queued already, so a run of messages tells it once. A
  start a hold queued (a held message or brief) says nothing of
  `max_live`; a drain that finds it waiting on a full tree tells the
  parent then, once per queued start (`queued` debounced against
  `queued_at`), and says so to its caller (a resume prints `task <slug> is
  queued on max_live (n/max live)`). `jeet watch` notifies the user of a held
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
  in between, whose walk interrupts under that lock, holds them too, and
  a resume written in between, which reads each agent's activity under
  that lock to hand over what queued, leaves it to them. How far a session's time is charged is kept beside its status
  (`.<session>.charged`, under a lock), so a sweep (`jeet watch`, the
  assistant's turns), a pause's interrupt and a stop, which each charge a
  spell still going, never charge the same time as the hook that ends it.
  A sweep, a stop, an archive and a delete charge only an agent whose own
  pane is alive, and an interrupt is sent to no other (so charges none) (`tmux::agent_alive`; a window the user opened keeps its
  session up but is no agent), and only a task in the tree (`fold_busy`
  charges no top-level task, as `task interrupt` would otherwise): one that
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
  itself, in each unit it raises or newly limits (one kept or lowered is
  not checked: siblings' spend may leave less above than a limit given
  before), and the 80%/100% reports start over, then the budgets are
  checked at once (`budget::enforce`), so a limit set at or below what
  the task has used pauses it now, not after a turn past it. Otherwise it
  leaves the run state alone: a task its budget paused runs again only once resumed, and the
  command says so. A manager's `BRIEF.md` is written again (P21). `jeet
  manager pause`, `resume` and `stop` (`manager.stop`) print one line per
  node the walk touched. `stop --all` is the user's alone, refused before
  any scope check to the assistant and to a node (M11); the assistant may
  still stop one task. `manager show` names a node that set a run state
  by its slug. The hidden `jeet manager drain` takes `--after <task id>`:
  the `Ended` hook runs before its agent exits, so the drain it leaves
  behind waits up to ten seconds for that task's agent's pane to go before it
  counts, or the ending session would still fill the slot it frees. A
  drain returns what it warns of instead of printing it, so the TUI,
  which drains on a full refresh (opening, and `r`), shows the first in
  its status line rather than on the screen it draws.
- **P21** A manager's role (§1 "Layout"): its home gets, in place of the
  worker's `jeet` skill (which says what a worker may do), the manager's
  `AGENTS.md` (read `BRIEF.md` first; the subtree only, no messages
  sideways; report with `jeet report`; the budget, pause and stop;
  read-only worktrees, so delegate code changes; jeet's limits are
  cooperative, stay within them; the untrusted-fence paragraphs), the
  `jeet-manager` skill, `notes/worklog.md` and `todo.md`, written once as
  the assistant's are. `jeet todo` stays the assistant's list (D6) and is
  refused to every task: a manager edits its `todo.md` by hand, its
  templates never name `jeet todo`, its settings do not pre-approve it,
  and its summary reads the file as `jeet todo` would parse it. Its
  `BRIEF.md` is its brief as its session context has it
  (`reports::brief_context`, fenced as `<jeet-brief>` with its tree path),
  plus its budget, which a worker's brief leaves to its manager. jeet
  writes it, mode 0444, when the manager is made, once its row is in (a
  failure is a warning: the session context has the brief too), and again
  on `manager grant` (for each active manager in the subtree, whose `You
  may` the grant cuts) and `manager budget`, and as each of its sessions
  starts (its SessionStart hook, before its summary; a failure is a
  warning), so a change made while it was archived, or to `[assistant]
  allow` in config, is in it before it reads it. Each write makes a new
  read-only file beside it and renames it over the old one rather than
  editing it in place, so an agent never reads half of one and a link put
  in its place is replaced, not followed; each reads the brief and
  renames under the task's `.jeet/brief.lock`, so a write that read the
  brief before a grant or budget changed cannot land over a newer one. A
  manager's `SessionStarted`
  (the hook path a worker's brief takes, switching on its kind) prints
  its summary (`manager::context::session_context`): its brief, the
  active tasks below it with their status and run state, the last
  reports of those under it, its budget and what its subtree used, its
  todo list and its inbox, which it hands over unless the manager is
  held. Each agent-written part is fenced. The assistant's summary gains
  one line, pointing to a new `jeet-managers` skill, a path of its own so
  existing homes get it (the write-once templates reach them no other
  way); it is otherwise unchanged to the byte.
- **P22** `jeet manager new <name>` (`manager.create`) is planned and
  made as a child (P11) of kind manager, under the assistant for the user
  and the assistant, under itself for a manager, or under `--parent`; a
  node's `--parent` has `task new`'s scope row (itself or a manager below
  it). `--max-children` (at least 1, at most `[managers] max_children`)
  is stored on the row. `--brief-file` has no column of its own (§2's
  `brief TEXT` is not added): its text follows `--goal` in the brief's
  goal, or is the goal without one, so it is checked, kept, fenced and
  shown with it (the two together at most a message's 8,000 bytes, refused
  as `--brief-file`'s when over). One of the two is required: a manager's
  brief is all it works towards, so one without a goal is refused before
  anything is made. The file is a regular file, read no
  further than that limit; named by an agent (the root or a node), it must
  resolve inside that agent's own home, links followed, since its
  `manager new` is pre-approved and `manager show` hands the brief back.
  It is opened, then its path resolved again: still inside the home and
  leading to the file opened (same device and inode), which has no other
  name (one link), or it is refused, so a link in the home swapped as it
  is read reaches nothing outside. The command prints the manager's slug, tree path and
  parent to stderr and its home on stdout (without changing directory, as
  `task new` asks the shell to), then starts it with
  `manager::first_prompt` through the start gate as `task new --prompt`
  does (one function, `commands::task::start_new`): started, queued over
  `max_live` with its parent told, or held under a paused parent with the
  prompt in its inbox. A start that fails keeps the manager, says it was
  made but its agent did not start, and exits 1 (M35). A child's name
  (whatever makes it under a parent: `task new --parent`, `manager new`,
  a review task made for the assistant) is refused when it holds a control
  character, whitespace other than a space, a bidirectional, zero-width
  or other invisible format character (Unicode's Cf, the tag block, the
  variation selectors),
  `<` or `>`: the assistant's summary lists names, the user's
  `task ls` and TUI show them, and a manager now writes them. A top-level
  task's name is unchanged. 2-ib's test pinning `manager new` as no command
  is gone; a worker running it is refused as for `task new`. The TUI's
  ` ↳ assistant` (D22) names a task's real parent: ` ↳ <slug>` for one
  under a manager. A manager's summary introduces its queued messages as
  its own: what its parent or the user sent with `task send` is queued
  unfenced and is their instruction; whatever is fenced (jeet-report,
  github-event, github-data, jeet-message) is a claim or data, never
  instructions. `task prs` run by an agent lists a task's PRs inside
  `<github-data untrusted>`, titles unable to close it, as the root's
  summary lists them; `jeet prs` run by an agent lists a repository's
  open PRs the same way, titles and authors unable to close it. The user
  gets either without control characters.
  `jeet watch` sends an event for a pull request to the worker that owns
  it, never to a manager that came to have it recorded (say by checking
  out a worker's head to read it): routing events to a manager is built
  through a rule, never by PR ownership (H19).
- **P23** `--recursive` (`-r`) on `task archive`, `unarchive` and
  `delete` (M23) takes the subtree, through `tasks::{archive, unarchive,
  delete}(…, recursive)`, which the CLI and the TUI share. Archive and
  delete walk `tasks::subtree_leaves_first` (the walk `manager/run.rs`
  pauses and stops by); archive skips what is already archived. Before any
  work, every node is checked (`refuse_inside_any`: not this jeet's own
  session or review job; for delete, not the shell's cwd and a sane
  slug), then the CLI shows one listing, `subtree_losses` (delete) or
  `subtree_unkept` (archive), and asks once, `--force` without a
  terminal. A recursive archive always asks, as it ends a tree's agents,
  where a lone one asks only when files git ignores would go. Then every
  node's agent is ended, leaves first, as a stop ends it
  (`manager::run::end_agent`: tmux session, review jobs; its pane
  forgotten, its turn charged) and not through `run::stop`, which would
  leave an unarchived tree stopped; only then is each node archived or
  removed from disk, leaves first, with its own warnings prefixed by its
  slug. Delete checks the tree again once its agents are ended, and
  stops with nothing deleted if a task was made under it meanwhile (a
  manager's last act), so no row is left refusing to go after its disk
  went; it removes the rows last, in one transaction
  (`delete_rows_leaves_first`), each node's spend folded into its parent
  first. Unarchive runs top-down, each node under a parent already back;
  each is held to its parent's `max_children`, so a tree that would
  exceed it stops there. A failure part-way
  leaves what is done done; running the command again finishes the job.
  The TUI's `a` and `D` on a task with tasks under it are recursive, one
  confirm listing each task leaves first, and the confirm records which
  it was, so a child made after a lone task's confirm is refused, not
  swept up; `a` in the archived view brings the tree back. The log line
  of a recursive teardown says `and every task under it`.
- **P24** The M numbers cited above are the phase 2 plan's decisions.
  Where each is recorded: M0 I22 and §11; M1 here; M2 P11; M3 5.7; M4
  (`Caller` stays user, root or node: a manager is a node, told apart by
  its kind); M5 P7; M6 §2; M7-M8 P7; M9 P6, P9; M10-M11 P8; M12 §3, P11;
  M13-M14 P11; M15 P16, here; M16-M18 P16-P18; M19 5.3, P19; M20 P11,
  here; M21 5.3, P16; M22 P16-P18, here; M23 P2, P23; M24 P4; M25 here;
  M26-M27 P21; M28 5.7, P5; M29 P13, P15, here; M30 P16, here; M31 P13;
  M32 P6; M33 the README; M34 P3; M35 P22; M36 P12; M37 P10; M38 P18;
  M39 P21. What the notes above leave out:
  - M1: there is no boundary rule. `placement::for_new()` gives `local`
    for every kind and depth; a `[managers] placement` key left in
    config.toml from an earlier design is ignored, not an error (a test
    pins it).
  - M15: the assistant, which has no budget of its own, may raise a
    budget after an automatic 100% pause (within what is left above the
    task) and then resume it, since `budget` ranks lowest among setters.
    This is intended. `manager budget` never changes the run state, so it
    needs no rank check; `resume` is the only way back to `run`.
  - M20: `default_budget` goes to a manager and a manager's child only,
    not to every child as §3 had it, so the assistant's workers, the
    user's and tasks made before phase 2 keep running with no budget.
  - M22, a change in behaviour: `max_live` counts every running task under
    the assistant, the assistant's own workers included (top-level tasks
    are not counted). At the default 8, the assistant's ninth running
    worker is queued where it used to start. The user's own open starts it
    anyway, with a warning.
  - M25: the move into `src/manager/` is minimal, not §1's and §8's
    "generalise `session_context`, multi-session windows". A manager is
    started by its parent into one `jeet-<slug>` session and typed into
    through `TmuxAgent`, as a worker is; it needs none of the windows that
    let the user talk to the assistant in several sessions. So
    `src/manager/` holds the templates by role, `ensure_files`, the
    fences, the inbox take, `context_text` by role, `write_brief`,
    `tree_path`, `first_prompt` and `root()`, while `assistant/` keeps
    its windows, `SessionAgent` and session machinery, re-exporting what
    moved. The assistant's output is unchanged to the byte, and a
    manager's session context comes through the `kind` switch in the
    SessionStart hook (P21).
  - M29: the commands are `jeet manager …`, not #34's `jeet agent …`, by
    decision 1 (the code says manager; UI text may say agent).
  - M30: tests use real rows, never a placement fake. Unit tests insert
    rows of any kind; the pause and stop walks take the runtime lookup as
    a parameter (P16). Integration tests make managers with `jeet manager
    new` and their children from the manager's own pane, against a stub
    tmux with several sessions and a temporary `JEET_HOME`; raw SQL is
    left only for states the CLI refuses to make.

### Phase 3 implementation notes

Where phase 3 (#35, the agents pane) settled what this RFC left open, or
departed from it. An *A* number records the phase 3 plan's decision of the
same *Q* number. It landed in two PRs: the read-only pane, then the
actions (`p`, `P`, `x`, `n`), whose notes are A15-A17 and A20.

- **A1** Two stacked PRs, for review size alone: neither widens what an
  agent can do, and every action calls the library functions the CLI
  calls, with its checks.
- **A2** The pane acts as the user. `tui::tasks::run` asks
  `permissions::caller` once, and the agents view's keys that show an
  agent's text (`l`, `i`, `b`) work only for `Caller::User`; an identity
  error counts as not the user. So do the actions (`p`, `P`, `x`, `n`),
  which pass `Caller::User` to the library. Anything else gets "the agents pane
  acts as you; open jeet from your own terminal, not from an agent's pane"
  (phase 4 made the check and the words stricter: H17). The
  tree, `⏎` and moving about stay open, as the active view is. Known gap,
  left as it is: the other views' `a` and `D` still run with no caller.
- **A3** A third view between *active* and *archived*: `⇥` cycles active →
  agents → archived, `⇧⇥` goes back. The title is `jeet · agents`, and the
  assistant's pinned line is not drawn: it is the root. The tab counts the
  tree's nodes; the list's title says how many of them a closed manager or
  the filter leaves shown.
- **A4** One ordering for `manager ls` and the pane:
  `manager::tree_order`, depth first, siblings by slug.
- **A5** `→` opens a closed manager, else goes to the first line under the
  node; `←` closes an open manager, else goes to the parent. A closed node
  still rolls its subtree up. A typed filter keeps the matches and their
  ancestors, whatever is closed; it matches the slug each row shows
  ("assistant" for the root). Files the pane reads from an agent's home
  (worklog, inbox) are opened without waiting or following links, and only
  if regular.
- **A6** Rollup reads what agents do from the status poller
  (`TaskRow.activity`) and the assistant's thread, never again per node, so
  the views cannot disagree; the index facts come from a tree thread
  (`tui/managers.rs::gather`) on a connection of its own, started on
  entering the view and stopped on leaving it, gathering when asked and
  every two seconds. It never makes the assistant. Each ask carries a
  sequence number and older answers are dropped. A failed gather keeps the
  last read and says so once (the index has no `busy_timeout`). An inbox is
  counted with a non-blocking lock; while a hook holds it the last count
  stands. The rollup itself (`managers::retree`) is pure and runs on the
  screen's thread, so the draw reads no index and no tmux.
- **A7** A node's own urgency: a prompt is *needs input*; else an open ask
  (its last own report blocked or a question, `manager::open_ask`) is
  *blocked*; else a budget at 80% or a budget's pause is *budget*; else a
  running agent is *running*; else a last own report of done is *done*;
  else *idle*. A *needs input* lifts to *needs you* above it, with the count
  of descendants at the shown state when it comes from below. Before phase
  4 the only *needs you* is a prompt. The assistant's own state is its
  sessions': one waiting is *needs input*, one running *running*. A held
  node shows its hold (A9) and appends what rolled up when that is blocked
  or worse; a node held by an ancestor shows `⏸ paused` or `■ stopped` with
  no count.
- **A8** The header: `live n/max` (the live set against `max_live`, `live
  n` when the policy has an error, nothing before the first gather), `needs
  you n` (each prompt once: the nodes' own and the assistant's sessions')
  and `queued n` (starts waiting on `max_live`), the last two only when not
  0. No wake cell.
- **A9** The columns: the tree's rails, `▾`/`▸` for a manager and the
  slug (jeet's, never a name an agent wrote), `✦ assistant` at the root;
  the status, glyph and words with the agent's detail (`● running – Edit`,
  `◆ needs input – Bash`, `! question`, `$ budget`, `✓ done`); the budget,
  `0h40/2h` against a time limit, else `12/40 turns`, and for the
  assistant `n children`; the notes `inbox n`, `todo n` (the assistant),
  the newest open PR as `repo#n`, `budget n%` from 80% and `queued`,
  dropped from the right as room runs out. Narrower, the notes go first
  (under 80 columns), then the budget (under 64), then the status's detail
  is cut. A node made since the last gather draws its activity, with no
  budget or notes.
- **A10** Nothing is fenced: the pane is the user. Every string an agent
  wrote goes through `printable` (a line) or `printable_text` (a text), and
  a text panel drops control characters again and expands tabs, which
  ratatui would draw with no width.
- **A11** `⏎` on a node is the active view's open: a held node's live pane
  is attached, one not live is refused (P17), `max_live` is passed with its
  warning (M22). On the assistant it is `C`'s session picker.
- **A12** `l` reads the last 256 KiB of the assistant's or a manager's
  `notes/worklog.md` (`manager::WORKLOG_MD`), decoded lossily and from the
  first whole line, into a scrollable panel (`↑↓`, `PgUp`/`PgDn`, `g`/`G`;
  the draw keeps the scroll within the text). A worker "keeps no
  worklog"; a missing file is "no worklog yet", on the status line.
- **A13** `i` shows the node's open ask with its id and whom it asks, what
  is queued in its inbox (`messages::peek`, which waits on the lock: it is
  a key press, under the spinner), and the last 50 reports by and to it,
  newest first (`Database::reports_for`). On the assistant it lists its
  children's reports to it. `⏎` opens the node's PRs when it has some: a
  report carries no link (D18), so there is no "linked PR".
- **A14** `b` is `jeet manager show`'s text (`commands::manager::shown`
  and `shown_text`) as the user, unfenced, with the node's own open ask
  after it. On the assistant too: it is a read for the user, so
  `manager show`'s scope, which refuses the assistant, is not asked.
- **A15** `p` and `P` pause and resume the node's subtree as `jeet manager
  pause` and `resume` do: `permissions::in_scope` as the user (the CLI's
  `scoped`, since `run::*` checks no scope), then `run::pause` or
  `run::resume` (which asks `check_resume`), on the node as the index has it
  then, under the spinner. Neither asks first: both can be undone. Each
  node's outcome goes to a message titled `paused manager api` (or `kept …
  stopped`, `resumed …`), after the CLI's own summary
  (`commands::manager::outcome_summary`). On the assistant, `in_scope`'s
  refusal is shown.
- **A16** `x` asks first. The confirm lists the nodes that run, leaves
  first, from the tree's last live set (before the first gather, the status
  poller's), and says only the user resumes them; `y` runs `in_scope` and
  `run::stop`, `n` changes nothing. There is no force. On the assistant,
  `x` is `stop --all` (`run::stop` with no task, the user's alone, P8),
  listing every active task under it.
- **A17** `n` on the assistant or a manager opens the new-task form with
  the node as parent and more fields: kind (task or manager, `space`),
  name, goal, done, budget (`2h,40turns`) and allow (comma-separated); a
  task also has project, repositories and branch, a manager none (`manager
  new` takes none). The form parses the budget (`budget::parse_part`) and
  the capabilities (`permissions::parse_capability`) as clap would, then
  `⏎` runs what `jeet task new --parent` and `jeet manager new` run, as the
  user: `tasks::plan_child`, then `tasks::create`. `--parent` is
  `assistant` for the root and the node's id below it (`find_exact` never
  finds the assistant, even by id). What `plan_child` refuses (a manager
  with no goal, a capability its parent does not hold, a full fan-out)
  stays on the form, which stays up. A manager starts at once
  (`commands::task::start` with `manager::first_prompt`, M35), and how
  (started, queued on `max_live`, held) goes to the status line; a worker
  is not started, as `n` in the active view starts none, and `⏎` starts it.
  Warnings (the create's and the start's) go to a message. On a worker `n`
  is refused at the key, in `plan_child`'s words. `max_children` is not
  offered: it is the config default. In the active view `n` is unchanged.
- **A18** `a` in the agents view does not archive there. Until phase 4 it
  said "answering comes with #36"; it now answers (H15).
- **A19** Nothing about wake: no cell, field or read of watcher state.
- **A20** No capture of stderr. The actions return what they did as data
  (`NodeOutcome` lines, `drain`'s warnings, the create's warnings and the
  start's `Start`), shown in a message or on the status line; after
  each the screen is blanked and painted whole (`term::blank`), so a line
  a deep error path wrote (a budget charge or a report that failed) cannot
  stay on screen. Its text is lost to the TUI: a known gap, shared with the
  other views, whose `r` and `⏎` reach the same paths. After each action the
  rows are read again and the tree thread asked for a new gather, whose
  sequence number drops any older answer.
- **A21** `?` and the hints follow the view (`HELP_AGENTS`,
  `HINTS_AGENTS`).
- **A22** The agents view's keys are its own: moving, `/`, `?`, `r`, `q`,
  `⇥`, `c`, `C` and `t` as everywhere; `⏎`, `→`, `←`, `l`, `i`, `b`, `p`,
  `P`, `x`, `n` and `a` as above; `e`, `w`, `v`, `o`, `D` and `S` say "not
  in the agents view; ⇥ to active".
- **A23** This section, §4, §6, §8 and §12 were brought in line with the
  pane as built, and the README has an agents-tab section.

### Phase 4 implementation notes

Where phase 4 (#36) settled what this RFC left open, or departed from it.
An *H* number records the phase 4 plan's decision of the same *R* number
(*E* and *R* were taken). Escalation and answers came first, without the
TUI; H15-H17 (`a` and the needs-you rollup) came with the TUI's part.

- **H1** Phase 4 lands in parts, for review size: escalation and answers
  (library, CLI, what agents read); then the agents pane's `a` and
  needs-you rollup; watch rules to a manager; `delete --reparent`.
- **H2** No migration: v4's `needs_user`, `answer`, `answered_by` and the
  `task_reports_to(to_id, answer)` index suffice. Escalation moves
  `to_id`; an answer is one guarded `UPDATE`; the path an ask took is the
  asker's ancestors up to `to_id`. No `answered_at`, no trail, no new
  kind.
- **H3** One meaning of open (`OPEN_ASK` in `db/tasks.rs`, behind
  `open_asks` and `manager::open_ask`): a `question` or `blocked`, with no
  answer, that is the asker's last report other than `status`. So an
  answer now closes an ask for A7, A13, A14 and `manager show`, and a
  `progress` report withdraws one, even one with the user.
- **H4** A question reaches the user only by escalation: there is no
  `--to-user`, since each manager above may be able to answer within its
  brief. When the assistant passes one up, `needs_user` becomes 1 and
  `to_id` stays the assistant.
- **H5** `jeet report escalate <id>`: the caller must hold the ask, open,
  not yet with the user. A manager's goes to its parent, fenced as
  `fence_escalated(.., via = "manager <slug>")`, which a question or
  blocked may wake (P14). The assistant's sets `needs_user` and notifies.
  A worker gets "you hold no question #42"; the user "you are the top;
  answer #42 …". Check and write are one IMMEDIATE transaction, so of two
  escalations, or an escalation and an answer, one wins; the loser gets
  "#42 changed meanwhile; it is no longer yours to act on".
- **H6** No `--note` on escalate: no column would keep it, and a note sent
  only with the delivery would never reach the user. Context goes in the
  escalator's own `progress` report.
- **H7** The user answers any open ask in the tree. The assistant and a
  manager answer only what they hold and have not passed to the user. Both
  commands refuse in jeet's words: no such report, not an ask ("#41 is a
  progress report"), answered ("#42 was answered by the user"), superseded
  ("task w has reported since (#57); #42 is no longer open"), held by
  another ("#42 is held by manager backend"), with the user, and an
  archived asker ("unarchive it first").
- **H8** Acting as the user (answering, or listing every open ask) needs
  `permissions::at_terminal`, the bar `task adopt` set and now shares: no
  task identified, `JEET_TASK_ID` unset or blank, not assistant mode, and
  no pane of jeet's tmux sessions above the process (so a window an agent
  opens in its own `jeet-<slug>` session with the env cleared fails). No
  stdin-is-a-terminal check. An identity error fails closed. Otherwise:
  "answering as you runs at your own terminal, outside jeet's tmux
  sessions and any task's pane". An orphaned, env-stripped process still
  passes, as 5.7 records.
- **H9** `fence_answer` and `fence_escalated` in `manager/fence.rs`. An
  answer is not marked untrusted; its body goes through `fence_body`.
  `from` is `user` or `manager::label` of the answerer (a slug, never a
  name). `answered_by` stores the answerer's task id or `user`; every
  display resolves it, or says "a deleted task" (it has no FK).
- **H10** The answer row is written first, then delivered to the asker
  (`messages::deliver` with an agent that may start, through the start
  gate and `max_live`; the assistant through its sessions), then the
  copies, never started (`may_start = false`; the assistant's through
  `assistant::deliver(.., false)`). Copies go to the asker's ancestors from
  its parent up to and including the holder, less the answerer. A copy is
  the question, fenced as `fence_escalated(.., via = "jeet (copy)")`, then
  `fence_answer(.., copy = true)`. Fencing triples each `<`/`>`, so a long
  question and a long answer could exceed `MAX_MESSAGE`; the answer is kept
  whole and the question cut to fit, ending "… (cut; #42 is kept in
  full)". An answer is cleaned, trimmed, non-empty and at most
  `MAX_REPORT` bytes. A failed delivery leaves the row answered: the
  command says so and exits 0, and nothing re-sends it.
- **H11** When `needs_user` becomes 1, one desktop notification: title
  `jeet: task w asks you (#42)`, subtitle the asker's tree path, body the
  question as written (not passed through `printable` first: the notifier
  already drops control characters and cuts it at `BODY_MAX`), no url.
  It ignores `[assistant] notify`. A question that reaches only the assistant
  notifies nobody. The notifier and the delivery are parameters of
  `reports::escalate`/`answer`'s cores, so no unit test reaches tmux or a
  real notifier; end to end it is tested with `JEET_NOTIFY_CMD` pointed at
  a recording stub.
- **H12** `jeet report ls`. The user, at a terminal (H8), sees every open
  ask, those with the user first, marked `with you`, bodies made printable;
  a top-level task's agent is refused, or it would read every question
  unfenced. The assistant or a manager sees what it holds, its `needs_user`
  ones marked `with the user`, bodies fenced as `<jeet-report>`. A worker
  sees its own open ask. A line is `#42 question task assistant/api/w 12m
  ago[, with you]`, then the question. No `--json`. `jeet report` became a
  subcommand; `jeet report <kind> "…"` parses and prints as before.
- **H13** What agents read. The assistant's and each manager's summary have
  "## Questions you hold" (fenced in `<jeet-reports untrusted>`, with how
  to answer and escalate), and nothing when they hold none, so the summary
  is otherwise unchanged. `manager show`'s `open` is what the node holds,
  however deep the asker, marked `escalated from <path>` and `with the
  user`. Every parented node's brief says what a `<jeet-answer>` is, and a
  manager's how to answer and escalate: templates reach new homes only
  (`ensure_files` never replaces one), so the brief, read at every
  SessionStart, carries the meaning. The brief's body goes through
  `fence_body`, which would turn a spelled-out tag into `‹…›`, so those
  lines name the fence in words ("fenced in jeet-answer, to= its id") and
  not as `<jeet-answer …>`. The inbox texts explain a queued answer, and
  only when one is queued. The TUI's `i` marks each report `answered by …` or `with the
  user`, with the answer under it. The templates and the `jeet`,
  `jeet-manager` and `jeet-managers` skills say how to answer, escalate
  and act on an answer.
- **H14** `jeet task send` and `jeet task new --prompt` put a sender's raw
  text before an agent, so for anyone but the user at a terminal (H8) both
  `defang` it: `<jeet-` and `</jeet-`, in any ASCII case, become `‹jeet-`
  and `‹/jeet-`. Control characters are dropped first, as delivery drops
  them, so `<\x01jeet-` cannot hide a tag, and invisible format
  characters (a zero-width space, say) are looked past, though kept. The
  user's own commands and the TUI's `n` form are unchanged.
- **H15** `a` in the agents view answers the open ask at the node or
  nearest below it, as the tree thread last read them
  (`managers::nearest_ask`, pure): among the asks with the user, the
  node's own, else the shallowest, ties in tree order; with none, the
  same over every open ask. So `a` on the root after a notification opens
  the question the user was notified about, not a nearer one a manager
  still holds (§6 said only "nearest"). It then reads the asker's open ask
  from the index under the spinner (`manager::open_ask`, rather than the
  report by id, so one asked since in its place is the one shown; with
  none, "that question was answered or withdrawn meanwhile"), and opens a
  form with the id, kind, the asker's tree path, `with you` when it is, and the
  question (`printable_text`), with a one-line input. `⏎` calls
  `reports::answer` with `Caller::User`, as `jeet report answer` does at
  the user's terminal, and shows how the answer and each copy went in a
  message; then the tree thread gathers again. An empty answer says "type
  an answer"; a refusal (answered or withdrawn meanwhile) stays on the
  form; `esc` cancels. With nothing open: "no open question at or below
  <slug>". The other views' `a` (archive, unarchive) is unchanged.
- **H16** `NodeInfo.ask` carries `needs_user`. An ask with the user is
  *needs you* on its asker, before the prompt check, since *needs you*
  outranks *needs input*, and the rollup carries it up as before. Its
  status reads `◆ needs you – question` (or `– blocked`). The header's
  `needs you n` counts each such ask besides the prompts. An answered or
  withdrawn ask stops counting at the next gather (H3).
- **H17** The pane's user check is stricter: `ui.user` is
  `Caller::User` **and** `permissions::at_terminal` (H8), so a top-level
  task's agent, `Caller::User` to the gate, gets none of the agents view's
  keys that read or act (`l`, `i`, `b`, `p`, `P`, `x`, `n`, `a`). The
  refusal reads "the agents pane acts as you; open jeet from your own
  terminal, outside jeet's tmux sessions and any task's pane". `?` lists
  `a` and puts it on the "only from your own terminal" line. The check is
  made once, when the tasks screen starts; `⏎` in the answer form does not
  make it again, since where jeet runs does not change while it runs.

### Phase 4 implementation notes: watch rules to a manager

Notes H18-H20, numbered as the phase 4 plan maps them (its R18-R20).

- **H18** `action = "manager:<slug>"`. `RuleAction` gains `Manager(String)`
  and loses `Copy`, with a hand-written serde form as a string:
  `assistant`, `task`, `notify` or `manager:<slug>`, written back as read.
  An unknown word is a parse error, as before, and so is `manager:` with an
  empty or blank slug. The slug is not resolved when config.toml is read
  (P6): a manager made or archived later is found, or not, at the next
  poll. `watch::decide` returns the action by clone.
- **H19** Dispatch. Each rule's slug is looked up with `get_task_by_slug`
  alone, and must be an active task of kind `manager`; never `find_exact`,
  which also matches an id and a task's name in any case, and a name is
  what a manager writes. Missing, archived, not a manager, or a delivery
  that errs: the events go to the assistant with a warning in the watch
  log, as a `task` rule's do with a PR no task has; what the assistant
  cannot be given is notified. A manager's events are batched under
  `MAX_MESSAGE`, as the assistant's are (`watch::batched`, one function
  for both), each message headed by `MANAGER_HEAD`, unfenced and in jeet's
  words only, since a manager takes unfenced text as its parent's or the
  user's; each event is fenced as `<github-event untrusted="true">`. Each
  is logged as `manager <slug>`, which the poll's summary line and the
  assistant's summary of recent events show. Only an event left `Held` is
  notified too, as for the others. P22's owner rule is unchanged: a PR's
  news never reaches a manager by ownership, only by a rule. The manager
  acts on what it gets within its own grant and subtree; the rule is the
  user's, so nothing about scope changes.
- **H20** An event may start a manager that is not running: the delivery
  is `TmuxAgent::new` with `may_start` left true, through the start gate
  and `max_live`, as a `task` rule's to a worker. The user wrote the rule
  for the manager to act on these events; one that could not start it
  would sit `Held` and notify the user on every poll. A paused or stopped
  manager keeps them queued until resumed (`Paused`, not notified). The
  manager template's untrusted-data section names `github-event`, but it
  reaches new homes only (`ensure_files` never replaces a template, P21),
  so what an existing manager needs to know is in `MANAGER_HEAD`, which
  every message carries.

### Phase 4 implementation notes: --reparent

Notes H21-H30, numbered as the phase 4 plan maps them (its R21-R30). Only
`jeet task delete <manager> --reparent` is built; `jeet task move`, which
the plan left to the user's say, is not, nor what only it needs (a cycle
check, depth, budget room above, a node-scoped `--parent`).

- **H21** One library path, general enough for a move. `tasks::plan_move`
  plans moving tasks, each with its subtree, under a new parent, writing
  nothing; `plan_reparent` calls it with the manager's children (archived
  ones too) and its parent, and `delete_reparenting` deletes the manager
  and writes the move. The rows to write are a plain `db::tasks::MoveRows`
  (movers, new parent, the task deleted, the new parent's fan-out, each
  subtree node's grant before and after, the setters rewritten), so `db`
  does not depend on `tasks`; there is no `MoveLimits`, since the depth
  and budget checks it would carry are only a move's. `--reparent`
  conflicts with `--recursive`, and a task with no children is refused
  ("delete it without --reparent"), as is the assistant. It is CLI only:
  the tasks list's `D` stays recursive, and the agents pane has no key for
  it (R31).
- **H22** Who may: what may `delete`. The gate asks `task.delete`, which
  is held, so a manager runs it only when a parent named it in its grant;
  it names the manager by exact slug, strictly below itself
  (`Verb::TaskDelete`), and the action is logged to its `actions.log` as
  "deleted task x, moving the tasks under it to its parent". The user, and
  the assistant within `[assistant] allow`, may so delete any manager in
  the tree.
- **H23** Inside the tree. The movers are a manager's children, so each
  has a parent and goes to the assistant or a manager; nothing moves to or
  from the top level, and the movers' gating does not change.
- **H24** Invariants, checked twice. `move_refusal_in` checks, on the
  connection it is given: the new parent exists, is active, and is the
  assistant or a manager; no mover is the assistant and each has a parent;
  for a delete, the deleted task is still under the new parent and its
  children are exactly the movers; the new parent's active children plus
  the active movers, the deleted task not counted, fit its `max_children`
  (archived movers do not count; the assistant has no limit); no node's
  effective run state changes (H26); and the subtrees, their stored grants
  and the setters to rewrite are as planned, and no planned grant is
  wider than the new chain of managers holds now (a narrowing meanwhile,
  C14). Anything that changed since the plan is "the tasks to move changed since jeet looked; try again".
  `check_move` runs it before anything is asked or removed, and
  `apply_move` again in one IMMEDIATE transaction before it writes. A
  reparent moves to the grandparent, so it cannot make a cycle, every
  moved node becomes shallower, and spend stays above the movers: those
  are not checked, and `insert_child` has no depth re-check yet, since
  nothing else can make a node deeper than its creation did.
- **H25** Grants never widen, now or later. Every node of the moved
  subtrees, archived ones too, has its stored grant replaced by its
  effective list before the move, less what the new chain cuts. For a
  reparent the new chain is the old one less the deleted manager, so the
  effective lists are unchanged: what goes is only what a node's grant
  named and the deleted manager cut. That is dropped, not freed, so it
  cannot become effective unannounced if the new parent is later granted
  it. Each node that drops something is named in the confirm and the
  output ("w's grant drops pr.comment, which it could not use where it
  was"). With effective lists unchanged, no harness rules go stale and no
  `record_held` is needed.
- **H26** Run state. A reparent is refused if it would change any moved
  node's effective run state, which for a delete means the manager's own
  pause or stop held the tasks under it: "task w is paused where it is,
  and would be running under assistant; pause or resume first". A node
  paused or stopped by a setter
  off the new chain (the deleted manager, say) is held by the caller
  instead when the caller ranks as high (`run::outranks`); otherwise the
  move is refused, so no node escapes a hold by moving.
- **H27** Budgets. The deleted manager's use is folded into its parent
  (`keep_spend_in`) and its children's use is still below it, so no
  total above changes and no budget is checked or enforced.
- **H28** Reports. In the transaction, every report addressed to the
  deleted manager, open or not, is re-addressed to its parent before its
  row goes, so the `ON DELETE CASCADE` on `to_id` does not take the
  children's history or their open questions; the output says how many
  were kept. The manager's own reports go with its row. A moved task's
  open question is named to its new holder in the status report of H29,
  but not delivered to it again: re-delivering each open ask, fenced as an
  escalated question `via="jeet (moved)"`, needs phase 4's definition of
  an open ask and its escalation fence (PR A1), and is a follow-up once
  those are in. A report the deleted manager answered keeps its id in
  `answered_by`, which has no foreign key; showing it as "a deleted task"
  is A1's, with its display of answers.
- **H29** Telling the tree. After the commit (`tasks::after_move`): every
  active manager in the moved subtrees gets its `BRIEF.md` again, since
  its path changed (`manager::write_briefs`, moved from the CLI module so
  the library can call it); a running mover is handed its new brief
  (`reports::brief_context`) with `may_start(false)`, and one not running
  gets it as it starts; and each active mover files a jeet-worded
  `status` report to its new parent, "moved here from manager api by the
  user; its open question #42 is yours now" (`StatusEvent::Moved`,
  through `reports::auto_status` as the hooks' status events, so it never
  starts the parent; it is never deduplicated by time, as a budget report
  is not, so a task moved twice in a second tells both parents). The open question is `manager::open_ask`'s. Every
  line printed names tasks by slug and path, never by a task's name, which
  a manager writes. A delivery that fails is a line in the output; the
  move stands.
- **H30** Order and accepted gaps. The plan and the confirm come first,
  then the manager's agent is ended and the move is checked again, so a
  plan gone stale meanwhile is refused with nothing deleted. Its files are
  then removed as a lone delete's are, and the move is written. A refusal
  after that, from a change in the instant between, leaves the row ("task
  api's files are removed but its row is kept: run the delete again"), as
  `--recursive` does (P23). Unfenced messages the
  deleted manager queued in a mover's inbox stay; the new parent's summary
  introduces them as its parent's or the user's, which is no longer
  exact. The tasks list's and agents pane's `x`/`D` confirms act on the
  ids they recorded, even if a task moved in between.

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
| 3 (#35) | the `i` view (built) and inbox depth on each row (built); `wake: armed/none` comes with 2a, so the pane as built shows nothing for wake (A19) |
| 4 (#36) | answers, copies and watch events use today's delivery; with no 2a, the wake-cap condition does not apply (were 2a built, they could arrive mid-turn, and the wake cap and turn accounting would have to exist first) |

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
