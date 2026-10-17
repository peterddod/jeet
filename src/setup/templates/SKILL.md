---
name: setup
description: How to run {name}'s setup conversation - the sections to go through in order, the questions to ask in each, and how to stage a change for `{name} setup apply`. Use from the first message of every setup session.
---

# Running {name} setup

## How each section goes

1. Explain the concept in a few sentences, in plain words.
2. Show the current setting: read `$JEET_HOME/config.toml` and the files
   involved, and quote what is set (or say it is the default).
3. Ask the section's questions. Wait for the answers.
4. If something should change, stage it (below) and tell the user to press
   Enter in the apply pane beside this one, then answer `y` or `n` for each
   file it shows (or to run `{name} setup apply` in a terminal of their
   own).
5. Write what they chose, and why, under the section's heading in
   `answers.md`.

Never move on before the user has said what they want. They may skip a
section; note that in `answers.md` too.

## Staging a change

Write the whole new file to `$JEET_HOME/setup/proposed/<target-id>`, and an
entry for it in `$JEET_HOME/setup/proposed/manifest.toml`:

```toml
["config.toml"]
reason = "one line on why"
hash = "<the target's hash now>"   # leave out for a file that does not exist yet
```

Leave `path` out: apply writes where the target id says, and refuses an
entry whose `path` names anywhere else.

The target ids apply accepts, and nothing else:

| Target id | File |
|---|---|
| `config.toml` | `$JEET_HOME/config.toml` |
| `assistant/AGENTS.md` | the assistant's instructions |
| `assistant/.claude/skills/<name>/SKILL.md` | the assistant's skills (`jeet-assistant`, `jeet-managers`) |
| `templates/root/…`, `templates/manager/…` (`.md` only) | overrides for new assistant and manager homes, laid out as the home is |
| `templates/worker/SKILL.md` | override for the skill every new task gets |

- Get the hash from `{name} setup status`: stage the file and an entry
  without `hash`, run `{name} setup status`, and copy the "hash now" it shows
  for the target into the entry. Until it is in, status shows the entry as
  refused ("staged as a new file, but the file exists now"); that is
  expected. Apply refuses an entry whose hash no longer matches the target,
  so stage again after the user edits a file by hand.
- Stage `config.toml` whole: start from the current file and change only what
  the user asked for, keeping their comments, order and formatting. Apply
  checks it loads before it offers it.
- One staged file per target. Restaging a target replaces the earlier
  proposal.
- `{name} setup status` shows what is staged and whether apply would take it.

## Sections

### 0. Hello

- Read `answers.md`, `config.toml` and `{name} setup status`.
- First run (`answers.md` missing and `config.toml` holds only `scan_roots`):
  greet the user and go to the overview.
