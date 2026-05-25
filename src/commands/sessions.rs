use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::context::App;
use crate::db::{RepoRecord, SessionRecord};
use crate::docker;
use crate::git;
use crate::remote;
use crate::resolve;

pub fn create(app: &App, name: &str, filter: Option<&str>, branch: Option<&str>) -> Result<()> {
     if name.contains('/') || name.contains('\\') {
        bail!("session name cannot contain path separators");
         }

    let sessions_root = app.home.join("sessions");
    std::fs::create_dir_all(&sessions_root).context("create sessions root")?;

    let workspace_path = sessions_root.join(name);

    let (repo, default_branch) = if let Some(f) = filter {
        resolve_repo(app, f)?
      } else {
        ephemeral_repo_info()?
       };

    let target_branch = branch.unwrap_or(&default_branch);

    std::fs::create_dir_all(&workspace_path).context("create session workspace dir")?;

    if repo.trunk_path == workspace_path.to_string_lossy() {
        bail!("workspace path conflicts with existing repo path");
         }

    clone_to_workspace(Path::new(&repo.trunk_path), &workspace_path)?;

    checkout_branch(&workspace_path, target_branch)?;

    docker::create_container(name, &workspace_path)?;
    eprintln!("jeet: created container jeet-session-{}", name);

    docker::start_container(name).context("start session container")?;

    let now = std::time::SystemTime::now()
           .duration_since(std::time::UNIX_EPOCH)
           .unwrap_or_default()
           .as_secs() as i64;

    app.db.upsert_session(&SessionRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        repo_id: repo.id.clone(),
        branch: Some(target_branch.to_string()),
        trunk_path: workspace_path.to_string_lossy().to_string(),
        created_at: now,
        status: "running".to_string(),
       })?;

    eprintln!(
           "jeet: session '{}' created for {} (branch: {})",
        name, repo.id, target_branch
      );

    Ok(())
}

pub fn enter(app: &App, name: Option<&str>, interactive: bool) -> Result<()> {
     match name {
         Some(n) => {
            if !docker::is_container_running(n)? {
                eprintln!("jeet: starting container for session '{}'", n);
                docker::start_container(n).context("start container")?;
             }

             let exit_code = docker::exec_into_container(n)?;
             std::process::exit(exit_code);
             }
         None => {
            let sessions = app.db.list_sessions(None)?;

            if sessions.is_empty() {
                bail!("no sessions found");
                 }

            if interactive || sessions.len() > 1 {
                interactive_list(app, &sessions)?;
              } else if sessions.len() == 1 {
                  let session = &sessions[0];
                  eprintln!("jeet: entering single session '{}'", session.name);

                  if !docker::is_container_running(&session.name)? {
                      docker::start_container(&session.name).context("start container")?;
                      }

                  let exit_code = docker::exec_into_container(&session.name)?;
                  std::process::exit(exit_code);
              } else {
                  print_session_list(&sessions)?;

                  eprint!("\nSelect session (1-{}): ", sessions.len());
                  io::stdout().flush().unwrap();

                  let mut input = String::new();
                  io::stdin().read_line(&mut input).context("read selection")?;

                  let idx: usize = input.trim().parse().context("invalid number")?;
                  if idx == 0 || idx > sessions.len() {
                      bail!("invalid session number");
                       }

                  let session = &sessions[idx - 1];
                  enter_session(app, session)?;
              }
             }
         }

     Ok(())
}

pub fn list(app: &App, filter: Option<&str>, interactive: bool) -> Result<()> {
    let sessions = app.db.list_sessions(filter)?;

    if sessions.is_empty() {
        println!("no sessions found");
        return Ok(());
         }

    if interactive || sessions.len() > 1 {
        interactive_list(app, &sessions)?;
      } else {
          println!("{:<20} {:<45} {:<20}", "NAME", "REPO", "BRANCH");
          println!("{}", "-".repeat(85));

          for s in &sessions {
              let repo_id = s.repo_id.clone();
              let branch = s.branch.as_deref().unwrap_or("HEAD");
              println!("{:<20} {:<45} {:<20}", s.name, repo_id, branch);
              }
          }

     Ok(())
}

