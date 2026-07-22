use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Convert a UTC DateTime to Modified Julian Date (MJD).
pub fn utc_to_mjd(dt: DateTime<Utc>) -> f64 {
    const UNIX_EPOCH_MJD: f64 = 40587.0;

    let secs = dt.timestamp() as f64;
    let nanos = dt.timestamp_subsec_nanos() as f64;
    UNIX_EPOCH_MJD + (secs + nanos * 1e-9) / 86400.0
}

/// Convert a UTC modified Julian date to a chrono DateTime
///
/// Warning: Returns a type that does not account for leap seconds.
pub fn utc_mjd_to_datetime(mjd: f64) -> DateTime<Utc> {
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

fn gps_epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_naive_utc_and_offset(
        NaiveDate::from_ymd_opt(1980, 1, 6)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        Utc,
    )
}

#[derive(Clone, Copy, Debug)]
struct LeapEntry {
    effective_utc: (i32, u32, u32, u32, u32, u32),
    gps_minus_utc: i64,
}

const LEAP_TABLE: &[LeapEntry] = &[
    LeapEntry {
        effective_utc: (1981, 7, 1, 0, 0, 0),
        gps_minus_utc: 1,
    },
    LeapEntry {
        effective_utc: (1982, 7, 1, 0, 0, 0),
        gps_minus_utc: 2,
    },
    LeapEntry {
        effective_utc: (1983, 7, 1, 0, 0, 0),
        gps_minus_utc: 3,
    },
    LeapEntry {
        effective_utc: (1985, 7, 1, 0, 0, 0),
        gps_minus_utc: 4,
    },
    LeapEntry {
        effective_utc: (1988, 1, 1, 0, 0, 0),
        gps_minus_utc: 5,
    },
    LeapEntry {
        effective_utc: (1990, 1, 1, 0, 0, 0),
        gps_minus_utc: 6,
    },
    LeapEntry {
        effective_utc: (1991, 1, 1, 0, 0, 0),
        gps_minus_utc: 7,
    },
    LeapEntry {
        effective_utc: (1992, 7, 1, 0, 0, 0),
        gps_minus_utc: 8,
    },
    LeapEntry {
        effective_utc: (1993, 7, 1, 0, 0, 0),
        gps_minus_utc: 9,
    },
    LeapEntry {
        effective_utc: (1994, 7, 1, 0, 0, 0),
        gps_minus_utc: 10,
    },
    LeapEntry {
        effective_utc: (1996, 1, 1, 0, 0, 0),
        gps_minus_utc: 11,
    },
    LeapEntry {
        effective_utc: (1997, 7, 1, 0, 0, 0),
        gps_minus_utc: 12,
    },
    LeapEntry {
        effective_utc: (1999, 1, 1, 0, 0, 0),
        gps_minus_utc: 13,
    },
    LeapEntry {
        effective_utc: (2006, 1, 1, 0, 0, 0),
        gps_minus_utc: 14,
    },
    LeapEntry {
        effective_utc: (2009, 1, 1, 0, 0, 0),
        gps_minus_utc: 15,
    },
    LeapEntry {
        effective_utc: (2012, 7, 1, 0, 0, 0),
        gps_minus_utc: 16,
    },
    LeapEntry {
        effective_utc: (2015, 7, 1, 0, 0, 0),
        gps_minus_utc: 17,
    },
    LeapEntry {
        effective_utc: (2017, 1, 1, 0, 0, 0),
        gps_minus_utc: 18,
    },
];

fn make_utc(ts: (i32, u32, u32, u32, u32, u32)) -> DateTime<Utc> {
    DateTime::<Utc>::from_naive_utc_and_offset(
        NaiveDate::from_ymd_opt(ts.0, ts.1, ts.2)
            .unwrap()
            .and_hms_opt(ts.3, ts.4, ts.5)
            .unwrap(),
        Utc,
    )
}

fn gps_minus_utc_for_utc(utc: DateTime<Utc>) -> i64 {
    let mut offset = 0i64;
    for entry in LEAP_TABLE {
        if utc >= make_utc(entry.effective_utc) {
            offset = entry.gps_minus_utc;
        } else {
            break;
        }
    }
    offset
}

