# Adding a harness

A *harness* is everything jeet knows about one coding agent. Claude Code's is
`src/harness/claude.rs`. Adding another (OpenCode, Goose, …) should take one
self-contained PR: a module, a line in the registry, fixtures and tests. No
other part of jeet should need to change, because the rest of jeet sees only
the `Harness` trait (`src/harness/mod.rs`) and its neutral events.

## What to implement

Create `src/harness/<name>.rs` with a unit struct that implements `Harness`,
and add it to `HARNESSES` in `src/harness/mod.rs`. Only the first three
methods are required. Every other method defaults to "not supported", and
jeet does without that feature:

| Method | What it is for | When it is unsupported |
| --- | --- | --- |
| `id` | the name for `harness = "…"` in config.toml | (required) |
| `name` | the name shown in messages | (required) |
| `default_command` | the binary run when config.toml names no command | `None`: a command has to be configured |
| `runs_program` | recognises `agent = "/path/to/bin"` without a `harness` key | the user sets `harness` |
| `tested_version` | the latest version you tested; older ones get a warning | no version check |
| `new_session_args(id, name)` | starts a session under an id jeet picks | no recorded sessions, no resume, no assistant |
| `resume_args(id)` | resumes a recorded session | as above |
| `prompt_args(prompt)` | gives the interactive session its first prompt (after every other argument) | `task new --prompt`, and `task send` to an agent that is not running, fail with a clear message |
| `hook_args(&HookSetup)` | installs jeet's hooks for this launch only, plus permission for the commands in `HookSetup::allow` (for a task: `jeet worktree`, `jeet ls`, `jeet report`) and, when the harness enforces rules, refusal of the commands in `HookSetup::deny` (see [Deny rules](#deny-rules)) | no status, `task send` types straight in |
| `hook_file(&HookSetup, path)` | `hook_args` read from a file instead: what to write at `path` and the arguments naming it (Claude Code: `--settings <path>`). jeet writes it 0600 under `$JEET_HOME/run/<task id>/`, outside the task, at each launch, so the hooks and rules cost the command line, which shares tmux's 16KB limit with the first prompt, only the path; it goes with the task at archive and delete | `hook_args` on the command line |
| `enforcement` | how far the harness enforces `HookSetup::deny`: `Enforcement::Rules` when its own permission rules refuse those commands (a `Sandbox` level waits for the `local-sandbox` placement) | `None`: deny rules are dropped, a parented task launches with a warning that its agent is not stopped from force-pushing or merging, and (phase 2) `jeet manager new` refuses the harness, naming "deny rules" |
| `interrupt_keys` | the tmux keys that end the running turn and keep the session (Claude Code: `Escape`); jeet presses them once, only while the agent is running or waiting on a prompt a hook reported since its launch (never at a startup prompt), and only when `has_draft` sees no unsent text (and, while waiting or while `in_dialog` says it shows a dialog of its own, no one is attached to the session, who may be typing into the prompt). A harness with these keys needs `hook_args` too, or no hook ever reports and nothing is pressed, and `has_draft`, or the keys are pressed over what the user is typing | `None`: `jeet task interrupt` refuses, and pausing a subtree (phase 2) waits for the turn to end |
| `in_dialog` | whether a status that reads as working is the agent showing the user a dialog of its own (Claude Code: a PreToolUse for `AskUserQuestion` or `ExitPlanMode`, which, with no hook for the answer, stays the status until the next tool call or the end of the turn), for `interrupt_keys` to leave alone while someone is attached | `false`: only a waiting status counts as a prompt |
| `parse_hook(event, raw)` | turns a hook's stdin into a `HookEvent` | – |
| `continue_output(reason)` | what a turn-end hook prints so the agent carries on with queued messages | queued messages are typed in once it is idle |
| `session_context_output(context)` | what a session-start hook prints to add context (the assistant's summary) | the assistant starts without its summary |
| `last_messages(dir, id, n)` | reads a session's last `n` messages and how its last turn ended (finished, interrupted, an API error, still running) from the agent's own record | `jeet task last` says the agent's record cannot be read |
| `has_session`, `sessions`, `copy_sessions`, `latest_activity` | read the agent's own session store for a directory | sessions are only what jeet recorded; `jeet import` copies none |
| `startup_prompt(pane)`, `accept_trust(pane)` | spot a prompt that blocks the agent before its first hook, and accept a trust prompt one key at a time | the task shows `needs input – startup prompt?` after 15s |
| `instructions_file`, `instructions_import` | the file the agent reads its instructions from, and, when that is not `AGENTS.md`, what jeet writes in it to import `AGENTS.md` (Claude Code: `CLAUDE.md` holding `@AGENTS.md`) | none of jeet's instruction files is written for it |

`AgentSpec::supports_sessions` and `AgentSpec::has_hooks` check whether these
methods return `Some`, so they must return `Some` for any arguments, empty
ones included.

### Hooks

jeet's hook command is `jeet task-hook --task <id> <Event>`, with the event's
payload on stdin; `<id>` is `HookSetup::task_id`, the task jeet launched the
agent for, and only a hook that names it, and runs as a descendant of the
process jeet starts in the task's pane, reports to the task's parent (jeet
identifies callers by process ancestry; the agent's own `jeet` commands are
held to the same, so a harness that runs hooks or shell tools from a
separate daemon gets status but no reports, and gated commands refused). For
any harness except Claude Code, add `--harness <id>` so that `task-hook`
hands the payload to your `parse_hook`. A hook has to be quick,
must print nothing on stdout except the outputs above, and always exits 0.
`task-hook` takes care of that: `parse_hook` only reads the payload.

If the agent can take hooks only from a file, not a flag or an environment
variable, the launch arguments alone cannot install them. Likewise, if it
cannot start a session under an id jeet picks, jeet has to learn the id
from the first hook instead of recording it before launch. Both need a
change to the trait, so raise them in the PR.

### Mapping hook payloads to events

`parse_hook` returns a `HookEvent`: the harness's own event `name` (kept in
the status file), the `session_id`, the session's `cwd` (used to find the
task), and a `kind`. Map each event the agent sends to the `EventKind` that
describes it, and use `None` for events jeet does not use:

