//! `jeet setup`: the setup agent, in a home of its own
//! (`$JEET_HOME/setup/`) and a tmux session of its own, with
//! `jeet setup apply` running in a pane beside it. It is stored as a task
//! of kind `setup` with the slug `setup`, kept out of the tasks list as the
//! assistant is, and launched as a task's agent is
//! ([`runtime::launch_tmux`]), with settings of its own ([`hooks`]): it
//! reads, and writes only its home.
//!
//! Having no parent, its jeet commands run as the user's, as a top-level
//! task's do: what keeps it from applying is its settings, and that
//! `jeet setup apply` refuses any agent's session.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use super::{setup_dir, PRODUCT};
use crate::agent::AgentSpec;
use crate::cd;
use crate::context::App;
use crate::db::anchors::AnchorKind;
use crate::db::tasks::{RunState, TaskKind, TaskRecord, TaskSessionRecord, TaskState};
use crate::harness::HookSetup;
use crate::identity;
use crate::instructions;
use crate::manager::{self, Role};
use crate::permissions;
use crate::placement::Placement;
use crate::runtime::{self, Launch, Started};
use crate::tasks;
use crate::tmux;
use crate::worktrees::now_secs;

/// The setup agent's task slug, which no other task may take; its tmux
/// session is `jeet-setup`.
pub const SLUG: &str = "setup";

const NAME: &str = "setup";

const AGENTS_MD: &str = include_str!("templates/AGENTS.md");
const SKILL: &str = include_str!("templates/SKILL.md");

/// Where the skill goes in the home.
const SKILL_PATH: &str = ".claude/skills/setup/SKILL.md";

/// Where the templates this jeet ships are copied, for the agent to diff
/// the user's files against.
const BUILTIN: &str = "builtin";

/// `text` with `{name}` filled in from [`PRODUCT`].
fn fill(text: &str) -> String {
    text.replace("{name}", PRODUCT)
}

/// Write each of the setup home's templates that is not in `dir` yet, as
/// [`manager::ensure_files`] does: AGENTS.md, each harness's import of it
/// and the setup skill. Nothing there is ever replaced. `answers.md` is the
/// agent's to write: its absence says this is a first run.
pub fn ensure_files(dir: &Path) -> Result<Vec<&'static str>> {
    instructions::migrate(dir).context("move CLAUDE.md to AGENTS.md")?;
    let imports = instructions::imports()
        .into_iter()
        .map(|i| (i.file, i.text.to_string()));
    let templates = std::iter::once((instructions::AGENTS_MD, fill(AGENTS_MD)))
        .chain(imports)
        .chain(std::iter::once((SKILL_PATH, fill(SKILL))));
    let mut written = Vec::new();
    for (rel, text) in templates {
        let path = dir.join(rel);
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        // `create_new`: another jeet writing it meanwhile wins.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, text.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
                written.push(rel);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("write {}", path.display())),
        }
    }
    Ok(written)
}

/// The built-in templates, by where they go under [`BUILTIN`]: the
/// assistant's files at their target ids (`assistant/AGENTS.md`, …) and
/// those of new homes under `templates/{root,manager,worker}/`.
fn builtin_files() -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    let instructions_only = |rel: &str| {
        rel == instructions::AGENTS_MD
            || (rel.starts_with(".claude/skills/") && rel.ends_with(".md"))
    };
    for (rel, text) in manager::templates(Role::Root) {
        if instructions_only(rel) {
            out.push((format!("assistant/{rel}"), text));
        }
    }
    for (role, dir) in [(Role::Root, "root"), (Role::Manager, "manager")] {
        for (rel, text) in manager::templates(role) {
            if instructions_only(rel) {
                out.push((format!("templates/{dir}/{rel}"), text));
            }
        }
    }
    out.push(("templates/worker/SKILL.md".to_string(), tasks::SKILL));
    out
}