pub fn rename(app: &App, old_name: &str, new_name: &str) -> Result<()> {
     if new_name.contains('/') || new_name.contains('\\') {
        bail!("session name cannot contain path separators");
         }

     let old_session = app.db.get_session_by_name(old_name)?;
     match old_session {
         None => bail!("session '{}' not found", old_name),
         Some(_) => {}
         }

     if app.db.get_session_by_name(new_name)?.is_some() {
        bail!("session '{}' already exists", new_name);
         }

     docker::stop_container(old_name).context("stop old container")?;
     docker::remove_container(old_name, true).ok();

     let sessions_root = app.home.join("sessions");
     let old_workspace = sessions_root.join(old_name);
     let new_workspace = sessions_root.join(new_name);

     if old_workspace.exists() {
        std::fs::rename(&old_workspace, &new_workspace).context("rename workspace dir")?;
         }

     app.db.delete_session(old_name)?;

     if let Some(session) = old_session {
        let mut updated = session.clone();
        updated.name = new_name.to_string();
        updated.trunk_path = new_workspace.to_string_lossy().to_string();
        app.db.upsert_session(&updated)?;
         }

     eprintln!("jeet: renamed session '{}' -> '{}'", old_name, new_name);

     Ok(())
}

pub fn delete(app: &App, name: &str, force: bool) -> Result<()> {
     let session = app.db.get_session_by_name(name)?;
     match session {
         None => {
             if force {
                 eprintln!("jeet: session '{}' not found (ignoring)", name);
                 return Ok(());
                  }
             bail!("session '{}' not found", name);
             }
         Some(_) => {
             docker::stop_container(name).context("stop container")?;

             let workspace = app.home.join("sessions").join(name);
             if workspace.exists() {
                 std::fs::remove_dir_all(&workspace).context("remove workspace dir")?;
                  }

             app.db.delete_session(name)?;

             eprintln!("jeet: session '{}' deleted", name);
             }
         }

     Ok(())
}

fn clone_to_workspace(trunk_path: &Path, dest: &Path) -> Result<()> {
     let status = Command::new("rsync")
            .args([
                "-a",
                "--exclude=.git/refs/",
                "--exclude=logs/",
                "--exclude=FETCH_HEAD",
                "--exclude=ORIG_HEAD",
            trunk_path.to_str().unwrap(),
                &dest.to_string_lossy(),
            ])
            .status();

     match status {
         Ok(s) if s.success() => {
             let _ = Command::new("git")
                     .args(["-C", &dest.to_string_lossy(), "fetch", "--prune"])
                     .status();
             }
             _ => {
             std::fs::remove_dir_all(dest).ok();
             let status = Command::new("git")
                     .args(["clone", "--mirror", &trunk_path.to_string_lossy(), &dest.to_string_lossy()])
                     .status()
                     .context("clone repo to workspace")?;
             if !status.success() {
                 bail!("git clone failed");
                  }
             }
         }

     Ok(())
}

fn checkout_branch(repo_path: &Path, branch: &str) -> Result<()> {
     let status = Command::new("git")
              .args(["-C", &repo_path.to_string_lossy(), "rev-parse", "--is-shallow-repository"])
              .status()
              .context("check if git repo is valid")?;

      if !status.success() {
          let _ = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "init", "-b", branch])
                   .status();
          let _ = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "config", "user.email", "jeet@example.com"])
                   .status();
          let _ = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "config", "user.name", "jeet"])
                   .status();

          let _ = std::fs::write(repo_path.join(".session-init"), "initialized by jeet");
          let _ = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "add", ".session-init"])
                   .status();
          let _ = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "commit", "-m", "initial commit"])
                   .status();

          return Ok(());
           }

      let status = Command::new("git")
              .args([
                  "-C",
                  &repo_path.to_string_lossy(),
                  "checkout",
                  "-f",
              branch,
                   ])
              .status()
              .context("checkout branch");

     if !status.map(|s| s.success()).unwrap_or(false) {
          for base_branch in &["origin/main", "origin/master"] {
             let output = Command::new("git")
                      .args(["-C", &repo_path.to_string_lossy(), "show-ref", base_branch])
                      .output()
                      .context("check if base branch exists")?;

             if output.status.success() {
                 let status = Command::new("git")
                          .args([
                              "-C",
                              &repo_path.to_string_lossy(),
                              "checkout",
                              "-b",
                            branch,
                            base_branch,
                          ])
                          .status()
                          .context("create branch from remote")?;

                 if status.success() {
                     return Ok(());
                      }
                  }
              }

          let output = Command::new("git")
                   .args(["-C", &repo_path.to_string_lossy(), "branch", "-a"])
                   .output()
                   .context("list branches")?;

          let has_local = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
              line.trim().ends_with(branch) || line.contains(format!("/{}", branch).as_str())
                  });

          if has_local {
              let status = Command::new("git")
                       .args([
                           "-C",
                           &repo_path.to_string_lossy(),
                           "checkout",
                           "-b",
                        branch,
                           ])
                       .status()
                       .context("create new local branch")?;

              if status.success() {
                  return Ok(());
                   }
              }

          bail!("branch '{}' does not exist", branch);
           }

      Ok(())
}

