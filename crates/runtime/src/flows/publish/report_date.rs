//! Default report date (B12): the calendar date of the run's instant in the
//! effective target timezone (`publish.target_timezone`, overridable with
//! `--timezone`), not the UTC date. A report scheduled at 08:00 Asia/Shanghai
//! must be keyed by the Shanghai date even though UTC is still on the
//! previous day before 08:00.

use time::OffsetDateTime;

use crate::error::RuntimeError;

/// `YYYY-MM-DD` of `now` in the IANA zone `timezone`. A pure function of
/// (instant, zone): callers sample `now` once per run. An unknown zone is an
/// error, never a silent fallback to UTC.
pub fn default_report_date(now: OffsetDateTime, timezone: &str) -> Result<String, RuntimeError> {
    // The parser validate-config and doctor use (R1-P-S14).
    let zone = rss_ai_news_config::validate::parse_target_timezone(timezone).map_err(|error| {
        RuntimeError::Config(format!("invalid IANA timezone {timezone:?}: {error}"))
    })?;
    let instant = jiff::Timestamp::from_nanosecond(now.unix_timestamp_nanos())
        .map_err(|error| RuntimeError::Config(format!("instant {now} out of range: {error}")))?;
    let date = instant.to_zoned(zone).date();
    Ok(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> OffsetDateTime {
        OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[test]
    fn shanghai_after_midnight_is_the_local_date_not_the_utc_date() {
        // 2026-11-21T00:30+08:00 is 2026-11-20T16:30Z.
        let now = at("2026-11-21T00:30:00+08:00");
        assert_eq!(
            default_report_date(now, "Asia/Shanghai").unwrap(),
            "2026-11-21"
        );
        assert_eq!(default_report_date(now, "UTC").unwrap(), "2026-11-20");
    }

    #[test]
    fn negative_offset_zone_can_still_be_on_the_previous_day() {
        // 2026-11-21T03:00Z is 2026-11-20T22:00 in New York (EST, -05:00).
        let now = at("2026-11-21T03:00:00Z");
        assert_eq!(
            default_report_date(now, "America/New_York").unwrap(),
            "2026-11-20"
        );
    }

    #[test]
    fn daylight_saving_offset_comes_from_the_zone_rules() {
        // 2026-07-01T03:30Z is 2026-06-30T23:30 in New York (EDT, -04:00).
        let now = at("2026-07-01T03:30:00Z");
        assert_eq!(
            default_report_date(now, "America/New_York").unwrap(),
            "2026-06-30"
        );
    }

    /// R1-P-S14: jiff's special `Etc/Unknown` behaves like UTC; it must be
    /// refused like any unknown zone, not used.
    #[test]
    fn etc_unknown_is_an_error_not_a_utc_fallback() {
        for zone in ["Etc/Unknown", "etc/unknown"] {
            assert!(
                default_report_date(at("2026-11-21T00:30:00Z"), zone).is_err(),
                "{zone}"
            );
        }
    }

    #[test]
    fn unknown_zone_is_an_error_not_a_utc_fallback() {
        let error =
            default_report_date(at("2026-11-21T00:30:00Z"), "Mars/Olympus_Mons").unwrap_err();
        assert!(error.to_string().contains("Mars/Olympus_Mons"), "{error}");
    }
}
