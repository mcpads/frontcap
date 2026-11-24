use serde::{Deserialize, Serialize};

/// Virtual time since the generation's time origin, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VirtualTime(pub u64);

/// A span of virtual time in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VirtualDuration(pub u64);

impl VirtualTime {
    pub const ZERO: VirtualTime = VirtualTime(0);

    /// Converts an engine timestamp in milliseconds (relative to the time origin) to microseconds,
    /// rounding to the nearest microsecond; negative inputs clamp to zero.
    pub fn from_engine_ms(ms: f64) -> VirtualTime {
        if ms.is_nan() || ms <= 0.0 {
            return VirtualTime::ZERO;
        }
        VirtualTime((ms * 1000.0).round() as u64)
    }

    pub fn saturating_add(self, d: VirtualDuration) -> VirtualTime {
        VirtualTime(self.0.saturating_add(d.0))
    }

    /// Time elapsed since `earlier`; zero if `earlier` is later.
    pub fn since(self, earlier: VirtualTime) -> VirtualDuration {
        VirtualDuration(self.0.saturating_sub(earlier.0))
    }
}

impl VirtualDuration {
    pub fn from_ms(ms: u64) -> VirtualDuration {
        VirtualDuration(ms.saturating_mul(1000))
    }

    pub fn from_micros(us: u64) -> VirtualDuration {
        VirtualDuration(us)
    }

    pub fn as_micros(self) -> u64 {
        self.0
    }
}

/// `ms` after the Unix epoch as UTC, `2000-01-01T00:00:00.500Z`: the page's `Date` the clock gives.
pub fn utc_iso(ms: u64) -> String {
    // Days to the civil date (proleptic Gregorian), H. Hinnant's `civil_from_days`.
    let days = (ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let in_day = ms % 86_400_000;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", in_day / 3_600_000, in_day / 60_000 % 60, in_day / 1000 % 60, in_day % 1000)
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
