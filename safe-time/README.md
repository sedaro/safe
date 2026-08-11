# `safe-time`

`safe-time` contains time and attitude utilities shared by SAFE. The `safe`
crate re-exports these functions from `safe::utils`.

## Time Scales

| Function | Description |
| --- | --- |
| `utc_to_mjd` | Convert a UTC `DateTime` to Modified Julian Date. |
| `utc_mjd_to_datetime` | Convert MJD to UTC. The result does not model leap seconds. |
| `gps_to_utc` | Convert non-negative finite GPS seconds since 1980-01-06 to UTC. |
| `utc_to_gps` | Convert UTC to GPS seconds using the embedded leap table. |
| `gps_to_utc_mjd` | Convert GPS seconds to MJD. |
| `utc_mjd_to_gps` | Convert MJD to GPS seconds, returning `None` before the GPS epoch. |

GPS conversions use the leap-second table included in `src/lib.rs`, currently
ending at the 2017 offset of 18 seconds. Update the table when mission time
requirements extend beyond its last entry.

Chrono cannot represent `23:59:60`. At the 2016 leap-second boundary, this
crate's documented convention maps the leap-second instant to the preceding
`23:59:59` value and then maps the next second to `2017-01-01T00:00:00`.

## Attitude Utility

`euler213_to_quaternion(pitch, roll, yaw)` returns an `(x, y, z, w)` quaternion.
Angles are expected in radians. Zero angles return `(0.0, 0.0, 0.0, 1.0)`.

## Tests

```bash
cargo test -p safe-time
```
