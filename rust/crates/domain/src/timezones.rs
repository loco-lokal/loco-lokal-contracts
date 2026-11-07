//! Rails time zone names (`ActiveSupport::TimeZone`), stored in `accounts.timezone`.

use std::{collections::HashMap, sync::LazyLock};

use serde::Deserialize;

#[derive(Deserialize)]
struct Zone {
    name: String,
    tzinfo: String,
}

/// `ActiveSupport::TimeZone.all` in Rails' order (dumped from Rails into
/// `rust/data/rails_time_zones.json`).
static ZONES: LazyLock<Vec<Zone>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../../data/rails_time_zones.json"))
        .expect("valid rails_time_zones.json")
});

/// `ActiveSupport::TimeZone::MAPPING`: Rails name to IANA identifier.
static MAPPING: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    ZONES
        .iter()
        .map(|z| (z.name.as_str(), z.tzinfo.as_str()))
        .collect()
});

/// `Accounts.normalize_timezone`: maps an IANA identifier (what the browser reports) or a
/// Rails name to the first Rails name for that zone. Valid identifiers without a Rails
/// name are kept as given; unknown values become `"UTC"`.
#[must_use]
pub fn normalize(timezone: &str) -> String {
    let identifier = MAPPING.get(timezone).copied().unwrap_or(timezone);

    if identifier.parse::<chrono_tz::Tz>().is_err() {
        return "UTC".to_owned();
    }

    ZONES
        .iter()
        .find(|z| z.tzinfo == identifier)
        .map_or_else(|| timezone.to_owned(), |z| z.name.clone())
}

/// The IANA zone for a stored `accounts.timezone` value (`Account#tz_info`).
#[must_use]
pub fn tz(timezone: &str) -> Option<chrono_tz::Tz> {
    MAPPING
        .get(timezone)
        .copied()
        .unwrap_or(timezone)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_rails() {
        assert_eq!(normalize("Europe/Berlin"), "Berlin");
        assert_eq!(normalize("Berlin"), "Berlin");
        assert_eq!(normalize("Etc/UTC"), "UTC");
        assert_eq!(normalize("UTC"), "UTC");
        assert_eq!(normalize("America/New_York"), "Eastern Time (US & Canada)");
        assert_eq!(normalize("America/Boise"), "America/Boise");
        assert_eq!(normalize("Not/AZone"), "UTC");
        assert_eq!(normalize(""), "UTC");
        assert_eq!(tz("Berlin"), Some(chrono_tz::Europe::Berlin));
    }
}
