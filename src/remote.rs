use anyhow::{bail, Result};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl RepoIdentity {
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.repo)
    }
}

#[derive(Debug, Error)]
pub enum RemoteParseError {
    #[error("unsupported or unrecognised remote URL: {0}")]
    Unrecognised(String),
}

pub fn parse_remote_url(url: &str) -> Result<RepoIdentity, RemoteParseError> {
    let url = url.trim();
    let url = url.strip_suffix(".git").unwrap_or(url);

    if let Some(rest) = url.strip_prefix("git@") {
        return parse_scp_like(rest);
    }

    if url.starts_with("ssh://") {
        let rest = url.strip_prefix("ssh://").unwrap();
        let (auth_host, path) = rest
            .split_once('/')
            .ok_or_else(|| RemoteParseError::Unrecognised(url.to_string()))?;
        let host = auth_host
            .rsplit_once('@')
            .map(|(_, h)| h)
            .unwrap_or(auth_host);
        let host = host.split(':').next().unwrap_or(host);
        return parse_host_owner_repo(host, path);
    }

    if url.starts_with("https://") || url.starts_with("http://") {
        let without_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap();
        let (host, path) = without_scheme
            .split_once('/')
            .ok_or_else(|| RemoteParseError::Unrecognised(url.to_string()))?;
        return parse_host_owner_repo(host, path);
    }

    Err(RemoteParseError::Unrecognised(url.to_string()))
}

fn parse_scp_like(rest: &str) -> Result<RepoIdentity, RemoteParseError> {
    let (host, path) = rest
        .split_once(':')
        .ok_or_else(|| RemoteParseError::Unrecognised(rest.to_string()))?;
    parse_host_owner_repo(host, path)
}

fn parse_host_owner_repo(host: &str, path: &str) -> Result<RepoIdentity, RemoteParseError> {
    let path = path.trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(RemoteParseError::Unrecognised(path.to_string()));
    }
    let repo = parts[parts.len() - 1].to_string();
    let owner = parts[..parts.len() - 1].join("/");
    Ok(RepoIdentity {
        host: host.to_string(),
        owner,
        repo,
    })
}

pub fn parse_remote_url_anyhow(url: &str) -> Result<RepoIdentity> {
    parse_remote_url(url).map_err(|e| anyhow::anyhow!(e))
}

pub fn identity_from_id(id: &str) -> Result<RepoIdentity> {
    let parts: Vec<&str> = id.split('/').collect();
    if parts.len() < 3 {
        bail!("invalid repo id (expected host/owner/repo): {id}");
    }
    let repo = parts[parts.len() - 1].to_string();
    let owner = parts[parts.len() - 2].to_string();
    let host = parts[..parts.len() - 2].join("/");
    Ok(RepoIdentity { host, owner, repo })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_github() {
        let id = parse_remote_url("https://github.com/acme/widget.git").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }

    #[test]
    fn parses_git_scp() {
        let id = parse_remote_url("git@github.com:acme/widget.git").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }

    #[test]
    fn parses_gitlab_https() {
        let id = parse_remote_url("https://gitlab.com/group/subgroup/myrepo").unwrap();
        assert_eq!(id.host, "gitlab.com");
        assert_eq!(id.owner, "group/subgroup");
        assert_eq!(id.repo, "myrepo");
        assert_eq!(id.id(), "gitlab.com/group/subgroup/myrepo");
    }

    #[test]
    fn parses_ssh_url() {
        let id = parse_remote_url("ssh://git@github.com/acme/widget").unwrap();
        assert_eq!(id.id(), "github.com/acme/widget");
    }
}
