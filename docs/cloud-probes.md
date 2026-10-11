# Cloud and wake-channel probes

Manual checks that decide what phase 2 builds. The cloud placement
(`docs/design/agent-hierarchy.md` §12) and the watcher wake channel (§13)
rest on behaviour that cannot be tested offline. Run these before either is
built. #33 needs none of them.

Each probe gives its steps, the expected result, what it decides and its
cost. Probes marked **needs Q5** start or message cloud sessions, which
bills the account they run under: do not run them until the user grants
permission (Q5, §14). Run every cloud probe under the account and
`CLAUDE_CONFIG_DIR` that cloud nodes would use (Q2), never the user's own
login, and on a scratch repo with no secrets.

Write each result here, under the probe, with the date and the `claude`
version (`claude --version`).

## Cloud runtime (P1–P9)

Setup for P1–P8: a scratch GitHub repo with the Claude GitHub App installed
on it alone, a local clone, and `export CLAUDE_CONFIG_DIR=<probe dir>`
logged into the probe account.

### P1: a session id without a terminal

**Needs Q5.** Cost: one short session.

1. In the clone, run `claude --cloud "Reply with OK and stop." </dev/null
   >out 2>err; echo $?`.
2. Run `claude -p "Reply with OK and stop." --cloud --output-format json
   </dev/null >out.json`, if the CLI accepts `-p` for a new session.
3. Look for a session id and URL in `out`, `err` and `out.json`.

Expected: a parseable session id in at least one form, exit 0.
Decides: how `start` records a node. If neither prints one, the fallback is
a routine `/fire` per node tier, which returns `claude_code_session_id` but
wraps its text as untrusted and allows 30 fires an hour per routine.

### P2: credentials inside the VM

**Needs Q5.** Cost: one session.

1. Start a session and ask it to list its environment variable names, the
   files under `~/.claude*` and `~/.config`, and whether `claude` is on its
   `PATH`.
2. Ask it to try `claude -p "hi" --cloud` and to start a new session.

Expected: no credential that can create or message sessions; both
attempts fail.
Decides: whether jeet is the only way to spawn. If a node can create or
message sessions itself, depth, fan-out and `max_live` (5.2) cannot be
enforced, and the §8 table must say so.

### P3: messaging a reclaimed VM

**Needs Q5.** Cost: one session, left idle.

1. Start a session (P1), let it finish, and leave it idle past the point its
   VM is reclaimed (an hour or more).
2. Run `claude -p "Reply with OK." --cloud <id> --output-format json`.
3. Archive the session on claude.ai and send again.

Expected: the first send reprovisions and is answered; the send to the
archived session fails with a distinct error.
Decides: whether managers can be message-driven (§12.1), and how jeet
detects archiving.

### P4: where a session can push

**Needs Q5.** Cost: one session.

1. Install the App on a second scratch repo the session is not attached to.
2. Ask the session to push a branch to its own repo, then to the second
   repo by URL, then to delete a branch and push a tag.

Expected: only the push to the attached repo succeeds.
Decides: whether a cloud node is confined to its attached repo (§12.6).

### P5: the actor GitHub records

