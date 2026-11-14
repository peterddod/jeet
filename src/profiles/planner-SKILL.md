---
name: plete-planner
description: The plete commands a planner uses to run its area of work - making tasks under it, steering them, answering their questions, and reporting to its parent. Use whenever you need to delegate, check on or steer work below you, or say how your area is going.
---

# plete for a planner

Every command here acts on you or the tasks below you only. Commands that
print a path print only that on stdout; plete says everything else on stderr.

## Looking

- `plete task ls` lists the tasks below you, with what each agent is doing.
- `plete task tree` shows the tree; `plete task show <task>` a task's brief,
  grant, budget, run state, children and open questions.
- `plete task last <task> [-n <N>]` shows what a task's agent last said and
  how its last turn ended, fenced as untrusted.
- `plete task prs <task>` lists a task's pull requests.

## Delegating

- `plete task new <name> [--repo <r>]… --prompt "<brief>" --goal "<goal>"
  [--done "<when>"] [--agent <profile>] [--allow <caps>] [--budget <parts>]
  [--pr auto|draft|ask]` makes a task under you and starts its agent on the
  brief. `--goal` and `--done` are shown to it each time a session starts.
  `--allow` and `--budget` (`2h`, `30m`, `20turns`) must fit within what
  you have; without them it gets plete's defaults, cut to what you have.
  `--agent planner` makes a planner of its own for a part of your area.
- `--parent <task>` places it lower in your subtree; the default is you.

## Steering

- `plete task send <task> "<message>"` gets a message to a task's agent:
  typed in when it is idle, queued when it is busy.
- `plete task interrupt <task>` stops its current turn.
- `plete task grant <task> <caps>` replaces what a task below you may do
  (`--none` for nothing); `plete task budget <task> +1h|=2h,40turns` raises
  or sets what it may spend. Both stay within what you have.
- `plete task pause|resume|stop <task>` holds a task and everything below
  it, lets it run again, or ends its agents.

A refused command says why; report `blocked` or `question` rather than
look for a way around it.

## Questions from below

A `question` or `blocked` report from a task below you is yours until it is
answered. `plete report ls` lists the ones you hold.

- `plete report answer <id> "…"` when your brief lets you decide.
- `plete report escalate <id>` when it is beyond your brief: it goes to
  your parent.

## Reporting to your parent

- `plete report done "…"` when your area's goal is met, saying where the
  work is.
- `plete report blocked "…"` or `plete report question "…"` when you are
  stuck or need a decision.
- `plete report progress "…"` for a milestone worth knowing about.
