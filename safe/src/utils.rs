use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use safe_time::{
    euler213_to_quaternion, gps_to_utc, gps_to_utc_mjd, utc_mjd_to_datetime, utc_mjd_to_gps,
    utc_to_gps, utc_to_mjd,
};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::warn;

#[allow(dead_code)]
pub const SECONDS_PER_DAY: f64 = 86_400.0;

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

/// J2000 epoch: 2000-01-01T12:00:00 (commonly referenced in TT/TAI contexts)
pub fn tai_s_to_j2000_s(tai_seconds: f64) -> f64 {
    // Unix timestamp for 2000-01-01T12:00:00 UTC:
    const J2000_UNIX_UTC_SECONDS: f64 = 946_728_000.0;

    // Convert UTC -> TAI, then subtract J2000 reference
    tai_seconds - J2000_UNIX_UTC_SECONDS
}

/// Crash-safer atomic write:
/// 1) write to temp file in same dir
/// 2) fsync temp file
/// 3) rename temp -> target (atomic within same fs)
/// 4) fsync parent dir (persist dir entry change)
///
/// Note: this function runs blocking filesystem calls in spawn_blocking.
pub async fn atomic_write_file(path: impl AsRef<Path>, data: &[u8]) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let data = data.to_vec();

    tokio::task::spawn_blocking(move || atomic_write_file_blocking(&path, &data))
        .await
        .map_err(|join_err| {
            io::Error::new(io::ErrorKind::Other, format!("join error: {join_err}"))
        })?
}

fn atomic_write_file_blocking(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;

    // Create a unique temp file path in the same directory.
    // Same directory is important so rename is atomic.
    let tmp_path = make_temp_path(parent, path.file_name().unwrap_or_default());

    // 0o600 so it's private while being written.
    let mut tmp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp_path)?;

    // Write and flush file contents.
    tmp.write_all(data)?;
    tmp.flush()?;

    // Ensure file contents + inode metadata are on disk.
    tmp.sync_all()?;

    // Drop before rename (not strictly required on Unix, but cleaner).
    drop(tmp);

    // Atomic replacement (on same filesystem).
    fs::rename(&tmp_path, path)?;

    // fsync parent directory so the rename itself is durable. Some Unix
    // platforms and filesystems, including common macOS filesystems, reject
    // directory fsync even though the rename itself succeeded atomically.
    let dir = File::open(parent)?;
    match dir.sync_all() {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

fn make_temp_path(parent: &Path, base_name: impl AsRef<std::ffi::OsStr>) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let mut name = std::ffi::OsString::from(".");
    name.push(base_name.as_ref());
    name.push(format!(".tmp.{pid}.{nanos}"));

    parent.join(name)
}

pub async fn save_json_atomic<T: Serialize>(path: &PathBuf, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec(value)?;
    atomic_write_file(path, &bytes).await.expect("atomic write");
    Ok(())
}

pub async fn append_jsonl<T: Serialize>(path: &PathBuf, v: &T) -> anyhow::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file_contents = if Path::new(path).exists() {
        tokio::fs::read_to_string(path).await?
    } else {
        "".to_string()
    };
    let line = serde_json::to_string(v)?;
    file_contents.push_str(&line);
    file_contents.push('\n');
    atomic_write_file(path, file_contents.as_bytes()).await?;
    Ok(())
}

/// Append one JSONL record without allowing the journal to exceed `max_bytes`.
/// Returns true when checkpointed records were replaced by the new record.
pub async fn append_jsonl_bounded<T: Serialize>(
    path: &PathBuf,
    v: &T,
    max_bytes: u64,
) -> anyhow::Result<bool> {
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    if line.len() as u64 > max_bytes {
        anyhow::bail!(
            "JSONL record is {} bytes, exceeding the {max_bytes} byte journal limit",
            line.len()
        );
    }

    let current_bytes = match fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    if current_bytes.saturating_add(line.len() as u64) > max_bytes {
        atomic_write_file(path, &line).await?;
        return Ok(true);
    }

    append_jsonl(path, v).await?;
    Ok(false)
}

pub async fn load_or_default_json<T>(path: &PathBuf, default_value: T) -> anyhow::Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
{
    if Path::new(path).exists() {
        let s = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&s)?)
    } else {
        save_json_atomic(path, &default_value).await?;
        Ok(default_value)
    }
}

pub async fn load_or_default_jsonl<T>(path: &PathBuf, default_value: T) -> anyhow::Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
{
    if Path::new(path).exists() {
        let f = fs::File::open(path).await?;
        let mut lines = BufReader::new(f).lines();
        let mut last_seen = None;
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(t) => last_seen = Some(t),
                Err(_) => {
                    warn!("Failed to parse JSON line in {:?}", path);
                    continue;
                }
            }
        }

        if last_seen.is_some() {
            Ok(last_seen.unwrap())
        } else {
            save_json_atomic(path, &default_value).await?;
            Ok(default_value)
        }
    } else {
        save_json_atomic(path, &default_value).await?;
        Ok(default_value)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, Utc};

    use super::*;

    fn make_utc(ts: (i32, u32, u32, u32, u32, u32)) -> DateTime<Utc> {
        DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(ts.0, ts.1, ts.2)
                .unwrap()
                .and_hms_opt(ts.3, ts.4, ts.5)
                .unwrap(),
            Utc,
        )
    }

    fn gps_epoch() -> DateTime<Utc> {
        make_utc((1980, 1, 6, 0, 0, 0))
    }

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

    #[test]
    fn epoch_maps_correctly() {
        let t = gps_to_utc(0.0).unwrap();
        assert_eq!(t.to_rfc3339(), "1980-01-06T00:00:00+00:00");
    }

    #[test]
    fn recent_date() {
        // 2017-01-01 UTC should be GPS seconds corresponding to offset 18
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
        // 2016-12-31 leap second:
        // 1167264017 corresponds to UTC 23:59:60, which chrono cannot represent.
        // Convention used here collapses it to 23:59:59.
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