| `EventKind` | Meaning | Status shown | Claude Code event |
| --- | --- | --- | --- |
| `SessionStarted(source)` | a session began; `source` is `Startup`, `Resume`, `Clear`, `Compact` or `Other` | `idle` | `SessionStart` |
| `Working(detail)` | busy; `detail` is the prompt's first line or the tool in use | `running – <detail>` | `UserPromptSubmit`, `PreToolUse`, `SubagentStop` |
| `Idle` | sitting at its prompt without a turn having just ended | `idle`; the queue is typed in | `Notification` with `idle_prompt` |
| `NeedsInput(detail)` | stopped mid-turn for a permission or a question | `needs input – <detail>` | `Notification` with `permission_prompt` or `elicitation_dialog` (or no type); others are ignored |
| `Stopped` | a turn finished | `idle`; the queue is handed over | `Stop` |
| `Ended { cleared }` | the session is over; `cleared` when a new one replaces it in the same window, as `/clear` does | `ended` | `SessionEnd` (`cleared` when its `reason` is `clear`) |

Cut details with `status::first_line(text, status::DETAIL_MAX)`. A
`Clear` or `Other` start records the new session as following on from the
task's previous one. A `Compact` start keeps whatever state the session was
in. Map to `NeedsInput` only what blocks until the user answers: an
informational notification mapped there tells a parent its task waits on
the user when it does not.

For a task with a parent (one the assistant made), some events also become
`status` reports to the parent: `NeedsInput` (once until the prompt is
answered), `Ended` unless `cleared`, and `Idle` for longer than `[managers]
idle_report_mins`, which `jeet watch` and the assistant's own Stop hook look
for. The same task's `SessionStarted` prints its brief through
`session_context_output`; a harness that returns `None` there gets none.
Both need the hook to carry `--task <id>` (`hook_args`) and to run as a
descendant of the process jeet starts in the task's pane: jeet reports and
briefs only when `--task` names the task found and jeet confirms, by walking
the hook's process ancestry up to that recorded process, that it is that
task's agent. The environment is only a hint. Status needs only `--task` or
the cwd, so a hook run by a plugin host, daemon or server outside the
pane's process tree still shows status, and idle reports (swept from
status) still go out, but it silently never sends the other reports or
briefs; run hooks as children of the agent. The same holds for the agent's
shell tool: its `jeet report`, `jeet worktree add` and other gated jeet
commands are refused ("jeet can't confirm this is task X's agent") unless
they run under the pane's process. Only what the harness knows (the `clear` reason, say) belongs in its module:
the reporting itself is jeet's.