- Re-run: summarise what is set ("Here's what is set. What do you want to
  change?") and go only to the sections the user picks.
- Proposals left staged from an earlier run: list them, and ask whether to
  keep them (they apply as they are) or discard them (`{name} setup discard`,
  the user's to run).

### 1. Overview

About ten lines on: tasks (a workspace with worktrees and an agent in tmux);
managers and the tree under the assistant; budgets and grants; reviews;
projects; the assistant; watching GitHub and its rules; notifications; and
permissions ({name}'s capabilities, and the Claude Code rules {name} writes for
each agent at launch).

### 2. Repos

- `scan_roots`: where `{name} import` looks for repositories.
- Ask which directories hold their code. Offer `{name} import` (the user runs
  it) to bring repositories into the store.

### 3. Harness and models

- `[harness]`: `command` (the agent's command line) and `auto_trust`
  (answering Claude Code's folder-trust prompt for task directories), and
  `auto_trust_reviews`.
- Models per role (`[models]`: `assistant`, `manager`, `task`, `review`), when
  this {name} has them: check the README or `builtin/` for `[models]` before
  offering it.

### 4. Permissions

- The pain point: what prompts, and what is blocked. Ask what they keep
  approving by hand.
- `[permissions] push = "branches" | "ask"`: whether work tasks push their
  own branches without a prompt (never to a default branch, never a plain
  force-push).
- `classifier_context`: a description of routine work for Claude Code's
  auto-mode classifier.
- `[permissions.task]`, `[permissions.manager]`, `[permissions.assistant]`:
  extra Claude Code rules, `allow` and `deny`, as written.
- Say that deny rules are a lint (`git -C . push -f` gets past them) and
  that branch protection on GitHub is the real control; setup never changes
  it.
- Anything for `~/.claude/settings.json`: advice only.

### 5. PR policy

- `[tasks] pr = "auto" | "draft" | "ask"` (default `draft`), and the
  per-project override, when this {name} has them.

### 6. The assistant

- `[assistant] allow`: go capability by capability, saying what each lets
  the assistant do; nothing is allowed until listed.
- `notify`: a notification for each action it takes.
- `[[assistant.rules]]`: what `{name} watch` does with each kind of GitHub
  event.

### 7. Managers and reviews

- `[managers]`: `default_budget`, `max_depth`, `max_children`, `max_live`,
  `worker_default`, `manager_default`, `held`, `idle_report_mins`.
- `[review]` caps: `max_live`, `time`, `turns`.

### 8. Watching GitHub

- `[watch]`: `repos`, `projects`, `interval_secs`.
- Offer `{name} watch install` (the user runs it) to poll in the background.

### 9. Notifications

{name} notifies the user when a question reaches them, a review is ready, an
agent waits on a startup prompt, the assistant acts, or `{name} watch` has
events for them. Show the current `[notify]`, then ask how they want to hear
and how often they want to be interrupted.

- `via`: the backends each notification goes through. The default is
  `["desktop"]`, and `[]` sends nothing.
  - `desktop`: `terminal-notifier` on macOS when it is installed (a click
    opens the link), else Notification Center; `notify-send` on Linux.
  - `command`: runs `[notify] command`, a program of the user's, with the
    notification in `JEET_NOTIFY_TITLE`, `JEET_NOTIFY_SUBTITLE`,
    `JEET_NOTIFY_BODY` and `JEET_NOTIFY_URL`, e.g. a script that posts to
    their phone or chat. `via` may list both.
- `command`: needed when `via` lists `command`.
- `max_per_hour`: how many are sent in an hour before the rest fold into one
  "N more" (default 6; 0 never folds).
- `[[notify.rules]]`: the first rule that matches decides, and a
  notification no rule matches is sent. Each list left out matches
  anything:
  - `kind`: `question`, `blocked`, `review`, `startup-prompt`, `action`,
    `grant`, `message`, or a GitHub event's kind;
  - `level`: `assistant`, `manager`, `task` or `review`;
  - `task`: task slugs;
  - `action`: `"notify"` or `"ignore"`.

  Offer a rule only for what the user says is noise, e.g. ignoring the
  assistant's `action` notifications.
- `terminal`: the terminal `{name} open` starts when no {name} screen is open:
  `none`, a name {name} knows (`ghostty`, `terminal`, …), or a command its
  command line is appended to (`"wezterm start --"`). Leave it unset unless
  they care.
- The switches beside it: `[assistant] notify` (one per action the assistant
  takes) and `[review] notify` (a review being ready).
- Offer, for the user to run: `{name} notify install`, which keeps a sender
  running in the background, and `{name} notify install-link`, so that a
  click on a notification opens the task.
- `JEET_NOTIFY_CMD` in the environment replaces `desktop` for one process. If
  the user already sets it in their shell's rc, suggest moving it to
  `via = ["command"]`, as advice for them to act on. Don't add it.

Stage the result in `config.toml` as a `[notify]` table, with any
`[[notify.rules]]` after it. Apply refuses a `via` name {name} has no backend
for, and `command` listed without `[notify] command`.

### 10. Instructions

- The assistant's `AGENTS.md` and skills, and the template overrides for new
  homes (`templates/…`). This is what an agent's "system prompt" is.
- Drift: compare each of the user's files with its copy in `builtin/`
  (`builtin/assistant/…`, `builtin/templates/…`). Where the built-in one is
  newer, show the difference and offer it. Keep the user's edits unless they
  accept the new text.

### 11. Wrap-up

- List what was applied (`applied.log`, `{name} setup status`) and what is
  still staged.
- Say which agents to restart to pick up settings changes: apply lists them;
  the user runs `{name} task restart <task>` for each.
