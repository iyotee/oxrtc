//! `api.output_memory` (oxrtc only, M5 item D (6), D45): the process-wide
//! memory budget of the readers that write over HTTP or a WebSocket
//! (`oxrtc_core::memory`). A share of the memory available to the process
//! (`"12.5%"`, the default), or a size (`256`, in MiB as the other `_mib`
//! keys count, or `"256MiB"`, `"2GiB"`).

use oxrtc_core::memory::BudgetSize;
use serde_norway::Value;

use super::{ConfigError, MAX_MIB};

/// What `api.output_memory` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMemory {
    /// A share of the memory available to the process, in thousandths
    /// (`12.5%` is 125), from 1 to 1000.
    PerMille(u16),
    /// A size in MiB, from 1 to [`MAX_MIB`].
    Mib(u64),
}

impl OutputMemory {
    /// The budget's size.
    pub fn size(self) -> BudgetSize {
        match self {
            Self::PerMille(per_mille) => BudgetSize::Fraction(f64::from(per_mille) / 1000.0),
            Self::Mib(mib) => BudgetSize::Bytes(mib.saturating_mul(1024 * 1024)),
        }
    }
}

const EXPECTED: &str =
    "a share of memory from 0.1% to 100% (\"12.5%\") or a size of 1 to 4096 MiB (256, \"256MiB\", \"2GiB\")";

/// Reads the key's value; `None` for an empty one (the default).
pub(crate) fn parse(value: &Value, path: &str) -> Result<Option<OutputMemory>, ConfigError> {
    let invalid = || ConfigError::Invalid(format!("{path}: expected {EXPECTED}"));
    let mib = |n: u64| {
        if (1..=u64::try_from(MAX_MIB).unwrap_or(u64::MAX)).contains(&n) {
            Ok(Some(OutputMemory::Mib(n)))
        } else {
            Err(invalid())
        }
    };
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => mib(n.as_u64().ok_or_else(invalid)?),
        Value::String(text) => {
            let text = text.trim();
            if let Some(percent) = text.strip_suffix('%') {
                let percent: f64 = percent.trim().parse().map_err(|_| invalid())?;
                let per_mille = (percent * 10.0).round();
                if !(1.0..=1000.0).contains(&per_mille) || !percent.is_finite() {
                    return Err(invalid());
                }
                return Ok(Some(OutputMemory::PerMille(per_mille as u16)));
            }
            let (number, unit) = match (text.strip_suffix("GiB"), text.strip_suffix("MiB")) {
                (Some(n), _) => (n, 1024),
                (None, Some(n)) => (n, 1),
                (None, None) => (text, 1),
            };
            let number: u64 = number.trim().parse().map_err(|_| invalid())?;
            mib(number.checked_mul(unit).ok_or_else(invalid)?)
        }
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn read(yaml: &str) -> Result<Option<OutputMemory>, ConfigError> {
        parse(&serde_norway::from_str::<Value>(yaml).unwrap(), "api.output_memory")
    }

    #[test]
    fn a_share_or_a_size() {
        assert_eq!(read("\"12.5%\"").unwrap(), Some(OutputMemory::PerMille(125)));
        assert_eq!(read("\"100%\"").unwrap(), Some(OutputMemory::PerMille(1000)));
        assert_eq!(read("\"0.1 %\"").unwrap(), Some(OutputMemory::PerMille(1)));
        assert_eq!(read("256").unwrap(), Some(OutputMemory::Mib(256)));
        assert_eq!(read("\"256MiB\"").unwrap(), Some(OutputMemory::Mib(256)));
        assert_eq!(read("\"2GiB\"").unwrap(), Some(OutputMemory::Mib(2048)));
        assert_eq!(read("").unwrap(), None);
        assert_eq!(OutputMemory::PerMille(125).size(), BudgetSize::Fraction(0.125));
        assert_eq!(OutputMemory::Mib(2).size(), BudgetSize::Bytes(2 * 1024 * 1024));
    }

    #[test]
    fn anything_else_is_refused_with_what_is_expected() {
        for bad in [
            "0", "5000", "\"0%\"", "\"101%\"", "\"-5%\"", "\"NaN%\"", "\"5GiB\"", "\"lots\"", "true", "[1]", "\"12.5\"",
        ] {
            let Err(ConfigError::Invalid(said)) = read(bad) else { panic!("{bad} taken") };
            assert!(said.starts_with("api.output_memory: expected a share of memory"), "{said}");
        }
    }
}
