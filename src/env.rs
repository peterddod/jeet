//! Environment variables under the product's two names.
//!
//! Reads look for `PLETE_<name>` first, then `JEET_<name>`, so a session or
//! script that still sets the old name keeps working. Writes (for a child
//! process) are not done here: they name the variable directly.

use std::env::VarError;
use std::ffi::OsString;

const NEW_PREFIX: &str = "PLETE_";
const LEGACY_PREFIX: &str = "JEET_";

/// The part of `name` after the prefix. A full name (`JEET_TASK_ID`, as the
/// `*_ENV` constants hold) is accepted too.
fn suffix(name: &str) -> &str {
    name.strip_prefix(LEGACY_PREFIX)
        .or_else(|| name.strip_prefix(NEW_PREFIX))
        .unwrap_or(name)
}

/// `PLETE_<name>`, else `JEET_<name>`. `NotPresent` only when neither is set;
/// a value that is not unicode is reported as such.
pub fn var(name: &str) -> Result<String, VarError> {
    let name = suffix(name);
    match std::env::var(format!("{NEW_PREFIX}{name}")) {
        Err(VarError::NotPresent) => std::env::var(format!("{LEGACY_PREFIX}{name}")),
        found => found,
    }
}

/// [`var`], without requiring unicode.
pub fn var_os(name: &str) -> Option<OsString> {
    let name = suffix(name);
    std::env::var_os(format!("{NEW_PREFIX}{name}"))
        .or_else(|| std::env::var_os(format!("{LEGACY_PREFIX}{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_name_wins_then_legacy() {
        // Names no other test touches.
        std::env::remove_var("PLETE_ENVTEST_A");
        std::env::set_var("JEET_ENVTEST_A", "old");
        assert_eq!(var("ENVTEST_A").unwrap(), "old");
        assert_eq!(var("JEET_ENVTEST_A").unwrap(), "old");
        std::env::set_var("PLETE_ENVTEST_A", "new");
        assert_eq!(var("ENVTEST_A").unwrap(), "new");
        assert_eq!(var_os("JEET_ENVTEST_A").unwrap(), "new");
        std::env::remove_var("PLETE_ENVTEST_A");
        std::env::remove_var("JEET_ENVTEST_A");
        assert!(matches!(var("ENVTEST_A"), Err(VarError::NotPresent)));
        assert!(var_os("ENVTEST_A").is_none());
    }
}
