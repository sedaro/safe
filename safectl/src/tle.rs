use std::path::PathBuf;

use anyhow::Context;
use chrono::{DateTime, Utc};
use safe::runtime::SafectlIngress;
use safe::telemetry_frame::TelemetryFrame;
use sgp4::{Constants, Elements, MinutesSinceEpoch};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::{Duration, sleep};

const EARTH_RADIUS_KM: f64 = 6_378.137;

pub async fn run_sender(
    norad_id: u64,
    tle_file: Option<PathBuf>,
    tle_url: Option<String>,
    start_at: Option<String>,
    step_secs: f64,
    speed: f64,
    frames: Option<u64>,
) -> anyhow::Result<()> {
    if !step_secs.is_finite() || step_secs <= 0.0 {
        anyhow::bail!("--step-secs must be positive");
    }
    if !speed.is_finite() || speed <= 0.0 {
        anyhow::bail!("--speed must be positive");
    }
    if frames == Some(0) {
        anyhow::bail!("--frames must be greater than zero");
    }

    let tle = match tle_file {
        Some(path) => tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("read TLE file {}", path.display()))?,
        None => {
            let url = tle_url.unwrap_or_else(|| {
                format!("https://celestrak.org/NORAD/elements/gp.php?CATNR={norad_id}&FORMAT=TLE")
            });
            reqwest::get(&url)
                .await
                .with_context(|| format!("fetch TLE from {url}"))?
                .error_for_status()
                .with_context(|| format!("fetch TLE from {url}"))?
                .text()
                .await
                .context("read TLE response body")?
        }
    };
    let elements = parse_tle(&tle)?;
    if elements.norad_id != norad_id {
        anyhow::bail!(
            "TLE NORAD ID {} does not match requested --norad-id {norad_id}",
            elements.norad_id
        );
    }

    let start = match start_at {
        Some(value) => DateTime::parse_from_rfc3339(&value)
            .context("parse --start-at as RFC3339")?
            .with_timezone(&Utc),
        None => Utc::now(),
    };
    let mut generator = TelemetryGenerator::new(elements, start)?;
    let runtime_cfg = super::load_runtime_config().await?;
    let sock_path =
        super::state_dir(&runtime_cfg.base_paths.base_writable_directory).join("safectl.sock");
    if !sock_path.exists() {
        anyhow::bail!("SAFE ingress socket not found at {}", sock_path.display());
    }
    let mut stream = UnixStream::connect(&sock_path).await?;
    let delay = Duration::from_secs_f64(step_secs / speed);
    let mut emitted = 0;

    loop {
        let frame = generator.frame(norad_id)?;
        let wire = serde_json::to_string(&SafectlIngress::Telemetry { telemetry: frame })?;
        stream.write_all(wire.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        emitted += 1;
        if frames.is_some_and(|limit| emitted >= limit) {
            break;
        }
        generator.advance(step_secs);
        sleep(delay).await;
    }
    stream.shutdown().await?;
    println!("sent {emitted} TLE telemetry frames for NORAD {norad_id}");
    Ok(())
}

fn parse_tle(input: &str) -> anyhow::Result<Elements> {
    let lines: Vec<_> = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line1 = lines
        .iter()
        .find(|line| line.starts_with("1 "))
        .context("TLE response does not contain line 1")?;
    let line2 = lines
        .iter()
        .find(|line| line.starts_with("2 "))
        .context("TLE response does not contain line 2")?;
    Elements::from_tle(None, line1.as_bytes(), line2.as_bytes()).context("parse TLE")
}

struct TelemetryGenerator {
    constants: Constants,
    epoch: DateTime<Utc>,
    simulated_at: DateTime<Utc>,
    elapsed_secs: f64,
    battery_soc: f64,
    temperature_c: f64,
}

impl TelemetryGenerator {
    fn new(elements: Elements, start: DateTime<Utc>) -> anyhow::Result<Self> {
        let epoch = DateTime::from_naive_utc_and_offset(elements.datetime, Utc);
        Ok(Self {
            constants: Constants::from_elements(&elements).context("initialize SGP4 propagator")?,
            epoch,
            simulated_at: start,
            elapsed_secs: 0.0,
            battery_soc: 0.65,
            temperature_c: 18.0,
        })
    }