## Deny rules

`HookSetup::deny` lists commands the agent must not run. jeet fills it for
the assistant and for every task with a parent; top-level tasks the user
made get none, so a user's own `git push --force-with-lease` keeps working.
The `[review] command` of a review task the assistant starts is the
exception: it is the user's own command, run by `jeet review-job` rather
than launched through a harness, so it gets no deny rules, and `jeet
review` warns when it starts one.
The list is:

- force pushes (`--force*`, `-f`, a `+` refspec, `--mirror`), `--all`
  and its git 2.46 synonym `--branches`, and pushes naming a default
  branch: `main`, `master` and the default branches of the repos jeet has
  indexed, up to eight in all, the ones most repos use first, as a
  destination (`git push *
  main`, `*:main`, and `*heads/main`, which covers `refs/heads/main` and
  `heads/main` with or without a source);
- merging outside `jeet pr merge`: `gh pr merge`, the REST merge endpoints
  (`pulls/*/merge`, and `repos/*/merges`, which merges branches) through
  `gh api` or `curl`, matched where the path ends there (followed by a
  space or nothing, so `repos/acme/mergesort/pulls` or
  `contents/docs/merges.md` still run), and the GraphQL `mergePullRequest`,
  `enablePullRequestAutoMerge`, `mergeBranch` and `enqueuePullRequest`
  mutations (any `gh api` or `curl` command naming one, flags before
  `graphql` included);
- for a parented task only, the commands of each capability it does not
  hold: `gh pr comment` and `gh issue comment` without `pr.comment`,
  `gh pr review` without `pr.review`. A held capability's commands are
  not denied, but still go to the user's permission prompt (a task's
  allow list stays `jeet worktree`, `jeet ls` and `jeet report`). The assistant is never denied these: it holds them by
  default, and at worst the user approves them at the prompt. Only these
  commands are denied, not the same through `gh api` or `curl` (see
  below).

A repo's default branch is what jeet recorded when it cloned or imported
it: `origin/HEAD`, or, without one, the branch checked out then, which may
be a feature branch. That name is then denied as a push destination in
every repo, to the assistant and every parented task; a push the agent
cannot make, say so, and push it yourself.

`jeet pr merge` runs `gh` from jeet's own process, so the merge denies do
not touch it.

**Pattern syntax.** Each entry is neutral, not any harness's own rule
syntax: words matched from the start of the command, where `*` matches any
run of characters, and the command may carry further arguments after the
pattern. A harness renders each entry in its own syntax. Claude Code's is
`Bash(<pattern> *)`, or `Bash(<pattern>)` when the pattern already ends in
`*`. Claude Code lets a trailing ` *` match the bare command too, but only
when it is the rule's one `*`, so a pattern with a `*` of its own also gets
`Bash(<pattern>)`, for the bare command. So `gh pr merge` becomes
`Bash(gh pr merge *)`, `git push --force*` stays `Bash(git push --force*)`,
and `git push * main` becomes `Bash(git push * main)` and
`Bash(git push * main *)`: with only the second, `git push origin main`
would get through. Allow rules keep the `Bash(<prefix>:*)` form.
Claude Code checks deny rules from `--settings` before any allow rule from
any scope, and a denied command cannot be approved at the prompt. jeet
leaves the `deny` key out when the list is empty.

Patterns are anchored at the start of the command on purpose: an
unanchored `*gh pr merge*` would also refuse `git commit -m "… gh pr merge
…"` or `rg 'gh pr merge'`, with no way for the user to approve them.

**What deny rules do not do.** They are a lint against honest mistakes, not
a gate. They do not catch:

- global options before the subcommand: `git -C . push -f`,
  `git -c a=b push --force`, `gh pr --repo o/r merge 3`;
- a command inside another: `sh -c 'git push --force'`, `xargs`, `env`;
- combined short flags (`git push -uf`), abbreviated long options
  (`git push x --mirr`), and git or shell aliases;
- a push whose destination is implied rather than named: a bare
  `git push` whose upstream is a default branch, and `git push origin HEAD`
  or `@` (or a bare push with `push.default=current`) while on one;
- a quoted refspec (`"HEAD:main"`), and a wildcard one
  (`'refs/heads/*:refs/heads/*'`);
