//! The local time zone for `os.date` and `os.time`.
//!
//! Stable `std` can't read the local zone. Until the ADR-7 decision on how
//! to read it (docs/design/chunks-io-os-cli.md, "os"), local time is UTC,
//! which is also what the zone is on the CI and cloud machines.

/// The local zone's rules at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    /// Seconds east of UTC.
    pub offset: i64,
    pub isdst: bool,
    /// Abbreviation for `%Z`.
    pub zone: &'static str,
}

/// The local zone at `utc` seconds since the epoch.
pub fn at(_utc: i64) -> Local {
    Local {
        offset: 0,
        isdst: false,
        zone: "UTC",
    }
}

/// The instant whose local time is `local` seconds since the epoch.
pub fn from_local(local: i64) -> i64 {
    local - at(local).offset
}
