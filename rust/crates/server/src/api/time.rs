//! `ActiveSupport::TimeZone#parse` for API filter parameters.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// Parses a date or date-time as `tz.parse` would: an explicit offset wins, otherwise the
/// time is local to `tz`, and a bare date is its midnight. Returns UTC.
#[must_use]
pub fn parse_in_zone(value: &str, tz: Tz) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(value) {
        return Some(t.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S %z",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M:%S%z",
    ] {
        if let Ok(t) = DateTime::parse_from_str(value, format) {
            return Some(t.with_timezone(&Utc));
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(value, f).ok())
    .or_else(|| {
        ["%Y-%m-%d", "%d.%m.%Y", "%Y/%m/%d"]
            .iter()
            .find_map(|f| NaiveDate::parse_from_str(value, f).ok())
            .map(|d| d.and_time(NaiveTime::MIN))
    })?;
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

/// `time.end_of_day` in `tz`.
#[must_use]
pub fn end_of_day(time: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
    let local = time.with_timezone(&tz).date_naive();
    let end = local
        .and_hms_micro_opt(23, 59, 59, 999_999)
        .unwrap_or_default();
    tz.from_local_datetime(&end)
        .latest()
        .map_or(time, |t| t.with_timezone(&Utc))
}

/// The account's zone (`ActiveSupport::TimeZone[account.timezone] || Time.zone`).
#[must_use]
pub fn account_zone(timezone: &str) -> Tz {
    loco_domain::timezones::tz(timezone).unwrap_or(Tz::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_in_the_account_zone() {
        let berlin: Tz = "Europe/Berlin".parse().unwrap();
        assert_eq!(
            parse_in_zone("2026-11-07", berlin).unwrap().to_rfc3339(),
            "2026-11-06T23:00:00+00:00"
        );
        assert_eq!(
            parse_in_zone("2026-11-07T10:00:00Z", berlin)
                .unwrap()
                .to_rfc3339(),
            "2026-11-07T10:00:00+00:00"
        );
        let start = parse_in_zone("2026-11-07", berlin).unwrap();
        assert_eq!(
            end_of_day(start, berlin).to_rfc3339(),
            "2026-11-07T22:59:59.999999+00:00"
        );
        assert!(parse_in_zone("garbage", berlin).is_none());
    }
}