- a GraphQL merge mutation read from a file
  (`gh api graphql -F query=@merge.graphql`, `--input body.json`):
  denying those would refuse read queries kept in files too;
- a REST merge endpoint quoted or carrying a query string
  (`curl -X PUT "https://api.github.com/repos/o/r/pulls/3/merge"`,
  `gh api 'repos/o/r/merges'`): matching what follows `merge` would refuse
  `repos/acme/mergesort/pulls` too, and quoted variants are not listed;
- the default branches of indexed repos past the eight listed;
- moving a branch through the REST refs endpoint
  (`gh api -X PATCH repos/o/r/git/refs/heads/main`): denying it would also
  refuse reading the ref;
- committing through the API: the REST contents endpoint
  (`gh api -X PUT repos/o/r/contents/README.md`, which commits to the
  default branch when no `branch` is given) and the GraphQL
  `createCommitOnBranch` mutation: denying them would refuse reading files,
  or committing to the task's own branch;
- commenting and reviewing through the API, for a parented task without
  `pr.comment` or `pr.review`: `gh api repos/o/r/issues/3/comments -f
  body=…`, `gh api -X POST repos/o/r/pulls/3/reviews -f event=APPROVE`, and
  the GraphQL `addComment` and `addPullRequestReview` mutations: denying
  them would refuse reading comments and reviews too, so these still reach
  the user's prompt;
- scripts the agent writes and runs, MCP tools, and `!` commands typed in
  the session (only the agent's Bash tool is checked);
- anything outside the agent's own tools: a deny on `Edit` covers Claude
  Code's file tools, not a shell that writes the same file.

The rules are a snapshot taken at launch: a repo indexed later, or a
capability granted later, takes effect the next time the agent starts. The
real control on a default branch is GitHub branch protection or a ruleset;
set one. A hard boundary (writes confined to the task's directory, no
`tmux` or `sqlite3`) is a placement with a boundary: a cloud or local
sandbox, designed but not built ([agent-hierarchy.md §11](design/agent-hierarchy.md#11-placement)).

`permissions.rs` has a table of commands that must and must not be denied,
checked against a model of Claude Code's matching (`claude_match`), and a
`KNOWN_GAPS` list pinning the bypasses above. The model is not Claude Code
itself, so check these by hand when you raise `tested_version` (last done
for Claude Code 2.1.296):

- a rule ending in ` *` matches the bare command (`gh pr merge`);
- a rule with another `*` before its trailing ` *` does not match the bare
  command (`Bash(git push * main *)` lets `git push origin main` through),
  and the extra `Bash(git push * main)` denies it, while
  `git push origin main-fix` still runs;
- a `*` in the middle of a rule matches (`gh api repos/o/r/pulls/3/merge`);
- `gh pr merge` is refused outright, with no prompt to approve it;
- Esc while the agent is running ends the turn, and the transcript records
  `[Request interrupted by user`;
- Esc at a permission prompt refuses the tool call and ends the turn.

## Fixtures and tests

- **Hook payloads**: put one real payload per event in
  `tests/fixtures/hooks/<id>/<Event>.json`, captured from the agent (log
  stdin from a hook) and trimmed of anything private. In your module, test
  that every installed event maps to the right `EventKind`, and through
  `status::status_for` to the right state and detail (see
  `each_event_leaves_the_session_in_its_state` in `claude.rs`). Also test that
  unknown events and payloads with no session id say nothing.
- **Panes**: if the agent has a startup prompt, capture it with
  `tmux capture-pane -p` in each state (nothing selected yet, accept selected,
  no prompt) into `tests/fixtures/panes/<id>/`. Test that `accept_trust` never
  returns a confirming key unless the accept option is the one selected.
- **Launch arguments**: test each `*_args` method, and that the hook arguments
  install every event with the jeet binary path shell-quoted, render
  `HookSetup::deny` in the harness's syntax, and leave deny rules out when
  the list is empty. Check by hand that the keys `interrupt_keys` returns
  end a turn in the real agent.
- **Integration**: `tests/integration.rs` runs jeet against a stub `tmux`
  that records each call (`stub_tmux`, `calls_of`) and feeds hooks through
  `task-hook` (`StoreLab::hook`). Add a test that `jeet task open` with
  `JEET_AGENT=<your binary>` launches the arguments you expect.

Then run `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test`.
Record the version you tested in `tested_version`, and add the harness to the
README's harness section.
