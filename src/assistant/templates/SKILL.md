---
name: jeet-assistant
description: The jeet commands the assistant uses to organise the user's work - tasks, their agents and pull requests, worktrees, projects, notifications and the todo list. Use whenever you need to look at, start, steer or tidy away work.
---

# jeet for the assistant

jeet keeps every repository in one store and does all work in tasks. A task
is a directory with any number of git worktrees in it and a coding agent
running in tmux. Commands that print a path print only that on stdout; jeet
says everything else on stderr.

## Looking

- `jeet task ls` lists active tasks, most recently active first, with what
  each agent is doing. `--archived` lists archived ones.
- `jeet task prs [<task>]` asks GitHub about a task's pull requests and
  lists them; `--offline` lists what is recorded without asking.
- `jeet task path <task>` prints a task's directory.
- `jeet ls` lists the repositories in the store; `jeet prs <repo>` lists a
  repository's open pull requests.
- `jeet worktree ls --task <task>` lists a task's worktrees.
- `jeet projects ls` lists projects: templates a task can start from.

## Starting and steering work

- `jeet task new <name> [--project <p>] [--repo <r>]… [--branch <b>]
  [--prompt "<brief>"]` makes a task. With `--prompt` its agent starts
  straight away with the brief as its first prompt.
- `jeet task send <task> "<message>"` gets a message to a task's agent:
  typed in when it is idle, queued until it stops when it is busy, its first
  prompt when it is not running.
- `jeet task open <task>` starts a task's agent without a brief.
- `jeet worktree add <repo> [--branch <b>] --task <task>` adds a worktree
  to a task.
- `jeet review <pr> --repo <repo>` checks a pull request out into its review
  task and starts the configured review.

## Tidying away

- `jeet task archive <task> --force` frees a task's disk, keeping what
  brings it back; `jeet task unarchive <task>` brings it back.
- `jeet task delete <task> --force` deletes a task and everything in it.
  Only ever when the user asked for that task to go.

## Telling the user

- `jeet notify "<title>" [--body "<text>"] [--url <https url>]` raises a
  desktop notification. Clicking it opens the URL, where it can.

## The todo list

`todo.md` in your home holds the list, most important first. Each item is a
checkbox line; `!p1`, `!p2`… mark its priority and `@task-slug` links it to
a task.

- `jeet todo ls [--json]` lists the items, numbered.
- `jeet todo add "<text>" [-p <1-9>] [--task <task>]…` adds one, in
  priority order.
- `jeet todo done <n>… [--undo]` ticks items off, or puts them back.
- `jeet todo rm <n>…` removes items.

Numbers are positions in the list, so list again after a change before
using them.

## Permissions

Each command that changes something needs a capability the user allows in
`[assistant] allow` in jeet's `config.toml`, such as `task.create`,
`task.send`, `task.archive` or `review.start`. Looking, `jeet notify` and
`jeet todo` need none. A refused command says which capability it needed:
pass that on to the user rather than trying another way.