/// Write this jeet's built-in templates to `builtin/` in the setup home,
/// afresh: they are jeet's copy, never the user's.
fn write_builtin(dir: &Path) -> Result<()> {
    let root = dir.join(BUILTIN);
    match std::fs::remove_dir_all(&root) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("remove {}", root.display()))
        }
        _ => {}
    }
    for (rel, text) in builtin_files() {
        let path = root.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// The setup agent's task, made the first time, with its home set up: the
/// templates that are missing and the marker that lets its hooks find it.
pub fn ensure(app: &App) -> Result<TaskRecord> {
    let dir = setup_dir(&app.home);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    ensure_files(&dir)?;
    let task = match app.db.get_setup_task()? {
        Some(task) => task,
        None => create_record(app)?,
    };
    let marker = tasks::marker_path(&dir);
    let marked = std::fs::read_to_string(&marker).is_ok_and(|id| id.trim() == task.id);
    if !marked {
        tasks::write_marker(&dir, &task.id)?;
    }
    Ok(task)
}

fn create_record(app: &App) -> Result<TaskRecord> {
    if let Some(other) = app.db.get_task_by_slug(SLUG)? {
        bail!(
            "task {} has the name `{SLUG}`, which {PRODUCT} keeps for the setup agent; move its \
             work to another task and delete it (`{PRODUCT} task delete {SLUG}`) to use \
             `{PRODUCT} setup`",
            other.name
        );
    }
    let now = now_secs();
    let task = TaskRecord {
        id: Uuid::new_v4().to_string(),
        slug: SLUG.to_string(),
        name: NAME.to_string(),
        project: None,
        kind: TaskKind::Setup,
        state: TaskState::Active,
        created_at: now,
        last_activity: now,
        parent_id: None,
        placement: Placement::Local,
        run_state: RunState::Run,
        run_state_by: None,
        max_children: None,
    };
    match app.db.insert_task(&task) {
        Ok(()) => Ok(task),
        // Another jeet made it first: there is only ever one.
        Err(e) => app.db.get_setup_task()?.ok_or(e),
    }
}

/// The setup agent's settings: it may read anything under the jeet home
/// and `~/.claude`, write only in its own home (`home_entries` are the
/// jeet home's other entries at launch, each denied; one made later is
/// not allowed either, so it prompts), and run jeet's read-only
/// commands and `jeet setup status` without asking. It may not run
/// `jeet setup apply`, push or use `gh`, nor edit the user's global
/// harness, git and shell files. Like all of jeet's rules, a lint: the
/// real control is that only the user's terminal applies.
pub fn hooks(
    exe: PathBuf,
    task_id: &str,
    home: &Path,
    home_entries: &[String],
    branches: &[String],
    wake: bool,
) -> HookSetup {
    let mut allow: Vec<String> = permissions::READ_ONLY
        .iter()
        .map(|c| c.to_string())
        .collect();
    allow.push(format!("{PRODUCT} setup status"));
    let mut deny = permissions::denied_commands(branches);
    for command in [
        format!("{PRODUCT} setup apply"),
        format!("{PRODUCT} setup discard"),
        "git push".to_string(),
        "gh".to_string(),
    ] {
        if !deny.contains(&command) {
            deny.push(command);
        }
    }
    // Claude Code's absolute paths start `//`.
    let abs = |p: &Path| format!("/{}", p.display());
    let own = abs(&setup_dir(home));
    let extra_allow = vec![
        format!("Read({}/**)", abs(home)),
        "Read(~/.claude/**)".to_string(),
        format!("Edit({own}/**)"),
        format!("Write({own}/**)"),
    ];
    // A deny wins over any allow, so "everywhere but its home" is each
    // other place by name.
    let mut elsewhere: Vec<String> = home_entries
        .iter()
        .filter(|e| e.as_str() != "setup")
        .flat_map(|e| {
            let path = abs(&home.join(e));
            [path.clone(), format!("{path}/**")]
        })
        .collect();
    elsewhere.extend(
        [
            "~/.claude/**",
            "~/.claude.json",
            "~/.gitconfig",
            "~/.config/git/**",
            "~/.config/gh/**",
            "~/.ssh/**",
            "~/.zshrc",
            "~/.zprofile",
            "~/.bashrc",
            "~/.bash_profile",
            "~/.profile",
        ]
        .map(str::to_string),
    );
    let mut extra_deny = vec![format!("Bash({PRODUCT} setup apply*)")];
    for path in &elsewhere {
        extra_deny.push(format!("Edit({path})"));
        extra_deny.push(format!("Write({path})"));
    }
    HookSetup {
        exe,
        task_id: task_id.to_string(),
        allow,
        deny,
        extra_allow,
        extra_deny,
        environment: None,
        wake,
    }
}

/// The jeet home's entries the setup agent may not write, by name: what is
/// there now, and the targets of apply whether there or not.
fn home_entries(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = ["config.toml", "assistant", "templates", "backups"]
        .map(str::to_string)
        .to_vec();
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// Start or resume the setup agent and attach to it: its latest session,
/// or with `new` a fresh one. A start opens `jeet setup apply` in a pane
/// beside it.
pub fn run(app: &App, new: bool) -> Result<()> {
    tmux::require()?;
    let spec = AgentSpec::from_config(&app.config)?
        .with_model(app.config.models.for_kind(TaskKind::Setup));
    if !(spec.supports_sessions() && spec.has_hooks() && spec.harness.prompt_args("").is_some()) {
        bail!(
            "the setup agent runs in a harness whose sessions, hooks and first prompt \
             {PRODUCT} keeps track of, such as Claude Code; `{}` lacks them (set `harness` in \
             config.toml)",
            spec.display()
        );
    }
    let task = ensure(app)?;
    let name = runtime::session_name(&task);
    let running = runtime::live_session(&name)? && tmux::agent_alive(&name);
    // Its agent gone, the apply pane keeps the session up: it goes too, so
    // a start can make it again.
    if !running && tmux::has_session(&name) {
        tmux::kill(&name)?;
    }
    if running && new {
        bail!(
            "the setup agent is running in tmux session {name}; end it there (`/exit`) to \
             start a fresh session"
        );
    }
    if !running {
        let started = start(app, &task, spec, new)?;
        for warning in &started.warnings {
            eprintln!("{PRODUCT}: {warning}");
        }
        if started.started == Started::Now {
            eprintln!("{PRODUCT}: started the setup agent in tmux session {name}");
        }
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !terminal {
        eprintln!("{PRODUCT}: `tmux attach -t {name}` to talk to the setup agent");
        return Ok(());
    }
    let code = tmux::attach(&name)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// What the agent is told on a session with no conversation yet.
const FIRST_PROMPT: &str = "Start the setup conversation with section 0 of the setup skill.";

/// Start the setup agent: resuming its latest session unless `new`, with
/// [`hooks`], then the apply pane beside it.
fn start(app: &App, task: &TaskRecord, spec: AgentSpec, new: bool) -> Result<runtime::Running> {
    let dir = setup_dir(&app.home);
    // Only as it starts, not under a running agent reading them.
    write_builtin(&dir)?;
    let mut known = app.db.list_task_sessions(&task.id)?;
    let now = now_secs();
    let record = |id: &str| -> TaskSessionRecord {
        TaskSessionRecord {
            session_id: id.to_string(),
            task_id: task.id.clone(),
            parent_id: None,
            name: NAME.to_string(),
            created_at: now,
            last_activity: now,
        }
    };
    // A fresh id, recorded first so the launch takes it as known.
    let session = match new {
        true => {
            let id = Uuid::new_v4().to_string();
            known.push(record(&id));
            Some(id)
        }
        false => None,
    };
    let resumes = match &session {
        Some(_) => false,
        None => runtime::latest_session(&dir, &known)
            .is_some_and(|id| spec.harness.has_session(&dir, &id)),
    };
    let exe = std::env::current_exe().context("locate the jeet binary")?;
    let hooks = hooks(
        exe.clone(),
        &task.id,
        &app.home,
        &home_entries(&app.home),
        &permissions::default_branches(app)?,
        runtime::wake(app, spec.harness),
    );
    let launch = Launch {
        id: task.id.clone(),
        slug: task.slug.clone(),
        subject: "the setup agent".to_string(),
        name: NAME.to_string(),
        dir: dir.clone(),
        known_sessions: known,
        ungated_warning: false,
        spec,
        hooks: Some(hooks),
        warnings: Vec::new(),
    };
    let prompt = (!resumes).then_some(FIRST_PROMPT);
    let running = runtime::launch_tmux(
        app,
        &launch,
        session.as_deref(),
        prompt,
        |pane| {
            identity::record(
                &app.db,
                &task.id,
                AnchorKind::Pane,
                Some(&pane.id),
                pane.pid,
            )
        },
        |id| app.db.upsert_task_session(&record(id)),
    )?;
    let _ = app.db.touch_task(&task.id, now);
    let mut running = running;
    let home = app.home.to_string_lossy();
    let env = apply_env(&home);
    if let Err(e) = tmux::split_window(&running.tmux_session, &dir, &env, &apply_argv(&exe)) {
        running.warnings.push(format!(
            "could not open the apply pane: {e:#}; run `{PRODUCT} setup apply` in a terminal"
        ));
    }
    Ok(running)
}

/// The apply pane's environment: this jeet home, and blank where the
/// agent's session sets who it runs for, as apply refuses any agent's
/// session.
fn apply_env(home: &str) -> [(&str, &str); 4] {
    [
        // Named, not left to whatever the tmux server's environment says:
        // apply must change this home's files, and open this index.
        ("JEET_HOME", home),
        (tasks::TASK_ID_ENV, ""),
        (permissions::ASSISTANT_ENV, ""),
        (cd::CD_FILE_ENV, ""),
    ]
}

/// `jeet setup apply`, run again each time the user presses Enter.
fn apply_argv(exe: &Path) -> Vec<String> {
    let script = "while :; do \"$1\" setup apply; printf '\\nPress Enter to look for staged \
                  changes again (ctrl-c to close this pane) '; read _ || exit; done";
    vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        exe.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings Claude Code would be given for `hooks`.
    fn settings_of(hooks: &HookSetup) -> serde_json::Value {
        let path = Path::new("/nonexistent/settings.json");
        let harness = crate::harness::by_id("claude-code").unwrap();
        let (contents, _) = harness.hook_file(hooks, path).unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    #[test]
    fn the_templates_name_the_product_once_filled() {
        for text in [fill(AGENTS_MD), fill(SKILL)] {
            assert!(!text.contains("{name}"), "{text}");
            assert!(text.contains(&format!("{PRODUCT} setup apply")));
        }
        assert!(fill(SKILL).starts_with("---\nname: setup\n"));
    }

    #[test]
    fn the_skill_covers_every_notify_setting_and_backend() {
        let skill = fill(SKILL);
        let section = &skill[skill.find("### 9. Notifications").unwrap()..];
        let section = &section[..section.find("### 10.").unwrap()];
        for needle in [
            "`via`",
            "`command`",
            "`max_per_hour`",
            "`[[notify.rules]]`",
            "`terminal`",
            "`[assistant] notify`",
            "`[review] notify`",
            "notify install",
            "`[notify.slack] user`",
            "notify setup slack",
            "`[notify.ntfy]",
            "notify setup ntfy",
        ] {
            assert!(section.contains(needle), "{needle}");
        }
        for backend in crate::notify::BACKENDS {
            assert!(section.contains(&format!("`{backend}`")), "{backend}");
        }
        assert!(!section.contains("has not landed"));
    }

    #[test]
    fn ensure_files_writes_what_is_missing_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let written = ensure_files(dir.path()).unwrap();
        assert!(written.contains(&"AGENTS.md"), "{written:?}");
        assert!(written.contains(&SKILL_PATH), "{written:?}");
        assert!(!dir.path().join("answers.md").exists());
        std::fs::write(dir.path().join("AGENTS.md"), "mine\n").unwrap();
        assert!(ensure_files(dir.path()).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn builtin_copies_are_at_target_ids_apply_takes() {
        let dir = tempfile::tempdir().unwrap();
        write_builtin(dir.path()).unwrap();
        let files = builtin_files();
        for (rel, _) in &files {
            assert!(
                super::super::target_path(Path::new("/h"), rel).is_ok(),
                "{rel} is no target"
            );
            assert!(dir.path().join(BUILTIN).join(rel).is_file(), "{rel}");
        }
        let ids: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        for id in [
            "assistant/AGENTS.md",
            "assistant/.claude/skills/jeet-assistant/SKILL.md",
            "assistant/.claude/skills/jeet-managers/SKILL.md",
            "templates/root/AGENTS.md",
            "templates/manager/AGENTS.md",
            "templates/worker/SKILL.md",
        ] {
            assert!(ids.contains(&id), "{id}: {ids:?}");
        }
        // Rewritten afresh: a stray file goes.
        std::fs::write(dir.path().join(BUILTIN).join("stray.md"), "x").unwrap();
        write_builtin(dir.path()).unwrap();
        assert!(!dir.path().join(BUILTIN).join("stray.md").exists());
    }

    #[test]
    fn the_settings_read_widely_and_write_only_the_home() {
        let home = Path::new("/u/.jeet");
        let entries = home_entries(Path::new("/nonexistent"));
        let hooks = hooks(
            PathBuf::from("/bin/jeet"),
            "t1",
            home,
            &entries,
            &["main".to_string()],
            false,
        );
        let settings = settings_of(&hooks);
        let rules = |key: &str| -> Vec<String> {
            settings["permissions"][key]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let allow = rules("allow");
        let deny = rules("deny");
        for rule in [
            "Read(//u/.jeet/**)",
            "Read(~/.claude/**)",
            "Edit(//u/.jeet/setup/**)",
            "Write(//u/.jeet/setup/**)",
            "Bash(jeet setup status:*)",
            "Bash(jeet ls:*)",
            "Bash(jeet task ls:*)",
        ] {
            assert!(allow.contains(&rule.to_string()), "{rule}: {allow:?}");
        }
        for rule in [
            "Bash(jeet setup apply*)",
            "Bash(jeet setup apply *)",
            "Bash(git push *)",
            "Bash(gh *)",
            "Edit(//u/.jeet/config.toml)",
            "Edit(//u/.jeet/assistant/**)",
            "Write(//u/.jeet/templates/**)",
            "Edit(~/.claude/**)",
            "Edit(~/.zshrc)",
        ] {
            assert!(deny.contains(&rule.to_string()), "{rule}: {deny:?}");
        }
        assert!(
            !deny.iter().any(|d| d.contains("/u/.jeet/setup")),
            "{deny:?}"
        );
        assert!(!allow.iter().any(|a| a.contains("jeet setup apply")));
    }

    #[test]
    fn the_apply_pane_runs_apply_without_the_agents_id() {
        let argv = apply_argv(Path::new("/opt/my jeet"));
        assert_eq!(argv[..2], ["sh", "-c"]);
        assert!(argv[2].contains("\"$1\" setup apply"));
        assert_eq!(argv[4], "/opt/my jeet");
        let env = apply_env("/tmp/h");
        assert!(env.contains(&("JEET_HOME", "/tmp/h")), "{env:?}");
        assert!(env.contains(&(tasks::TASK_ID_ENV, "")));
        assert!(env.contains(&(permissions::ASSISTANT_ENV, "")));
    }
}