    fn frame(&mut self, norad_id: u64) -> anyhow::Result<TelemetryFrame> {
        let minutes = (self.simulated_at - self.epoch).num_milliseconds() as f64 / 60_000.0;
        let prediction = self
            .constants
            .propagate(MinutesSinceEpoch(minutes))
            .context("propagate TLE")?;
        let position = prediction.position;
        let velocity = prediction.velocity;
        let sunlight = !in_earth_shadow(position, sun_vector_eci(self.simulated_at));
        let solar_power_w = if sunlight { 72.0 } else { 0.0 };
        let load_power_w = 38.0;
        let net_power_w = solar_power_w - load_power_w;
        let voltage_v = 28.0 + self.battery_soc * 1.8 - (net_power_w / 100.0);
        let altitude_km = magnitude(position) - EARTH_RADIUS_KM;
        let (latitude_deg, longitude_deg) = ground_track(position, self.simulated_at);

        Ok(TelemetryFrame {
            source: Some(format!("tle-sim:{norad_id}")),
            ts_mono: (self.elapsed_secs * 1_000.0).round() as u64,
            payload: serde_json::json!({
                "telemetry": {
                    "timestamp_utc": self.simulated_at.to_rfc3339(),
                    "onboard_time_ms": (self.elapsed_secs * 1_000.0).round() as u64,
                    "temperature_c": self.temperature_c,
                    "orbit": {
                        "frame": "TEME",
                        "position_km": position,
                        "velocity_km_s": velocity,
                        "altitude_km": altitude_km,
                        "latitude_deg": latitude_deg,
                        "longitude_deg": longitude_deg
                    },
                    "environment": { "sunlit": sunlight },
                    "power": {
                        "solar_power_w": solar_power_w,
                        "load_power_w": load_power_w,
                        "battery_soc": self.battery_soc,
                        "battery_voltage_v": voltage_v,
                        "battery_current_a": -net_power_w / voltage_v
                    },
                    "thermal": { "temperature_c": self.temperature_c }
                }
            }),
        })
    }

    fn advance(&mut self, seconds: f64) {
        let position = self
            .constants
            .propagate(MinutesSinceEpoch(
                (self.simulated_at - self.epoch).num_milliseconds() as f64 / 60_000.0,
            ))
            .map(|prediction| prediction.position)
            .unwrap_or([EARTH_RADIUS_KM, 0.0, 0.0]);
        let sunlight = !in_earth_shadow(position, sun_vector_eci(self.simulated_at));
        let net_power_w = if sunlight { 72.0 } else { 0.0 } - 38.0;
        self.battery_soc = (self.battery_soc + net_power_w * seconds / 360_000.0).clamp(0.05, 1.0);
        let target_temperature_c = if sunlight { 27.0 } else { -4.0 };
        self.temperature_c +=
            (target_temperature_c - self.temperature_c) * (seconds / 1_800.0).min(1.0);
        self.elapsed_secs += seconds;
        self.simulated_at += chrono::TimeDelta::milliseconds((seconds * 1_000.0).round() as i64);
    }
}

fn magnitude(vector: [f64; 3]) -> f64 {
    vector.iter().map(|value| value * value).sum::<f64>().sqrt()
}

fn in_earth_shadow(position: [f64; 3], sun: [f64; 3]) -> bool {
    let projection = dot(position, sun);
    projection < 0.0 && magnitude(cross(position, sun)) < EARTH_RADIUS_KM
}

fn sun_vector_eci(at: DateTime<Utc>) -> [f64; 3] {
    let days = (at - DateTime::UNIX_EPOCH).num_seconds() as f64 / 86_400.0;
    let longitude = (280.46 + 0.985_647_4 * (days - 10_957.5)).to_radians();
    let obliquity = 23.439_f64.to_radians();
    [
        longitude.cos(),
        longitude.sin() * obliquity.cos(),
        longitude.sin() * obliquity.sin(),
    ]
}

fn ground_track(position: [f64; 3], at: DateTime<Utc>) -> (f64, f64) {
    let latitude = (position[2] / magnitude(position)).asin().to_degrees();
    let inertial_longitude = position[1].atan2(position[0]).to_degrees();
    let days = (at - DateTime::UNIX_EPOCH).num_seconds() as f64 / 86_400.0;
    let longitude = (inertial_longitude - (280.46 + 360.985_647_4 * (days - 10_957.5)) + 180.0)
        .rem_euclid(360.0)
        - 180.0;
    (latitude, longitude)
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISS_TLE: &str = "ISS (ZARYA)\n1 25544U 98067A   20194.88612269 -.00002218  00000-0 -31515-4 0  9992\n2 25544  51.6461 221.2784 0001413  89.1723 280.4612 15.49507896236008\n";

    #[test]
    fn parses_three_line_tle() {
        assert_eq!(parse_tle(ISS_TLE).unwrap().norad_id, 25544);
    }

    #[test]
    fn generated_frame_has_bounded_bus_values() {
        let elements = parse_tle(ISS_TLE).unwrap();
        let start = DateTime::parse_from_rfc3339("2020-07-14T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut generator = TelemetryGenerator::new(elements, start).unwrap();
        let frame = generator.frame(25544).unwrap();
        assert!(
            frame.payload["telemetry"]["orbit"]["altitude_km"]
                .as_f64()
                .unwrap()
                > 100.0
        );
        assert!(
            (0.0..=1.0).contains(
                &frame.payload["telemetry"]["power"]["battery_soc"]
                    .as_f64()
                    .unwrap()
            )
        );
    }

    #[test]
    fn model_is_deterministic_for_the_same_tle_and_start_time() {
        let start = DateTime::parse_from_rfc3339("2020-07-14T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut first = TelemetryGenerator::new(parse_tle(ISS_TLE).unwrap(), start).unwrap();
        let mut second = TelemetryGenerator::new(parse_tle(ISS_TLE).unwrap(), start).unwrap();
        first.advance(60.0);
        second.advance(60.0);
        assert_eq!(
            first.frame(25544).unwrap().payload,
            second.frame(25544).unwrap().payload
        );
    }
}
