use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryFrame {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub ts_mono: u64,
    #[serde(default)]
    #[serde(with = "json_value_as_string")]
    pub payload: serde_json::Value,
}

impl TelemetryFrame {
    pub fn new(payload: serde_json::Value) -> Self {
        Self {
            source: None,
            ts_mono: 0,
            payload,
        }
    }

    pub fn decode_payload<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_value(self.payload.clone())
    }
}

mod json_value_as_string {
    use serde::de::Error as DeError;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let json = serde_json::to_string(value).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&json)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let json = String::deserialize(deserializer)?;
        serde_json::from_str(&json).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::TelemetryFrame;

    #[test]
    fn telemetry_frame_bincode_roundtrips_payload() {
        let frame = TelemetryFrame {
            source: Some("sim".to_string()),
            ts_mono: 42,
            payload: serde_json::json!({"counter": 7, "nested": {"ok": true}}),
        };

        let bytes = bincode::serialize(&frame).expect("serialize frame");
        let decoded: TelemetryFrame = bincode::deserialize(&bytes).expect("deserialize frame");

        assert_eq!(decoded.source.as_deref(), Some("sim"));
        assert_eq!(decoded.ts_mono, 42);
        assert_eq!(decoded.payload["counter"], 7);
        assert_eq!(decoded.payload["nested"]["ok"], true);
    }
}
