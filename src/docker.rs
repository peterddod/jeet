use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const DOCKER_PREFIX: &str = "jeet-session-";

/// Create a new docker container for a session
pub fn create_container(name: &str, volume_path: &Path) -> Result<String> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);
    let volume_path_abs = volume_path
         .canonicalize()
         .context("resolve session volume path")?;

      // Check if container already exists
    let existing = get_container_id(name)?;
    if existing.is_some() {
        bail!("container already exists: {}", container_name);
     }

      // Create the container (not start it yet)
    let status = Command::new("docker")
         .args([
             "create",
             "--name",
             &container_name,
             "-v",
             &format!("{}:/workspace:rw", volume_path_abs.display()),
             "debian:bookworm",
             "tail",
             "-f",
             "/dev/null",
         ])
         .status()
         .context("spawn docker create")?;

    if !status.success() {
        bail!("docker create failed for container {}", container_name);
     }

    Ok(container_name)
}

/// Get container ID by name (without prefix)
fn get_container_id(name: &str) -> Result<Option<String>> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);
    let output = Command::new("docker")
         .args([
             "inspect",
             "-f",
             "{{.Id}}",
             &container_name,
         ])
         .output()
         .context("spawn docker inspect")?;

    if output.status.success() {
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(Some(id))
      } else {
        Ok(None)
     }
}

/// Start a running session container
pub fn start_container(name: &str) -> Result<()> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);

    let status = Command::new("docker")
         .args(["start", &container_name])
         .status()
         .context("spawn docker start")?;

    if !status.success() {
        bail!("docker start failed for container {}", container_name);
     }

    Ok(())
}

/// Check if a container is running
pub fn is_container_running(name: &str) -> Result<bool> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);

    let output = Command::new("docker")
         .args([
             "inspect",
             "--format",
             "{{.State.Running}}",
             &container_name,
         ])
         .output()
         .context("spawn docker inspect")?;

    if output.status.success() {
        let running = String::from_utf8_lossy(&output.stdout).trim() == "true";
        Ok(running)
      } else {
        Ok(false)
     }
}

/// Enter a container and run bash interactively
pub fn exec_into_container(name: &str) -> Result<i32> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);

    eprintln!("jeet: entering session {} ({})", name, container_name);

    let status = Command::new("docker")
         .args(["exec", "-it", &container_name, "bash"])
         .status()
         .context("spawn docker exec")?;

    Ok(status.code().unwrap_or(1))
}

/// Stop and remove a container
pub fn stop_container(name: &str) -> Result<()> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);

      // Try to stop first (may already be stopped)
    let _status = Command::new("docker")
         .args(["stop", "-t0", &container_name])
         .status()
          .ok();

      // Remove the container
    let remove_status = Command::new("docker")
         .args(["rm", &container_name])
         .status()
         .context("spawn docker rm")?;

    if !remove_status.success() {
        bail!("docker rm failed for container {}", container_name);
      }

    Ok(())
}

/// Remove a container (optionally deletes workspace volume)
pub fn remove_container(name: &str, delete_volume: bool) -> Result<()> {
    let container_name = format!("{}{}", DOCKER_PREFIX, name);

     // Try to stop first if running
    let _status = Command::new("docker")
         .args(["stop", "-t0", &container_name])
         .status()
          .ok();

      // Remove the container
    let _ = Command::new("docker")
         .args(["rm", "-f", &container_name])
          .status();

    if delete_volume {
          // Delete the workspace directory
        let sessions_root = std::env::var("JEET_HOME")
              .map(|h| PathBuf::from(h))
              .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".jeet"));
        let workspace = sessions_root.join("sessions").join(name);
        let _ = std::fs::remove_dir_all(workspace);
     }

    Ok(())
}

/// List all session containers
#[allow(dead_code)]
pub fn list_containers() -> Result<Vec<String>> {
    let output = Command::new("docker")
         .args([
             "ps",
             "-a",
             "--filter",
             &format!("name=^{}", DOCKER_PREFIX),
             "--format",
             "{{.Names}}",
         ])
         .output()
         .context("spawn docker ps")?;

    if !output.status.success() {
        return Ok(Vec::new());
      }

    let names = String::from_utf8_lossy(&output.stdout)
          .lines()
          .filter(|l| !l.is_empty())
          .map(|l| l.trim_start_matches(DOCKER_PREFIX).to_string())
          .collect();

    Ok(names)
}

/// Get a session's workspace path from the container mount
#[allow(dead_code)]
pub fn get_session_workspace(name: &str, host_home: &Path) -> Result<PathBuf> {
    let volume_path = host_home.join("sessions").join(name);
    Ok(volume_path)
}