fn resolve_repo(app: &App, filter: &str) -> Result<(RepoRecord, String)> {
    let repo = resolve::resolve_repo_filter(&app.db, filter)?;
    let default_branch = git::default_branch(Path::new(&repo.trunk_path))?;
    Ok((repo, default_branch))
}

fn ephemeral_repo_info() -> Result<(RepoRecord, String)> {
     let identity = remote::parse_remote_url_anyhow("https://example.com/ephemeral/temp")?;
     Ok((
         RepoRecord {
             id: identity.id(),
             trunk_path: String::new(),
             remote_url: "https://example.com/ephemeral/temp".to_string(),
             default_branch: "main".to_string(),
             managed: false,
              },
              "main".to_string(),
         ))
}

fn interactive_list(app: &App, sessions: &[SessionRecord]) -> Result<()> {
     print_session_list(sessions)?;
     println!();
     println!("Enter number to enter session (1-{}), 'r' to rename, 'd' to delete, 'q' to quit:", sessions.len());

     let mut stdout = io::stdout();
     stdout.flush().unwrap();

     let mut input = String::new();
     io::stdin().read_line(&mut input).context("read selection")?;
     let input = input.trim();

     match input.to_lowercase().as_str() {
          "q" | "quit" | "" => Ok(()),
          "r" | "rename" => {
             eprint!("Enter session name to rename: ");
             stdout.flush().unwrap();

             let mut name = String::new();
             io::stdin().read_line(&mut name).context("read name")?;
             let name = name.trim();

             if !name.is_empty() {
                 eprint!("Enter new name: ");
                 stdout.flush().unwrap();

                 let mut new_name = String::new();
                 io::stdin().read_line(&mut new_name).context("read new name")?;
                 let new_name = new_name.trim();

                 if !new_name.is_empty() {
                     rename(app, name, new_name)?;
                      }
                  }
             Ok(())
              }
          "d" | "delete" => {
             eprint!("Enter session name to delete: ");
             stdout.flush().unwrap();

             let mut name = String::new();
             io::stdin().read_line(&mut name).context("read name")?;
             let name = name.trim();

             if !name.is_empty() {
                 eprintln!("Deleting session '{}'...", name);
                 delete(app, name, false)?;
                  }
             Ok(())
              }
          _ => {
             match input.parse::<usize>() {
                  Ok(idx) if idx > 0 && idx <= sessions.len() => {
                      let session = &sessions[idx - 1];
                      enter_session(app, session)
                      }
                      _ => bail!("invalid selection"),
                  }
              }
           }
}

fn enter_session(_app: &App, session: &SessionRecord) -> Result<()> {
      if !docker::is_container_running(&session.name)? {
          eprintln!("jeet: starting container for session '{}'", session.name);
          docker::start_container(&session.name).context("start container")?;
           }

      let exit_code = docker::exec_into_container(&session.name)?;
      std::process::exit(exit_code);
}

fn print_session_list(sessions: &[SessionRecord]) -> Result<()> {
     println!("Sessions:");
     println!("{:<20} {:<45} {:<15}", "NAME", "REPO", "BRANCH");
     println!("{}", "-".repeat(80));

     for s in sessions {
          let branch = s.branch.as_deref().unwrap_or("HEAD");
          println!("{:<20} {:<45} {:<15}", s.name, s.repo_id, branch);
           }

     Ok(())
}
