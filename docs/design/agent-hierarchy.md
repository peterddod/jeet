# Hierarchical agents

Status: **draft, for review**. Phase 1 (#31), #32 and #33 are built; the
notes after §10 say where they departed from this text. The cloud placement
in §12 is decided but not built, and its open questions are in §14.
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
were to move to v5. #33 took v5 instead, also additive: `tasks.placement`
(`local`, `cloud` or `local-sandbox`, default `local`, checked by a `CHECK`)
and the `task_anchors` table (5.7). The `manager` kind, `run_state`,
`task_grants`, `task_budgets` and `read_only` move to v6, whose rebuild of
`tasks` must copy `parent_id` and `placement` and re-create
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
on a default branch, and a placement with a boundary (§11) is the boundary.

### 5.7 What this does not protect against

Every agent runs as the user, with a shell. An agent that sets out to bypass
jeet can unset `JEET_TASK_ID`, edit `config.toml`, or attach to another tmux
session. The gate stops honest mistakes and agents steered through jeet's own
commands. A hard boundary needs the harness's sandbox and deny rules: no
writes outside the node's own directory, and no `tmux`, `sqlite3` or edits to
`~/.jeet/config.toml`. jeet writes none of those rules today (its deny rules
cover only pushes and merges, §5.6); they are what a `local-sandbox`
placement would provide (§11). It goes the other way for jeet itself: the
settings jeet gives Claude Code list `jeet` in `sandbox.excludedCommands`,
since the ancestry walk cannot see past a sandbox's pid namespace (Linux)
and jeet writes under `~/.jeet`, so with the user's sandbox on, jeet, and
possibly whatever a Bash command chains with it, runs unconfined. The `auto_trust` scope from #23 must exclude manager homes, as it
already excludes the assistant's, unless the user opts in.

**Decision (#33).** Where and how a node runs is its *placement*, chosen
per node and recorded in the index (`tasks.placement`):

- `local`: tmux panes on the user's machine, unconfined and cooperative.
- `cloud`: Claude Code cloud sessions in Anthropic-managed VMs. This is the
  first real boundary. It is designed in §12 and not built.
- `local-sandbox`: a confined local runtime, reserved and not built.

The root (assistant) and the workers it parents directly may run `local`.
Every manager below the root, and every node under one, must run on a
placement whose boundary is not `None`. A child is never less confined than
its parent. jeet refuses to start a node that breaks this, and the refusal
names the reason (`placement::runtime_for`, on every start and interrupt).
The first boundary implementation is `cloud` (decided). Which cloud runtime
comes first waits on Q1 (§14) and on the probes in `docs/cloud-probes.md`.

**Local identity.** A local caller is the task bound to the nearest
`(pid, start time)` in its process ancestry that jeet recorded when it
launched that pane or review job. The record is written only by the
launching jeet, never re-bound, and ignored after a reboot. `JEET_TASK_ID`
and `JEET_ASSISTANT` are hints. A hint that disagrees with the anchor is
refused and logged. A hint that claims a parented task or the root with no
anchor behind it is refused as unconfirmed. Hooks carry their task in argv
(`--task`), and report up only when that agrees with the anchor. This fails
closed against mistakes and casual forging. It is not a boundary against a
local agent that sets out to escape:

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
| Direct edits to `index.db` or `config.toml`; a forged `JEET_HOME` (acts only on a fake index) | **open locally** / harmless |
| User windows opened inside a parented task's or the assistant's session | refused for gated commands (inherited hint, no anchor), by design |
| jeet's own helpers (`watch --once`, `task-startup`) while their parent is alive | may resolve as the task; they are Free and must never ask for identity |

The open rows are closed only by running the node behind a boundary, which
is why managers below the root require one. Agents started before #33 have
no anchor and must be reopened.

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
| **deny rules** | yes (#32): `enforcement() >= Rules`; Claude Code only; a lint, not a gate (a boundary placement is the boundary, 5.7) | yes for children (5.6) and read-only worktrees (§1) |
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

**Gate before phase 2 ships: met for local identity by #33.**
- Local callers are identified by process ancestry and fail closed (5.7).
- Every node has a placement, and starts go through
  `placement::runtime_for`, which refuses a node below a non-root manager
  unless its placement has a boundary (§11).
- Phase 2 may not start a manager below the root until a boundary placement
  is built. The first will be `cloud` (§12), blocked on Q1 and the probes in
  `docs/cloud-probes.md` (§14).
- Phase 2a, an optional watcher wake channel for local Claude Code nodes
  (§13), runs its spike alongside those probes. The gaps cloud placement
  leaves in phases 2–4 are in the table below.

**What cloud placement cannot provide.** *Fence-off*, used below, means:
jeet sets `run_state = 'stopped'|'paused'`, rejects all of the node's
envelopes, sends it nothing more, fences off its subtree the same way, and
notifies the user with each session URL to stop or archive on claude.ai.
Each gap degrades to "jeet stops brokering for the node and tells the user";
none is presented as enforced.

| Requirement (§) | Cloud placement | How, or why not |
|---|---|---|
| Managers below the root (§1, ph2) | **Supported** | cloud session plus mailbox, message-driven |
| Effective allow-lists, intersection, scope, no sideways sends (5.1) | **Supported for jeet commands** | enforced at the local broker on every envelope |
| Same, for direct git/`gh` actions (5.6) | **Not supported per node** | one GitHub identity. Repo-wide no-bypass rulesets plus attestation only |
| Held capabilities (5.1) | **Supported by refusal** | never grantable to cloud nodes. Merge is root-only through `jeet pr merge`. GitHub-side protection needs Q4 |
| Depth, fan-out (5.2) | **Supported** | every spawn goes through the broker. Nodes cannot create sessions themselves, pending probe P2 |
| `max_live` and queueing (5.2) | **Degraded** | jeet counts nodes it has not stopped or ended as live. It cannot see whether a session is running or idle. Starts over the limit are queued |
| Time budget (5.3) | **Degraded** | wall clock since start, enforced by fence-off at 100%. The node is not paused |
| Turn budget (5.3) | **Not supported** | no turn events reach jeet |
| Cost budget (later, #26) | **Not supported** | no per-session usage source |
| Pause / resume (5.4) | **Degraded** | pause is fence-off plus holding the node's down-queue in the index. The current turn keeps running. Resume releases the queue |
| Stop, `stop --all` (5.4) | **Degraded** | fence-off. Real stop or archive is UI-only. jeet detects archiving when `-p --cloud` fails |
| Harness interrupt (#32) | **Not supported** | no CLI or API |
| Message delivered mid-turn | local Claude Code: **Supported** (wake channel, §13); cloud: **Not supported** | a cloud message queues until Claude reads it |
| Read-only worktrees (§1, decision 2) | **Not supported** | the node has a writable clone and push access. The read-only intent is cooperative |
| `worktree add` mid-session | **Not supported** | one repo per session, fixed at start. Use a child node instead |
| Recursive archive/delete (ph2) | **Degraded** | index bookkeeping is supported. Sessions are archived by the user in the UI. jeet deletes mail branches locally |
| Agents pane rollup (ph3) | **Supported from the index** | rows come from reports and `last heard`. No live status, needs-input or turns |
| Attach `⏎` (ph3) | **Degraded** | opens the claude.ai URL. `--teleport` gives a one-way local copy |
| Worklog `l` (ph3) | **Supported** | jeet appends verified envelopes and results to the node's local worklog |
| Inbox `i` (ph3) | **Degraded** | shows jeet's held and sent log; the server's queue is not visible. Local panes with a wake channel no longer show message text either, so `i` is where both are read |
| Pause/stop keys (ph3) | **Degraded** | fence-off, as above |
| Escalation and questions (ph4) | **Supported, with latency** | `report question` goes up through the mailbox, the answer comes down by `-p`, and copies go to each manager on the path. The last hop to a local manager uses the wake channel (§13): one tick plus the push, with no wait for Stop |
| Answers bypassing jeet | **Not preventable** | the user, or the node's own AskUserQuestion or permission prompt, can act on claude.ai, which jeet does not see. Nodes are told to use `jeet report question` (cooperative) |
| `jeet watch` rules to managers (ph4) | **Supported** | routed through `-p --cloud` as pointers only; to a local manager, through the wake channel. Events caused by nodes themselves never arrive as notifications |
| Reparent (ph4) | **Supported** | index-only. Session and key are unchanged, and scope is re-evaluated on the next envelope |
| Tree works while the Mac sleeps | **Not supported** | brokering, spawns, answers and budget checks stop. Nodes keep running until their next jeet call (Q6) |
| Multi-repo node | **Not supported** | the CLI attaches one repo |
| Non-Claude harnesses below the root | **Not supported** | cloud runs Claude Code only. Other harnesses wait for `local-sandbox` |

**Phase 2: managers.** (#34; needs #32, #33 and a built boundary placement)
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

None of the questions this RFC first raised are still open. The user decided
(2026-10-11). The cloud-placement questions in §14 are open; #33 depends on
none of them.

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
10. **Identity and placement (#33).** Managers below the root, and every
    node under them, run behind a real boundary through a per-node
    placement. The first implementation runs them as Claude Code cloud
    sessions; local sandboxing stays in the design. Local callers are
    identified by process ancestry to a pane or job jeet recorded, and fail
    closed; the env is a hint. `libc` is approved as a direct dependency for
    macOS pid lookups (5.7, §11–§12, #33).

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
  attached user typing feedback into one may still have it discarded. No retry, no confirmation and no status
  repair yet (the status reads `running` until the next hook, as after a
  manual Esc); those come with pause. The assistant is refused (5.4).
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
  only `LocalTmux`, `[managers] placement`, this text and
  `docs/cloud-probes.md`. No cloud code, no `[cloud]` config, no
  `worker_placement`, no `Delivered::Sent`/`Watched`, no MAC crate. Nothing
  depends on Q1–Q6.
- **I1** The anchor is the identity: a `(pid, start)` row in
  `task_anchors`, kind `pane` or `job`, recorded only by the jeet that
  spawned that pid, at one of three launch paths: a task's tmux session
  (`runtime`), an assistant window (`assistant::open`) and a review job's
  runner (`jeet review`). A process never records its own anchor.
- **I2** The walk starts at jeet's own pid and goes up through the kernel:
  `/proc/<pid>/stat` on Linux (split after the last `)`), `proc_pidinfo`
  (`PROC_PIDTBSDINFO`) through `libc` on macOS. It stops at pid ≤ 1, a
  failed lookup, a zombie, a cycle or depth 64. Other unix targets find
  nothing and fail closed. jeet never runs `ps` for identity.
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
  ends the task's review job (its runner's process group).
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
  else nothing (the user). If the anchor cannot be recorded, the runner is
  killed and the job ends with 127. A runner refused after the go byte
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
- **I12** `Placement { Local, Cloud, LocalSandbox }` on `tasks.placement`;
  `support()` gives `Support { boundary }` (§11).
- **I13** `NodeRuntime` has `start` and `interrupt` only; `runtime_for` is
  the one fail-closed choke point. The five start sites and the interrupt
  site route through `runtime::ensure_running` and `runtime::interrupt`
  unchanged.
- **I14** The boundary rule runs on every start and interrupt. Under D12 its
  refusing branch cannot be reached from the CLI; phase 2 adds the
  `kind == 'manager'` condition and calls it from `manager new`, `grant`
  and child starts.
- **I15** A new task's placement: `local` at the top level or under the
  root, else `[managers] placement`. A missing parent is refused at create.
- **I16** `[managers] placement` defaults to `cloud`. `local` is an error
  where it is used, not at load. `local-sandbox` parses and is refused at
  start as not built.
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
    E15) are superseded: #33 built no sandbox. The boundary is a
    placement (§11); `Enforcement::Sandbox` and write denies wait for
    `local-sandbox` or phase 2. The 0600 settings file is built (E9):
    `--task <id>` on every hook had filled the inline budget and cut the
    longest default branch denied from 24 bytes to 16.

## 11. Placement

A node's placement says where its agent runs and what confines it.

```rust
pub enum Placement { Local, Cloud, LocalSandbox }   // tasks.placement
pub enum Boundary { None, Machine, Sandbox }
pub struct Support { pub boundary: Boundary }       // Placement::support()
```

| Placement | Boundary | Built |
|---|---|---|
| `local` | `None` | yes: tmux panes, as before |
| `cloud` | `Machine` | no: designed in §12, waiting on Q1 and the probes |
| `local-sandbox` | `Sandbox` | no: reserved |

`Support` gains more fields only when phase 2–4 code reads them.

**The boundary rule.** A task needs a boundary when its parent exists and is
not the root, or when its parent's placement has one (a child is never less
confined than its parent). Such a task on a placement with `Boundary::None`
is refused: "managers below the root, and every node under them, must run
behind a boundary; placement local has none". A parent missing from the
index refuses too.

**Choosing a placement.** A task made at the top level or under the root
gets `local`. Anything deeper gets `[managers] placement`:

```toml
[managers]
placement = "cloud"   # the default; "local" is an error; "local-sandbox" is refused as not built
```

**The runtime seam.**

```rust
pub trait NodeRuntime {
    fn start(&self, app: &App, task: &TaskRecord, session: Option<&str>, prompt: Option<&str>) -> Result<Running>;
    fn interrupt(&self, app: &App, task: &TaskRecord) -> Result<Interrupted>;
}
pub fn runtime_for(app: &App, task: &TaskRecord) -> Result<&'static dyn NodeRuntime>;
```

`runtime_for` checks the boundary rule, then returns `LocalTmux` for
`local` and refuses `cloud` ("not built yet: it waits on the choice of cloud
runtime (Q1) and the probes in docs/cloud-probes.md") and `local-sandbox`
("not built yet"). `runtime::ensure_running` and `runtime::interrupt` keep
their signatures and dispatch through it, so every start and interrupt site
routes without change.

Left off the trait, each for a reason:

- **`stop`**: archive and delete must always work, and `tmux::kill` is
  already a no-op when there is no session, so a non-local task archives
  and deletes its local state correctly.
- **`send`**: `messages::Agent` is already the local abstraction. A message
  to a non-local task finds no pane and routes to Start, which refuses it
  in the foreground. Typing into a live session goes through `runtime_for`
  too (`TmuxAgent::type_in`), so a session left under the name of a task
  jeet may not run locally (one from before an upgrade, or one started
  outside jeet) is never typed into; a send that only queues still queues. A background send (`JEET_BACKGROUND=1`, which may not
  start an agent) holds it in the inbox instead (`Delivered::Held`), as for
  any agent it may not start; nothing ever delivers it until that placement
  is built.
- **`activity`, `attach`, `collect`**: no caller until phase 2. With no pane
  and no status files, a non-local task reads as not running.

**Introduced with the second runtime** (a sketch, not built):

```rust
pub struct Support {
    pub boundary: Boundary,
    pub interrupt: bool,           // local: Esc via the harness; cloud: false
    pub stop: bool,                // local: kill tmux; cloud: false (UI only)
    pub live_status: bool,         // local: hook status files; cloud: false
    pub turn_count: bool,          // local: Stopped events; cloud: false
    pub usage: bool,               // when #26 lands; cloud: false
    pub read_only_worktree: bool,  // local: yes; cloud: false
}
pub enum Attach { Tmux { session: String }, Url(String) }
pub enum Inbound {                 // what collect() hands to the broker
    Command { node: String, seq: u64, argv: Vec<String>, body: Option<String> },
    Rejected { node: String, reason: String },   // bad MAC, replay, rewrite
}
pub trait NodeRuntime {
    fn start(..) -> Result<Running>;
    fn interrupt(..) -> Result<Interrupted>;              // Unsupported on cloud
    fn send(&self, app: &App, task: &TaskRecord, msg: &str) -> Result<Delivered>;
    fn stop(&self, app: &App, task: &TaskRecord) -> Result<(), Unsupported>;  // cloud: fence-off (§8)
    fn activity(&self, app: &App, task: &TaskRecord) -> Activity;  // cloud: LastHeard(ts) | Unknown
    fn attach(&self, app: &App, task: &TaskRecord) -> Attach;
    fn collect(&self, app: &App) -> Result<Vec<Inbound>>;  // local: empty
}
```

`messages::Delivered` then gains `Sent` (handed to the cloud server's
queue; jeet cannot tell whether the session was busy) and, with phase 2a,
`Watched` (§13). `[managers] worker_placement` (default `local`) lets
root-parented workers run in the cloud too. A later `Placement::Managed`
(Q1) would be another implementation of the same trait.

Call sites once the second runtime lands:

| Call site | Today | With placement |
|---|---|---|
| `runtime::ensure_running` (`task new --prompt`, `task open`, a message to a stopped agent) | `runtime_for(task).start` (#33) | the same; cloud starts a session |
| `runtime::interrupt` (`task interrupt`) | `runtime_for(task).interrupt` (#33) | the same; cloud is unsupported |
| `runtime::stop` (archive, delete) | kill tmux | `.stop`; when it is unsupported, fence-off |
| `messages::deliver` (`task send`, `watch` dispatch, the Stop hand-over) | `TmuxAgent` routing | `.send`; cloud skips the inbox, the Stop hand-over and `Route` |
| `reports::deliver` | to the root through `assistant::deliver` | unchanged: the root is always local |
| `status.rs` hook path (`run_hook`, `auto_status`) | hook events become reports | local only; a cloud node's mailbox `report` envelopes go into the same `reports::send` |
| `reports::sweep_idle` | idle from status files | cloud: idle means "no verified envelope for `idle_report_mins`", flagged `last heard` |
| `permissions::authorize` | ancestry identity (#33) | local unchanged; cloud: `authorize_as(Caller::Node(id), …)` in-process, from verified envelopes only |
| `watch::run_once` | poll GitHub, dispatch, sweep | adds `collect()` and broker execution, under `WatchLock`, before dispatch |
| TUI refresh thread | status files | also `collect()` every `[cloud] poll_secs` while cloud nodes are live, under `WatchLock` |
| TUI attach (`⏎`) | `tmux attach` | `Attach::Url` opens the session; `claude --teleport <id>` is offered as a one-way local copy |

## 12. Cloud managers (designed, not built)

This is the decided design for the first boundary placement. Nothing here is
built: it waits on Q1 (§14) and on the probes in `docs/cloud-probes.md`.

**What the cloud boundary is.** It is a boundary *around the user's
machine*. A cloud node cannot reach `index.db`, `config.toml`, tmux, other
nodes' homes, the user's `gh` token or the user's shell. Every jeet action it
takes goes through one broker, the local jeet process, which re-checks
identity, scope and the effective allow-list on every call.

**What it is not.** It is *not* a boundary between cloud nodes, and it does
not limit what a node does directly through git or GitHub:

- all nodes push and comment as one GitHub identity;
- all nodes share one cloud environment's egress and environment variables;
- a node's jeet identity rests on a per-node key that sits in its own
  conversation.

### 12.1 Running a node

- **Start.** jeet pushes the node's branch `jeet/<node-id>/base` from a
  worktree in its store, then runs `claude --cloud "<brief>"` there with
  `CLAUDE_CONFIG_DIR=[cloud].config_dir`, and stores the session id and URL
  in the index. If the CLI prints no parseable session id without a
  terminal, the fallback is a routine `/fire` per node tier (P1). The CLI
  attaches one repo per session, so a cloud node is single-repo, and a
  child on another repo is a separate session jeet starts.
- **Send.** `claude -p "<fenced msg>" --cloud <session-id> --output-format
  json`. The server queues it. It fails on archived sessions, which is how
  jeet notices archiving. This queue is also the cloud node's wake channel:
  cloud runs no jeet watcher (§13).
- **Brief.** The first prompt carries the node id, its goal and done text
  (`<jeet-brief>`), the mailbox protocol and the node key. Cloud nodes get
  no session-start context from jeet. Repo `.claude/settings.json` hooks
  would run there, but the node can edit them, so jeet relies on none of
  them, a watcher included.
- **Shim.** `jeet cloud setup-script` prints an environment setup script
  that embeds a small `jeet` shim (POSIX sh plus python3). The user pastes
  it into the cloud environment once. The shim turns `jeet report …`,
  `jeet task new …` and similar into signed envelopes on the mailbox
  branch. It is not trusted: jeet trusts only what it verifies.
- **Managers are message-driven.** Idle VMs are paused and reclaimed, and
  background commands die after about 30 minutes. A manager ends each turn
  with its state in its conversation and on its branch, and wakes on the
  next `<jeet-result>` or report sent down. It never runs a poller.

**Managed Agents would do this better, and is not the choice.** Its custom
tools give a synchronous call back with identity bound to the session and no
key in the conversation, and it has `user.interrupt`, per-session usage and
cost budgets, and per-session repo tokens, which cover most of the §8 gaps.
It is a different product: another agent loop without Claude Code hooks or
settings, Console API billing, a workspace and key, and an HTTP client in
jeet. The trait is shaped so a `Placement::Managed` could be added later.
Making it the first instead is Q1.

Rejected for v1: a jeet remote MCP server (a daemon and a public tunnel),
transcript polling (no documented API), Claude Code Projects (UI only), and
self-hosted environments (a Team or Enterprise plan and a runner; a
strong-identity option later, through verifiable session JWTs).

### 12.2 The mailbox: up from a node

- A node writes envelopes to `jeet-mail/<node-id>` in its session's repo,
  one file per envelope (`<seq>.json`), pushed through the proxy:
  `{v, node_id, session_id, seq, ts, argv[], body}` plus
  `mac = HMAC-SHA256(K_node, canonical_json_without_mac)`.
- At most 8 KiB per envelope and 32 new envelopes per node per tick. An
  issue or PR comment is never accepted as a command.
- `collect()` runs `git fetch origin '+refs/heads/jeet-mail/*:refs/jeet-mail/*'`
  in the store, with `core.hooksPath=/dev/null`, `core.fsmonitor=false` and
  `GIT_CONFIG_NOSYSTEM=1`, and reads new blobs with `git cat-file`. No HTTP
  client, one fetch per attested repo with live cloud nodes.
- Triggers: the `jeet watch` launchd tick, the TUI refresh thread every
  `poll_secs`, and `jeet cloud sync`, all under `WatchLock`. GitHub does not
  notify a user of their own actions, so `jeet watch` notifications cannot
  carry this.
- Each verified envelope is claimed with a unique `(node_id, seq)` row
  before it runs, so the TUI and watch cannot run it twice. Latency is one
  tick plus the push.

### 12.3 How hard a node's identity is to forge

jeet accepts an envelope only if the MAC verifies under the node's key,
`node_id` matches the branch, `session_id` matches the bound session, `seq`
is above the last seen, `ts` is within 15 minutes, and the branch history
includes the last verified commit (a rewrite is rejected and the user told).

`K_node` is 32 random bytes kept in `index.db`, delivered only in the node's
first `claude -p --cloud` message, never through environment variables, the
repo or the setup script.

- **Cannot forge:** another cloud node (VMs are isolated, and pushes as the
  shared identity carry no authority); anyone with push or comment access to
  the repo.
- **Can forge:** anything that gets X's transcript or tricks X into leaking
  the key (it is in the model's context, visible on claude.ai and copied by
  `--teleport`); any local process holding the cloud account login, which can
  send X a fake "new key". Rekey messages therefore carry a MAC under the
  current key, a node that loses its key (compaction) is ended and replaced,
  and a key is never re-sent in the clear. The separate login (12.6) keeps
  ordinary local agents off the account, cooperatively.

A stolen key gets exactly X's effective allow-list and subtree scope.
Identity is *attributable and scoped*, not secret-proof against a
compromised victim, which is why held capabilities never go to cloud nodes.

### 12.4 Commands a cloud node may send

Every command runs in-process through `authorize_as(Caller::Node(id), …)`,
never as a `jeet` subprocess with `JEET_TASK_ID` set, and must name its
target: no fallback to the current directory.

| Command | Phase | Notes |
|---|---|---|
| `report progress\|done\|blocked\|question` | 1 | to the parent through `reports::send`, fenced `<jeet-report untrusted>` |
| `task new --parent <self or a descendant> --repo <attested> --goal … --done …` | 2 | jeet starts a cloud session, subject to depth, fan-out, `max_live` and grant ⊆ effective |
| `task send <descendant>` | 2 | scope applies; no sideways sends |
| `manager new\|grant\|budget\|pause\|resume\|stop <descendant>` | 2 | `pause` and `stop` mean fence-off |
| `tree` / `task ls` (own subtree) | 2 | answered in a `<jeet-result>` |
| `report answer\|escalate` | 4 | |
| `pr create --head jeet/<self>/…` | 2 | with the user's local `gh`; the head must be in the node's namespace, and `git.push` is required |
| `pr comment\|review\|merge` | — | refused in v1: held capabilities |

### 12.5 Down to a node, and how messages are framed

- Results are batched per tick into one `claude -p --cloud <id>` message per
  node: `<jeet-result seq=N ok=true|false>…</jeet-result>`, with a MAC under
  `K_node` so the shim can tell real results from forged ones.
- Messages from a parent and GitHub events are fenced `<jeet-message
  untrusted>`. A GitHub event goes down as a pointer only ("PR #12 has a new
  review, fetch it"): attacker-written text never arrives as a user turn.
- **Two framings.** A cloud `-p` message carries full user authority, so no
  event text may ever look like a `<jeet-result>` or a rekey message; those
  forgery rules are cloud-only. A local node with a wake channel (§13)
  receives messages as peer messages from another session, which cannot
  approve anything. 5.5 must say which framing each direction gets; the
  brief stays a user turn.

### 12.6 Credentials

**GitHub.** The Claude GitHub App, installed only on the repos in
`[cloud].repos`, not `/web-setup` (which exposes the user's whole `gh` token
behind the proxy). The proxy limits API calls to the attached repo, blocks
GraphQL and rejects branch deletion and tag pushes, but does not limit which
branches a push updates. So a node, as the shared identity, can push any
branch on its repo, comment and review through REST, and call the merge API.

- No-bypass rulesets must cover the default and protected branches (PR
  required, no force push, at least one approving review, which the shared
  identity cannot give itself) and `jeet-mail/**` (no force push). They bind
  the user too. Keeping merge with the root on GitHub's side needs a
  required check from a jeet-owned app (Q4).
- Branch namespaces (`jeet/<node-id>/…`, `jeet-mail/<node-id>`) are enforced
  only on brokered actions. Direct pushes cannot be limited per node with
  one identity. Siblings on one repo can read each other's branches;
  separate repos are the only isolation.
- `pr.merge`, `pr.comment`, `pr.review` and `task.delete` cannot be granted
  to a cloud node. Merges happen only through `jeet pr merge`, by the root.
- A repo enters `[cloud].repos` only after the user attests that no repo
  secrets are reachable from `on: push`, `pull_request_target` or
  `issue_comment` workflows on `jeet/**` and `jeet-mail/**`, and that there
  is no comment-triggered automation. jeet refuses cloud placement on other
  repos.

**Anthropic account.** Every cloud call uses `CLAUDE_CONFIG_DIR=[cloud].config_dir`,
logged into a dedicated claude.ai account with no connectors (Q2): connectors
reach every cloud session by default, so a prompt-injected manager could read
and exfiltrate mail. jeet refuses to start a cloud node until `jeet cloud
doctor` confirms no connectors (P6), or the user attests it. Usage counts
against that account's limits, and jeet cannot stop a node mid-turn.
Bedrock, Vertex and Foundry users, and ZDR or HIPAA orgs, cannot use cloud
placement.

**Secrets.** Environment variables are readable by every session in the
environment, and from the CLI every node shares one environment, so they
never hold per-node secrets. The only per-node secret is `K_node`, which
travels in the conversation. (If phase 2a settles on W3, its messaging token
is a secret too: 0600, out of logs and the worklog.) Egress is `Trusted`; the
Anthropic API and the GitHub proxy can carry data out even at `None`, which
is accepted.

**Config** (not built):

```toml
[cloud]
claude = "claude"                    # a claude.ai login, not Bedrock/Vertex/Foundry
config_dir = "~/.jeet/cloud-claude"  # CLAUDE_CONFIG_DIR for every cloud call
repos = ["owner/repo"]               # attested repos; others are refused
poll_secs = 30                       # TUI mailbox poll while cloud nodes are live
mail_prefix = "jeet-mail/"
branch_prefix = "jeet/"
```

### 12.7 What stays cooperative

- The root and local workers run as the user with a shell. They can escape
  the ancestry walk (5.7).
- They hold the user's `gh` token. If they can read `[cloud].config_dir`,
  they can drive cloud nodes, send messages, or read a transcript and its
  key. The cloud boundary protects the user's machine from cloud nodes, not
  cloud nodes from local agents.
- Upward prompt injection through reports, PR bodies and commits is the main
  remaining risk. Identity proves who sent text, not that it is safe.

### 12.8 What phase 2 builds for it

Once Q1 is answered and the probes pass: `placement/cloud.rs` (argv
builders, result parsing, envelopes and their checks, `<jeet-result>` and
rekey builders, the shim), the trait methods of §11, `Delivered::Sent`,
`[cloud]` and `worker_placement`, the `node_mailbox` table and claim rows,
`collect()` in `watch::run_once` and the TUI, fence-off, `jeet cloud
doctor`/`sync`, attestation for `[cloud].repos`, and refusing held grants to
cloud nodes. MAC verification waits on Q3.

## 13. Message delivery through a watcher (phase 2a)

Today jeet types messages into an agent's pane. A watcher would instead let
a background process, started by a hook, hand messages to the agent.

| Goal | Meaning |
|---|---|
| G1: seen once | the user does not see a message twice, typed and then fenced |
| G2: any time | an agent can be messaged while busy, not only idle or at Stop |
| G3: no typing | jeet never types into an input box: no draft check, no risk of answering a trust or permission prompt |
| G4: no pane | the same path could serve nodes without a pane |

**Verdict: adopt partly.** An optional *wake channel* for local Claude Code
nodes, as PR 2a of phase 2, after a spike (probes P-W1 to P-W10). It meets
G1 and G3, and G2 at tool-call boundaries. It needs no daemon and falls back
to today's path wherever it is missing. Do not:

- make it the transport: `.jeet/inbox.jsonl` stays the queue,
  `Inbox::lock` the one place a message is taken, and `task_reports` the
  record;
- require it of any harness;
- use it for cloud nodes: `claude -p --cloud` already meets G1, G3 and G4
  there, and nothing in the VM can host a watcher;
- adopt a long-lived `asyncRewake` waiter: its timeout is enforced (600 s by
  default, 30 s on `UserPromptSubmit`), so an idle manager's waiter would
  expire.

**Candidates, in order of preference** (the spike picks one):

| | W1: hook-started poster | W2: the harness watches the inbox | W3: jeet posts directly |
|---|---|---|---|
| Started by | a `SessionStart` hook entry with `async: true`, in jeet's `--settings` | `SessionStart` returns `watchPaths: [<task>/.jeet/inbox.jsonl]`; a `FileChanged` hook with `asyncRewake: true` | nothing; `SessionStart` stores the socket path and token |
| Reaches the model | posts to `CLAUDE_CODE_MESSAGING_SOCKET` as the agent's own child, so no `crossSessionInbound` setting is needed | exit 2 with the messages on stderr, as a system reminder | `deliver` posts to the socket; needs `crossSessionInbound: "accept"` |
| Mid-turn | documented: the next tool-call boundary | unverified (P-W5) | documented |
| Main unknown | the message wire format (P-W1); own-child status of a long-lived hook child (P-W2) | `FileChanged` with `asyncRewake`; the watch surviving `Inbox::replace` | the wire format; a token outside the session; repo `refuse` |

W1 is preferred: it wakes an idle session and lands mid-turn by documented
paths, needs no settings change, and its process dies with the agent. W2 is
the fallback; W3 is a last resort, as it keeps a session token in jeet.

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
suits reports going up but may let a child ignore a manager's command
(P-W9); W2's system reminder is trusted more than user text, a larger
injection risk. A repo's `crossSessionInbound: "refuse"` or a `--bare`
session turns it off, so jeet must detect a failed post and fall back to
typing. Async hook lifetime on interactive exit or tmux kill is unverified;
the pid check bounds an orphan to one poll. In the assistant's shared inbox,
a message with no session wakes whichever watcher wins the lock.
`@internal` fields (`rewakeSummary`, `rewakeMessage`) are used for
appearance only.

| Phase | What lands |
|---|---|
| Before 2 | the spike, alongside the cloud probes; no code |
| 2a (#34) | `Harness::wake_hook`, `HookSetup.wake`, the settings entry, `src/waiter.rs`, `Delivered::Watched` routing in `messages.rs`, the idle skip and ack in `status.rs`, hidden `jeet inbox watch --task`/`take`. It does not block phase 2 |
| 3 (#35) | the `i` view becomes required; rows show `wake: armed/none` and inbox depth |
| 4 (#36) | answers, copies up the path and watch rules to managers may arrive mid-turn; the wake cap and turn accounting must exist first |

§9 gains: "a per-agent watcher replacing typing: accepted as an optional
harness wake capability; the inbox, its lock and the Stop hand-over stay."

## 14. Open questions

Unanswered. #33 depends on none of them; the cloud placement (§12) depends
on all but Q6, which it must state.

1. **Q1: the first cloud runtime.** Claude Code cloud sessions, with the §8
   gaps (no pause, stop or interrupt; no turn or cost budgets; no read-only
   worktrees; no per-node git gating; a key in each node's context), or
   Managed Agents, which closes most of them but uses API billing, a
   different agent harness, and a Console workspace and key?
2. **Q2: the account.** A dedicated claude.ai account with no connectors
   for cloud nodes, and on which plan? Pro/Max gets network secrets;
   Team/Enterprise gets self-hosted environments, which give verifiable
   session identity later.
3. **Q3: MACs.** `hmac` and `sha2` (RustCrypto) as new dependencies, or
   `/usr/bin/openssl` by absolute path?
4. **Q4: GitHub.** The Claude GitHub App limited to the nodes' repos,
   no-bypass rulesets on default branches (which bind the user too), and
   the Actions and automation attestation. Optionally a jeet-owned GitHub
   App, so a required check keeps merges with the local root on GitHub's
   side.
5. **Q5: live probes.** Permission to run the live, billed probes in
   `docs/cloud-probes.md`.
6. **Q6: liveness.** Accept that a cloud tree cannot delegate, report, get
   answers or enforce budgets while the Mac sleeps, and that a round trip is
   one poll tick (about 30 s with the TUI open, the launchd interval
   otherwise)?
