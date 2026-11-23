//! UTC civil time without a date library: enough for `recorded_at`, the
//! `<utc>` file stamp and the `YYYY/MM` publication directories.

/// (year, month, day, hour, minute, second) in UTC for Unix seconds.
pub fn civil(unix: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// ISO 8601 UTC, e.g. `2026-11-23T19:24:05Z`.
pub fn iso8601(unix: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// File stamp, e.g. `2026-11-23_192405Z`.
pub fn file_stamp(unix: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix);
    format!("{y:04}-{mo:02}-{d:02}_{h:02}{mi:02}{s:02}Z")
}

/// (`YYYY`, `MM`) publication directory names.
pub fn year_month(unix: u64) -> (String, String) {
    let (y, mo, ..) = civil(unix);
    (format!("{y:04}"), format!("{mo:02}"))
}

/// Seconds since the Unix epoch now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2026-11-23T19:24:05Z
        assert_eq!(iso8601(1_795_461_845), "2026-11-23T19:24:05Z");
        assert_eq!(file_stamp(1_795_461_845), "2026-11-23_192405Z");
        // Leap day 2024-02-29T12:00:00Z
        assert_eq!(iso8601(1_709_208_000), "2024-02-29T12:00:00Z");
        assert_eq!(
            year_month(1_709_208_000),
            ("2024".to_string(), "02".to_string())
        );
    }
}