/// Convert GPS seconds to UTC.
pub fn gps_to_utc(gps_seconds: f64) -> Option<DateTime<Utc>> {
    if !gps_seconds.is_finite() || gps_seconds < 0.0 {
        return None;
    }

    let epoch = gps_epoch();
    let gps_duration = Duration::nanoseconds((gps_seconds * 1e9).round() as i64);
    let mut offset = LEAP_TABLE.last().map(|e| e.gps_minus_utc).unwrap_or(0);

    for _ in 0..8 {
        let candidate_utc = epoch + gps_duration - Duration::seconds(offset);
        let new_offset = gps_minus_utc_for_utc(candidate_utc);
        if new_offset == offset {
            return Some(candidate_utc);
        }
        offset = new_offset;
    }

    Some(epoch + gps_duration - Duration::seconds(offset))
}

/// Convert UTC to GPS seconds.
pub fn utc_to_gps(utc: DateTime<Utc>) -> f64 {
    let epoch = gps_epoch();
    let offset = gps_minus_utc_for_utc(utc);

    let delta = utc - epoch;
    let whole_secs = delta.num_seconds();
    let frac = (delta - Duration::seconds(whole_secs))
        .num_nanoseconds()
        .unwrap_or(0) as f64
        * 1e-9;

    whole_secs as f64 + frac + offset as f64
}

pub fn gps_to_utc_mjd(t: f64) -> Option<f64> {
    gps_to_utc(t).map(utc_to_mjd)
}

pub fn utc_mjd_to_gps(t: f64) -> Option<f64> {
    let utc = utc_mjd_to_datetime(t);
    if utc < gps_epoch() {
        None
    } else {
        Some(utc_to_gps(utc))
    }
}

pub fn euler213_to_quaternion(pitch: f64, roll: f64, yaw: f64) -> (f64, f64, f64, f64) {
    let (sy, cy) = (yaw / 2.0).sin_cos();
    let (sp, cp) = (pitch / 2.0).sin_cos();
    let (sr, cr) = (roll / 2.0).sin_cos();

    let w = cr * cp * cy - sr * sp * sy;
    let x = sr * cp * cy + cr * sp * sy;
    let y = cr * sp * cy - sr * cp * sy;
    let z = cr * cp * sy + sr * sp * cy;

    (x, y, z, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_utc_mjd_to_datetime() {
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

    #[test]
    fn epoch_maps_correctly() {
        let t = gps_to_utc(0.0).unwrap();
        assert_eq!(t.to_rfc3339(), "1980-01-06T00:00:00+00:00");
    }

    #[test]
    fn recent_date() {
        let utc = make_utc((2017, 1, 1, 0, 0, 0));
        let gps = (utc - gps_epoch()).num_seconds() + 18;
        let t = gps_to_utc(gps as f64).unwrap();
        assert_eq!(t, utc);
    }

    #[test]
    fn fractional_seconds() {
        let t = gps_to_utc(1_000_000.25).unwrap();
        assert_eq!(t.timestamp_subsec_nanos(), 250_000_000);
    }

    #[test]
    fn utc_to_gps_at_2017_boundary() {
        let utc = make_utc((2017, 1, 1, 0, 0, 0));
        let gps = utc_to_gps(utc);
        let expected = (utc - gps_epoch()).num_seconds() as f64 + 18.0;
        assert_eq!(gps, expected);
    }

    #[test]
    fn round_trip_non_leap_second() {
        let utc = make_utc((2020, 1, 1, 12, 34, 56));
        let gps = utc_to_gps(utc);
        let back = gps_to_utc(gps).unwrap();
        assert_eq!(back, utc);
    }

    #[test]
    fn leap_second_boundary_convention() {
        assert_eq!(
            gps_to_utc(1167264016.0).unwrap().to_rfc3339(),
            "2016-12-31T23:59:59+00:00"
        );
        assert_eq!(
            gps_to_utc(1167264017.0).unwrap().to_rfc3339(),
            "2016-12-31T23:59:59+00:00"
        );
        assert_eq!(
            gps_to_utc(1167264018.0).unwrap().to_rfc3339(),
            "2017-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn test_euler213_to_quaternion() {
        let (x, y, z, w) = euler213_to_quaternion(0.0, 0.0, 0.0);
        assert!((x - 0.0).abs() < 1e-6);
        assert!((y - 0.0).abs() < 1e-6);
        assert!((z - 0.0).abs() < 1e-6);
        assert!((w - 1.0).abs() < 1e-6);

        let (x, y, z, _) = euler213_to_quaternion(
            -2.77_f64.to_radians(),
            0.07_f64.to_radians(),
            -58.35_f64.to_radians(),
        );
        assert!((x - 0.0123).abs() < 1e-4);
        assert!((y - -0.0208).abs() < 1e-4);
        assert!((z - -0.4873).abs() < 1e-4);
    }
}
