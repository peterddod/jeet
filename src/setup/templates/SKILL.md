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
4. If something should change, stage it (below) and tell the user to type
   `y` for it in the apply pane beside this one (or to run
   `{name} setup apply` in a terminal of their own).
5. Write what they chose, and why, under the section's heading in
   `answers.md`.

Never move on before the user has said what they want. They may skip a
section; note that in `answers.md` too.

## Staging a change

Write the whole new file to `$JEET_HOME/setup/proposed/<target-id>`, and an
entry for it in `$JEET_HOME/setup/proposed/manifest.toml`:

```toml
["config.toml"]
path = "~/.jeet/config.toml"
reason = "one line on why"
hash = "<the target's hash now>"   # leave out for a file that does not exist yet
```

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
  for the target into the entry. Apply refuses an entry whose hash no longer
  matches the target, so stage again after the user edits a file by hand.
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

This section waits on a `[notify]` config schema that has not landed yet.
For now cover only:

- `JEET_NOTIFY_CMD`, a command of the user's own that receives each
  notification (`JEET_NOTIFY_TITLE`, `_SUBTITLE`, `_BODY`, `_URL`); it is an
  environment variable, so it goes in their shell's rc, as advice.
- The on/off switches: `[assistant] notify`.

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