**Needs Q5.** Cost: one session (reuse P4's).

1. After P4, read the push event and the branch's commits through
   `gh api repos/<owner>/<repo>/events` and the commit's author and
   committer.

Expected: one identity for every session (the App or the user).
Decides: what rulesets can key on, and confirms that per-node git gating is
impossible (§8, "Same, for direct git/`gh` actions").

### P6: connectors from the CLI

Cost: none (no session), unless step 2 needs one.

1. Look for a CLI command or settings file that lists the account's
   connectors.
2. If none, start one session (**needs Q5**) and ask it which connectors
   and MCP tools it has.

Expected: either a CLI way to list them, or the session's own list.
Decides: whether `jeet cloud doctor` can check the account for connectors
or must ask the user to attest it (§12.6).

### P7: permission mode in the cloud

**Needs Q5.** Cost: one session.

1. Start a session with `--permission-mode acceptEdits` (and, separately,
   the mode jeet would use) and a prompt that edits a file and runs a shell
   command.
2. Watch claude.ai for a permission prompt.

Expected: the mode is honoured and nothing waits on a prompt.
Decides: whether cloud nodes can stall on prompts only claude.ai shows. If
they can, the §8 table gains that gap and nodes must be told to avoid
prompting tools.

### P8: the base image

**Needs Q5.** Cost: one session.

1. Ask a session to run `python3 --version`, `openssl version` and
   `command -v git sh`.

Expected: python3 and openssl present.
Decides: whether the shim can be POSIX sh plus python3 and compute MACs
in the VM (§12.1, Q3).

### P9: device hooks and Remote Control

Later options, not dependencies. Cost: none for step 1; **needs Q5** for
step 2.

1. Find what the internal hook field `cloud: "device"` does: whether a hook
   marked so runs on the user's machine for a cloud session.
2. Check whether Remote Control lets a local session message a cloud
   session.

Expected: either could give a callback without the mailbox.
Decides: nothing in phase 2; recorded for a later, simpler channel.

## Wake channel (P-W1–P-W10)

The spike for §13. It runs on the user's machine with local Claude Code
sessions, in a scratch directory outside `~/.jeet`, with a throwaway
`--settings` file. It bills ordinary local usage only. Cost: about an
afternoon in all.

### P-W1: the message wire format

1. Start a session with a `SessionStart` hook that saves
   `CLAUDE_CODE_MESSAGING_SOCKET` and the token to a file.
2. From a hook child, connect, send the `{"type":"auth"}` line, then try
   candidate message lines.
3. Note what the receiving session shows and what the model sees.

Expected: one line format that delivers, shown as `› Message from …`.
Decides: whether W1 and W3 are possible at all.

### P-W2: a long-lived hook child keeps own-child status

1. Add a `SessionStart` entry with `async: true` running a script that
   posts every 10 minutes for an hour.
2. Leave `crossSessionInbound` unset.

Expected: every post is delivered, including after the first hour, on
macOS.
Decides: W1.

### P-W3: a non-child poster

1. From a process outside the session (not a hook child), post with the
   saved token, once with the session's `crossSessionInbound: "accept"` and
   once with the default.

Expected: delivered under `accept` only.
Decides: W3.

### P-W4: repo settings that refuse

1. In the session's directory, set `crossSessionInbound: "refuse"` in
   `.claude/settings.local.json`, then post as in P-W2.

Expected: the post is rejected with something the poster can detect.
Decides: how jeet notices a refused post and falls back to typing.

### P-W5: `asyncRewake` mid-turn, and the inbox watch

1. With a `FileChanged` hook (`asyncRewake: true`) and `watchPaths` from
   `SessionStart`, append to the watched file during a long busy turn.
2. Note whether the message lands mid-turn or at Stop.
3. Rename a new file over the watched path (as `Inbox::replace` does), then
   delete it, and append again.

Expected: whether it lands mid-turn, and whether the watch survives a
rename and a delete.
Decides: W2.

### P-W6: a wake while a prompt is open

1. Leave the session at a permission prompt, then at an AskUserQuestion.
2. Post (W1) and rewake (W2).

Expected: the message waits; it never answers the prompt.
Decides: delivery routing while a node is `NeedsInput`.

### P-W7: `--bg` sessions

1. Start a `--bg` session with the same hooks.
2. Check that hooks run, that the socket binds, and that a post wakes it
   once it has stopped.

Expected: all three.
Decides: G4 for local nodes without a pane.

### P-W8: async hook lifetime

1. With the P-W2 hook running, end the session by `/exit`, by
   `tmux kill-session` (on a test tmux server, `tmux -L probe`), and by `-p`
   teardown. Check whether the hook child is still alive.
2. Find the largest `timeout` an `asyncRewake` hook accepts.

Expected: the child dies in each case, or the pid check catches it within
one poll.
Decides: orphan handling, and confirms the long-lived `asyncRewake` waiter
stays rejected.

### P-W9: how a child treats a manager's command as a peer message

1. Post a short instruction to a working session as a peer message, then
   the same instruction typed.
2. Repeat with three different instructions.

Expected: a note of whether it complies, pushes back or ignores each.
Decides: the 5.5 framing: which direction may use peer messages.

### P-W10: throttling under a burst

1. Post 20 messages in 10 seconds, some identical, then 20 batched with a
   nonce each.

Expected: which are dropped or delayed.
Decides: the batch size and the nonce (§13, "Required safeguards").

## Identity under a sandbox (P-I1, P-I2)

For `local-sandbox`, which is design only. Cost: none.

### P-I1: `proc_pidinfo` from inside Seatbelt

1. Run a shell under a Seatbelt profile that excludes jeet's binary (jeet
   runs outside it), and from it run jeet, which walks the shell's ancestry.
2. Repeat with jeet itself under the profile.

Expected: the walk works with jeet outside the sandbox, and may fail
inside it.
Decides: that a sandboxed node must reach jeet outside its sandbox.

### P-I2: Linux `hidepid` and bwrap pid namespaces

1. Mount `/proc` with `hidepid=2` and run the walk as another user.
2. Run an agent under `bwrap --unshare-pid` and call jeet from inside it.

Expected: pids differ inside the namespace, so jeet must run outside it and
see the real pids.
Decides: how a Linux `local-sandbox` exposes jeet.

## The live end-to-end test (phase 2)

**Needs Q5.** Cost: one session per run.

Phase 2 adds an `#[ignore]` integration test that runs only with
`JEET_CLOUD_LIVE=1`: it starts one cloud node, waits for a signed envelope
on its mailbox branch, verifies it, and sends a `<jeet-result>` back. It is
not part of `cargo test` and never runs in CI.
