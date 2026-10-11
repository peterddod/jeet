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
| `hook_args(&HookSetup)` | installs jeet's hooks for this launch only, plus permission for the commands in `HookSetup::allow` | no status, `task send` types straight in |
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

jeet's hook command is `jeet task-hook <Event>`, with the event's payload on
stdin. For any harness except Claude Code, add `--harness <id>` so that
`task-hook` hands the payload to your `parse_hook`. A hook has to be quick,
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
| `NeedsInput(detail)` | stopped mid-turn for a permission or a question | `needs input – <detail>` | other `Notification`s |
| `Stopped` | a turn finished | `idle`; the queue is handed over | `Stop` |
| `Ended` | the session is over | `ended` | `SessionEnd` |

Cut details with `status::first_line(text, status::DETAIL_MAX)`. A
`Clear` or `Other` start records the new session as following on from the
task's previous one. A `Compact` start keeps whatever state the session was
in.

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
  install every event with the jeet binary path shell-quoted.
- **Integration**: `tests/integration.rs` runs jeet against a stub `tmux`
  that records each call (`stub_tmux`, `calls_of`) and feeds hooks through
  `task-hook` (`StoreLab::hook`). Add a test that `jeet task open` with
  `JEET_AGENT=<your binary>` launches the arguments you expect.

Then run `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test`.
Record the version you tested in `tested_version`, and add the harness to the
README's harness section.
