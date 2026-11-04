//! Environment variables under the product's two names.
//!
//! Reads of a `JEET_<x>` (or `PLETE_<x>`) name look for `PLETE_<x>` first,
//! then `JEET_<x>`, so a session or script that still sets the old name keeps
//! working. Any other name (`EDITOR`, `HOME`) is read as it is. Writes for a
//! child process go through [`set_both`], so the child sees either name.

use std::env::VarError;
use std::ffi::OsString;
use std::process::Command;

const NEW_PREFIX: &str = "PLETE_";
const LEGACY_PREFIX: &str = "JEET_";

/// The part of `name` after the product prefix, if it has one.
fn suffix(name: &str) -> Option<&str> {
    name.strip_prefix(LEGACY_PREFIX)
        .or_else(|| name.strip_prefix(NEW_PREFIX))
}

/// `PLETE_<x>`, else `JEET_<x>`, for a name `JEET_<x>` or `PLETE_<x>`; any
/// other name is read as is. `NotPresent` only when none is set.
pub fn var(name: &str) -> Result<String, VarError> {
    let Some(name) = suffix(name) else {
        return std::env::var(name);
    };
    match std::env::var(format!("{NEW_PREFIX}{name}")) {
        Err(VarError::NotPresent) => std::env::var(format!("{LEGACY_PREFIX}{name}")),
        found => found,
    }
}

/// [`var`], without requiring unicode.
pub fn var_os(name: &str) -> Option<OsString> {
    let Some(name) = suffix(name) else {
        return std::env::var_os(name);
    };
    std::env::var_os(format!("{NEW_PREFIX}{name}"))
        .or_else(|| std::env::var_os(format!("{LEGACY_PREFIX}{name}")))
}

/// Both names of `name` (`JEET_<x>` or `PLETE_<x>`), as `[new, old]`; a
/// name with neither prefix is just itself.
pub fn both_names(name: &str) -> Vec<String> {
    match suffix(name) {
        Some(x) => vec![format!("{NEW_PREFIX}{x}"), format!("{LEGACY_PREFIX}{x}")],
        None => vec![name.to_string()],
    }
}

/// Set `name` for `cmd` under both of its names, so an outer `PLETE_<x>`
/// cannot shadow a `JEET_<x>` given here, nor the reverse.
pub fn set_both(cmd: &mut Command, name: &str, value: impl AsRef<std::ffi::OsStr>) {
    for n in both_names(name) {
        cmd.env(n, value.as_ref());
    }
}

/// Name for `cmd` the homes it runs in: `JEET_HOME` and `PLETE_HOME` for the
/// index home, and `PLETE_NEW_HOME` when new item directories go elsewhere.
pub fn set_home(cmd: &mut Command, home: &std::path::Path, new_home: Option<&std::path::Path>) {
    set_both(cmd, "JEET_HOME", home);
    if let Some(new_home) = new_home {
        cmd.env(NEW_HOME_VAR, new_home);
    }
}

/// The variable that names where new item directories go, beside an
/// explicit home.
pub const NEW_HOME_VAR: &str = "PLETE_NEW_HOME";

/// Remove `name` from `cmd`'s environment under both of its names.
pub fn remove_both(cmd: &mut Command, name: &str) {
    for n in both_names(name) {
        cmd.env_remove(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_name_wins_then_legacy() {
        // Names no other test touches.
        std::env::remove_var("PLETE_ENVTEST_A");
        std::env::set_var("JEET_ENVTEST_A", "old");
        assert_eq!(var("JEET_ENVTEST_A").unwrap(), "old");
        std::env::set_var("PLETE_ENVTEST_A", "new");
        assert_eq!(var("JEET_ENVTEST_A").unwrap(), "new");
        assert_eq!(var_os("PLETE_ENVTEST_A").unwrap(), "new");
        std::env::remove_var("PLETE_ENVTEST_A");
        std::env::remove_var("JEET_ENVTEST_A");
        assert!(matches!(var("JEET_ENVTEST_A"), Err(VarError::NotPresent)));
        assert!(var_os("JEET_ENVTEST_A").is_none());
    }

    #[test]
    fn other_names_are_read_as_they_are() {
        std::env::set_var("ENVTEST_PLAIN", "plain");
        std::env::set_var("JEET_ENVTEST_PLAIN", "wrong");
        assert_eq!(var("ENVTEST_PLAIN").unwrap(), "plain");
        assert_eq!(var_os("ENVTEST_PLAIN").unwrap(), "plain");
        assert_eq!(both_names("ENVTEST_PLAIN"), ["ENVTEST_PLAIN"]);
        std::env::remove_var("ENVTEST_PLAIN");
        std::env::remove_var("JEET_ENVTEST_PLAIN");
        assert!(var("ENVTEST_PLAIN").is_err());
    }

    #[test]
    fn a_child_gets_both_names() {
        let mut cmd = Command::new("true");
        set_both(&mut cmd, "JEET_HOME", "/h");
        let envs: Vec<_> = cmd.get_envs().collect();
        let h = Some(std::ffi::OsStr::new("/h"));
        // Listed by name.
        assert_eq!(
            envs,
            [
                (std::ffi::OsStr::new("JEET_HOME"), h),
                (std::ffi::OsStr::new("PLETE_HOME"), h),
            ]
        );
        remove_both(&mut cmd, "JEET_HOME");
        assert!(cmd.get_envs().all(|(_, v)| v.is_none()));
    }
}
