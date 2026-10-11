//! What a node may spend: busy time and turns. A budget is written as parts,
//! `4h`, `30m` or `20turns`, joined by commas (`1h,20turns`); a change to one
//! is `+` or `=` before them (`+1h`, `=2h,40turns`). Cost budgets wait on
//! #26.

// Parsed for `[managers] default_budget` now; `--budget` and `manager
// budget` are the callers still to come.
#![allow(dead_code)]

use anyhow::{bail, Result};

/// A node's caps; `None` is no cap on that unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub secs: Option<i64>,
    pub turns: Option<i64>,
}

/// One part of a budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetPart {
    Secs(i64),
    Turns(i64),
}

/// A change to a budget: raise each unit given by so much, or set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add(Limits),
    Set(Limits),
}

const FORMS: &str = "hours (4h), minutes (30m) or turns (20turns)";

/// One part: `<n>h`, `<n>m` or `<n>turns`, with `n` at least 1. A `$`
/// amount is refused: cost budgets wait on #26.
pub fn parse_part(s: &str) -> Result<BudgetPart, String> {
    let s = s.trim();
    if s.contains('$') {
        return Err(format!("{s}: cost budgets wait on #26; give {FORMS}"));
    }
    let (digits, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let per = match unit {
        "h" => 3600,
        "m" => 60,
        "turns" => 1,
        _ => return Err(format!("{s}: not a budget; give {FORMS}")),
    };
    let n: i64 = match digits.parse() {
        Ok(n) => n,
        Err(_) => return Err(format!("{s}: not a budget; give {FORMS}")),
    };
    if n == 0 {
        return Err(format!("{s}: a budget of 0 allows nothing"));
    }
    let Some(n) = n.checked_mul(per) else {
        return Err(format!("{s}: too large"));
    };
    match unit {
        "turns" => Ok(BudgetPart::Turns(n)),
        _ => Ok(BudgetPart::Secs(n)),
    }
}

/// The caps `parts` give, refusing a unit given twice (`1h,30m`).
pub fn limits(parts: &[BudgetPart]) -> Result<Limits> {
    let mut out = Limits::default();
    for part in parts {
        let (slot, n, unit) = match *part {
            BudgetPart::Secs(n) => (&mut out.secs, n, "time"),
            BudgetPart::Turns(n) => (&mut out.turns, n, "turns"),
        };
        if slot.is_some() {
            bail!("the budget gives {unit} twice");
        }
        *slot = Some(n);
    }
    Ok(out)
}

/// Comma-separated parts, as in config and on the command line.
pub fn parse_limits(s: &str) -> Result<Limits, String> {
    let parts = s
        .split(',')
        .map(parse_part)
        .collect::<Result<Vec<_>, _>>()?;
    limits(&parts).map_err(|e| format!("{s}: {e}"))
}

/// A change to a budget: `+` raises each unit given by so much (`+1h`,
/// `+50turns`), `=` sets it (`=2h`).
pub fn parse_change(s: &str) -> Result<Change, String> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        return parse_limits(rest).map(Change::Add);
    }
    if let Some(rest) = s.strip_prefix('=') {
        return parse_limits(rest).map(Change::Set);
    }
    Err(format!(
        "{s}: start a change with + to raise the budget (+1h) or = to set it (=2h)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_part() {
        assert_eq!(parse_part("4h"), Ok(BudgetPart::Secs(4 * 3600)));
        assert_eq!(parse_part(" 30m "), Ok(BudgetPart::Secs(1800)));
        assert_eq!(parse_part("20turns"), Ok(BudgetPart::Turns(20)));
    }

    #[test]
    fn refuses_cost_zero_and_anything_else() {
        let err = parse_part("$5").unwrap_err();
        assert!(err.contains("#26"), "{err}");
        assert!(parse_part("5$").unwrap_err().contains("#26"));
        assert!(parse_part("0h").unwrap_err().contains("0"));
        assert!(parse_part("0turns").is_err());
        for bad in [
            "",
            "h",
            "4",
            "4d",
            "4 h",
            "-1h",
            "+1h",
            "1.5h",
            "4hours",
            "99999999999999999999h",
        ] {
            assert!(parse_part(bad).is_err(), "{bad}");
        }
        assert!(parse_part("9223372036854775807h")
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn limits_refuse_a_unit_given_twice() {
        assert_eq!(limits(&[]).unwrap(), Limits::default());
        assert_eq!(
            limits(&[BudgetPart::Secs(3600), BudgetPart::Turns(20)]).unwrap(),
            Limits {
                secs: Some(3600),
                turns: Some(20)
            }
        );
        let err = limits(&[BudgetPart::Secs(3600), BudgetPart::Secs(60)]).unwrap_err();
        assert!(err.to_string().contains("time twice"), "{err}");
        let err = limits(&[BudgetPart::Turns(1), BudgetPart::Turns(2)]).unwrap_err();
        assert!(err.to_string().contains("turns twice"), "{err}");
        assert!(parse_limits("1h,30m").is_err());
        assert_eq!(
            parse_limits("1h,20turns"),
            Ok(Limits {
                secs: Some(3600),
                turns: Some(20)
            })
        );
    }

    #[test]
    fn parses_changes() {
        assert_eq!(
            parse_change("+1h"),
            Ok(Change::Add(Limits {
                secs: Some(3600),
                turns: None
            }))
        );
        assert_eq!(
            parse_change("+50turns"),
            Ok(Change::Add(Limits {
                secs: None,
                turns: Some(50)
            }))
        );
        assert_eq!(
            parse_change("=2h,10turns"),
            Ok(Change::Set(Limits {
                secs: Some(7200),
                turns: Some(10)
            }))
        );
        for bad in ["1h", "+", "=", "+$5", "+0h", "+1h,+1h", "=1h,2h", "-1h"] {
            assert!(parse_change(bad).is_err(), "{bad}");
        }
        assert!(parse_change("1h").unwrap_err().contains("start a change"));
    }
}
