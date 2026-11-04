//! Backends' tokens, in the OS keyring (the `keyring` crate: the Keychain
//! on macOS, the Secret Service elsewhere). `plete notify setup` writes one;
//! only the sender reads one, through [`for_sender`], which refuses in any
//! process plete takes for an agent's.

use std::path::Path;

use anyhow::{bail, Result};

use super::Tokens;

/// The keyring services plete's tokens are kept under, new name and old.
const NEW_SERVICE: &str = "plete";
const LEGACY_SERVICE: &str = "jeet";
/// The service tokens are written to; both are read.
const SERVICE: &str = NEW_SERVICE;

/// The first secret `lookup` finds under the new service, else the old one.
/// A secret found under the old one is handed to `copy` to keep under the
/// new, best effort: the old item stays.
fn first_found(
    mut lookup: impl FnMut(&str) -> Result<Option<String>>,
    mut copy: impl FnMut(&str) -> Result<()>,
) -> Result<Option<String>> {
    for service in [NEW_SERVICE, LEGACY_SERVICE] {
        if let Some(secret) = lookup(service)? {
            if service != NEW_SERVICE {
                let _ = copy(&secret);
            }
            return Ok(Some(secret));
        }
    }
    Ok(None)
}

/// Where secrets are kept: the OS keyring, or a map in tests.
pub trait Keyring {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
}

/// The OS keyring.
pub struct OsKeyring;

impl Keyring for OsKeyring {
    fn get(&self, account: &str) -> Result<Option<String>> {
        first_found(
            |service| {
                let entry = keyring::Entry::new(service, account)?;
                match entry.get_password() {
                    Ok(secret) => Ok(Some(secret)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            },
            |secret| self.set(account, secret),
        )
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        keyring::Entry::new(SERVICE, account)?.set_password(secret)?;
        Ok(())
    }
}

/// The keyring account of `backend`'s token for the plete home `home`: one
/// per home, so a second home never sends with the first one's token.
pub fn account(backend: &str, home: &Path) -> String {
    format!("{backend}@{}", home.display())
}

/// Tokens read from a keyring, for the plete home `home`.
pub struct KeyringTokens {
    keyring: Box<dyn Keyring>,
    home: std::path::PathBuf,
}

impl KeyringTokens {
    pub fn new(keyring: Box<dyn Keyring>, home: &Path) -> KeyringTokens {
        KeyringTokens {
            keyring,
            home: home.to_path_buf(),
        }
    }
}

impl Tokens for KeyringTokens {
    fn token(&self, backend: &str) -> Result<Option<String>> {
        self.keyring.get(&account(backend, &self.home))
    }
}

/// What an agent's process is given in place of tokens: a refusal.
pub struct Refused;

impl Tokens for Refused {
    fn token(&self, _: &str) -> Result<Option<String>> {
        bail!("plete reads no token in an agent's process; run the sender from your own terminal or with `plete notify install`")
    }
}

/// The tokens the sender reads: the OS keyring's, unless this process is
/// one plete takes for an agent's (`agent`), which gets none.
pub fn for_sender(home: &Path, agent: bool) -> Box<dyn Tokens> {
    match agent {
        true => Box::new(Refused),
        false => Box::new(KeyringTokens::new(Box::new(OsKeyring), home)),
    }
}

/// An in-memory keyring.
#[cfg(test)]
#[derive(Default)]
pub struct MemKeyring(pub std::cell::RefCell<std::collections::BTreeMap<String, String>>);

#[cfg(test)]
impl Keyring for MemKeyring {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.borrow().get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0
            .borrow_mut()
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_service_is_read_first_then_the_old_and_copied() {
        let found = |services: &[(&str, &str)]| {
            let copied = std::cell::RefCell::new(Vec::new());
            let got = first_found(
                |service| {
                    Ok(services
                        .iter()
                        .find(|(s, _)| *s == service)
                        .map(|(_, v)| v.to_string()))
                },
                |secret| {
                    copied.borrow_mut().push(secret.to_string());
                    Ok(())
                },
            )
            .unwrap();
            (got, copied.into_inner())
        };
        assert_eq!(
            found(&[("jeet", "old")]),
            (Some("old".into()), vec!["old".into()])
        );
        assert_eq!(found(&[("plete", "new")]), (Some("new".into()), vec![]));
        assert_eq!(
            found(&[("jeet", "old"), ("plete", "new")]),
            (Some("new".into()), vec![])
        );
        assert_eq!(found(&[]), (None, vec![]));
        // A failure is not read as absent, and a failed copy does not fail the read.
        assert!(first_found(|_| bail!("locked"), |_| Ok(())).is_err());
        let got = first_found(
            |s| Ok((s == "jeet").then(|| "old".to_string())),
            |_| bail!("no"),
        );
        assert_eq!(got.unwrap().as_deref(), Some("old"));
        assert_eq!(SERVICE, NEW_SERVICE);
    }

    #[test]
    fn tokens_are_kept_per_home_and_an_agent_reads_none() {
        let keyring = MemKeyring::default();
        keyring
            .set(&account("slack", Path::new("/a")), "xoxb-a")
            .unwrap();
        let tokens = KeyringTokens::new(Box::new(keyring), Path::new("/a"));
        assert_eq!(tokens.token("slack").unwrap().as_deref(), Some("xoxb-a"));
        assert_eq!(tokens.token("ntfy").unwrap(), None);
        let other = KeyringTokens::new(Box::new(MemKeyring::default()), Path::new("/b"));
        assert_eq!(other.token("slack").unwrap(), None);
        assert!(for_sender(Path::new("/a"), true).token("slack").is_err());
    }
}
