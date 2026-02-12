use std::time::{SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Duration, Utc};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Timestamped<W> {
    timestamp: u128,
    wrapped: W,
}
impl<W> Timestamped<W> {
    pub fn new(wrapped: W) -> Self {
        Self {
            timestamp: SystemTime::now()
              .duration_since(UNIX_EPOCH)
              .expect("Time went backwards")
              .as_nanos(),
            wrapped,
        }
    }
    pub fn timestamp(&self) -> u128 {
        self.timestamp
    }
    pub fn into_inner(self) -> W {
        self.wrapped
    }
    pub fn inner(&self) -> &W {
        &self.wrapped
    }
}

/// Convert a UTC modified Julian date to a chrono DateTime
///
/// Warning: Returns a type that does not account for leap seconds, be cautious
/// when using this to calculate time differences.
pub(crate) fn utc_mjd_to_datetime(mjd: f64) -> DateTime<Utc> {
    static DT_EPOCH: DateTime<Utc> = DateTime::from_naive_utc_and_offset(
        chrono::NaiveDate::from_ymd_opt(1858, 11, 17)
            .expect("Failed to create NaiveDate")
            .and_hms_opt(0, 0, 0)
            .expect("Failed to create NaiveDateTime"),
        Utc,
    );
    let mjd_sec = mjd * 86400.;
    DT_EPOCH + Duration::new(mjd_sec as i64, (mjd_sec.fract() * 1e9) as u32).unwrap()
}

const LEAP_SECONDS: f64 = 18.0;  // number of leap seconds as of 2017-01-01
const GPS_EPOCH: f64 = 44244.0;  // MJD for 1980-01-06

/// Convert GPS time to Modified Julian Date (MJD).
/// NOTE: This is only valid for GPS times after 2017 and before the next leap second as of 2026.
pub(crate) fn gps_to_utc_mjd(t: f64) -> f64 {
    GPS_EPOCH + ((t - LEAP_SECONDS) / 86400.0)
}

/// Convert Modified Julian Date (MJD) to GPS time.
/// NOTE: This is only valid for GPS times after 2017 and before the next leap second as of 2026.
pub(crate) fn utc_mjd_to_gps(t: f64) -> f64 {
    ((t - GPS_EPOCH) * 86400.0) + LEAP_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utc_mjd_to_datetime() {
        // Truth from NASA HESEARC tools website
        let cases = [
            (58000.0, "2017-09-04T00:00:00.000+00:00"),
            (58000.5, "2017-09-04T12:00:00.000+00:00"),
            (53064.5, "2004-02-29T12:00:00.000+00:00"),
            (45835.9668082292, "1984-05-15T23:12:12.231+00:00"),
        ];
        for (mjd, expected) in cases {
            let dt = utc_mjd_to_datetime(mjd);
            assert_eq!(
                dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
                expected
            );
        }
    }
}