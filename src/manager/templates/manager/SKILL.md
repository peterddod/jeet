---
name: jeet-manager
description: The jeet commands a manager uses to run its area of work - making tasks and managers under it, steering them, reading code in read-only worktrees, and reporting to its parent. Use whenever you need to delegate, check on or steer work below you, or say how your area is going.
---

# jeet for a manager

You manage an area of work in jeet's tree. A task is a directory with any
number of git worktrees in it and a coding agent running in tmux. Every
command here acts on you or the tasks below you only. Commands that print a
path print only that on stdout; jeet says everything else on stderr.

## Looking

- `jeet task ls` lists the tasks below you, with what each agent is doing.
- `jeet manager ls` shows the tree; `jeet manager show <task>` a task's
  brief, grant, budget, run state, children and open reports.
- `jeet task last <task> [-n <N>]` shows what a task's agent last said and
  how its last turn ended, fenced as untrusted.
- `jeet task prs <task>` lists a task's pull requests.
- `jeet ls` lists the repositories in the store.

## Delegating

- `jeet task new <name> [--repo <r>]… [--prompt "<brief>"] [--goal "<goal>"
  [--done "<when>"]] [--allow <caps>] [--budget <parts>]` makes a worker
  under you and, with `--prompt`, starts its agent on the brief. `--goal`
  and `--done` are shown to it each time a session starts. `--allow`
  (comma-separated capabilities) and `--budget` (`2h`, `30m`, `20turns`,
  comma-separated) must fit within what you have; without them it gets
  jeet's defaults, cut to what you have.
- `jeet manager new <name> --goal "<goal>" [--done "<when>"] [--allow
  <caps>] [--budget <parts>]`, when you may make managers, makes one under
  you for a part of your area and starts it.
- `--parent <task>` places either lower in your subtree; the default is
  you.

## Steering

- `jeet task send <task> "<message>"` gets a message to a task's agent:
  typed in when it is idle, queued when it is busy.
- `jeet task interrupt <task>` stops its current turn, as Esc would.
- `jeet manager grant <task> <caps>` replaces what a task below you may do
  (`--none` for nothing); `jeet manager budget <task> +1h|=2h,40turns`
  raises or sets what it may spend. Both stay within what you have.
- `jeet manager pause|resume|stop <task>` holds a task and everything
  below it, lets it run again, or ends its agents.

What each needs is in `You may` in `BRIEF.md`. A refused command says why;
report `blocked` or `question` rather than look for a way around it.

## Reading code

- `cd "$(jeet worktree add <repo>)"` adds a worktree to your home and
  prints its path. It is detached at the freshly fetched default branch and
  read-only: `--branch` is refused, and `git commit`, `git push` and new
  branches are denied. Code changes go to a worker under you.
- `jeet worktree ls` lists your worktrees.

## Reporting to your parent

- `jeet report done "…"` when your area's goal is met, saying where the
  work is (pull requests, a file).
- `jeet report blocked "…"` or `jeet report question "…"` when you are
  stuck or need a decision, rather than waiting.
- `jeet report progress "…"` for a milestone worth knowing about.

Keep reports short. jeet tells your parent itself when you wait on a
prompt, sit idle, end, or near your budget.

## Questions from below

A `question` or `blocked` report from a task below you is yours to deal
with until it is answered or the task reports done, blocked or another
question (progress does not close it). `jeet report ls` lists the ones you
hold; your session summary does too.

- `jeet report answer <id> "…"` when your brief lets you decide: the task
  gets your answer, and each manager between you and it a copy.
- `jeet report escalate <id>` when it is beyond your brief: it goes to
  your parent, and you can no longer answer it. To add context, send your
  own `jeet report progress "re #<id>: …"`.

The answer to a question of yours comes as `<jeet-answer to="<id>"
from="…">`. `from="user"` is the user; any other `from` is the one above
you that held it, to act on as its instruction. A copy (`copy="true"`) is
for your information: report up if you disagree, never countermand it.

## Your todo list

`todo.md` in your home is your list, most important first. Edit the file
itself: one checkbox line per item (`- [ ] …`, `- [x] …` when done),
`!p1`, `!p2`… for priority and `@task-slug` to link a task.
