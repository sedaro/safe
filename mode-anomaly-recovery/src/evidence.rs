use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::types::LiveContextSnapshot;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EvidenceItem {
    pub(crate) id: String,
    pub(crate) kind: &'static str,
    pub(crate) version: u64,
    pub(crate) status: String,
    pub(crate) summary: Value,
}

#[derive(Debug)]
pub(crate) struct EvidenceLedger {
    items: Vec<EvidenceItem>,
    max_items: usize,
}

impl EvidenceLedger {
    pub(crate) fn new(max_items: usize) -> Self {
        Self {
            items: Vec::new(),
            max_items,
        }
    }

    pub(crate) fn record(
        &mut self,
        kind: &'static str,
        version: u64,
        status: &str,
        summary: Value,
    ) -> String {
        let id = format!("{kind}-{version}-{}", self.items.len() + 1);
        if self.items.len() == self.max_items {
            self.items.remove(0);
        }
        self.items.push(EvidenceItem {
            id: id.clone(),
            kind,
            version,
            status: status.into(),
            summary,
        });
        id
    }

    pub(crate) fn ids(&self) -> Vec<String> {
        self.items.iter().map(|item| item.id.clone()).collect()
    }

    pub(crate) fn contains_all(&self, ids: &[String]) -> bool {
        ids.iter()
            .all(|id| self.items.iter().any(|item| &item.id == id))
    }

    pub(crate) fn has_kind(&self, kind: &str) -> bool {
        self.items.iter().any(|item| item.kind == kind)
    }

    pub(crate) fn kind_is_unavailable(&self, kind: &str) -> bool {
        self.items
            .iter()
            .any(|item| item.kind == kind && item.status == "unavailable")
    }

    pub(crate) fn prompt_value(&self) -> Value {
        json!(self.items)
    }
}

impl Default for EvidenceLedger {
    fn default() -> Self {
        Self::new(16)
    }
}

pub(crate) fn telemetry_summary(snapshot: &LiveContextSnapshot) -> Value {
    let mut sources = BTreeMap::new();
    for (source, samples) in &snapshot.telemetry_history {
        let latest = samples.back();
        sources.insert(
            source,
            json!({
                "sample_count": samples.len(),
                "latest_ts_mono": latest.map(|sample| sample.ts_mono),
                "trend": trend(samples.iter().map(|sample| (sample.ts_mono, &sample.payload))),
            }),
        );
    }
    json!({"sources": sources, "history_available": !snapshot.telemetry_history.is_empty()})
}

fn trend<'a>(mut samples: impl Iterator<Item = (u64, &'a Value)>) -> Value {
    let Some((first_time, first)) = samples.next() else {
        return json!({"available": false});
    };
    let Some((last_time, last)) = samples.last().or(Some((first_time, first))) else {
        return json!({"available": false});
    };
    match (
        first.as_f64(),
        last.as_f64(),
        last_time.checked_sub(first_time),
    ) {
        (Some(a), Some(b), Some(dt)) if dt > 0 => {
            json!({"available": true, "delta": b - a, "sample_time_delta": dt})
        }
        _ => {
            json!({"available": false, "reason": "numeric values or a monotonic time basis are unavailable"})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ledger_retains_cumulative_provenance() {
        let mut ledger = EvidenceLedger::default();
        let telemetry = ledger.record("telemetry", 1, "ok", json!({"source":"a"}));
        let board = ledger.record("board", 2, "unavailable", json!({}));
        assert!(ledger.contains_all(&[telemetry, board]));
        assert!(ledger.has_kind("telemetry"));
    }

    #[test]
    fn configured_capacity_evicts_oldest_evidence() {
        let mut ledger = EvidenceLedger::new(2);
        let oldest = ledger.record("telemetry", 1, "ok", json!({}));
        let retained = ledger.record("board", 1, "ok", json!({}));
        ledger.record("telemetry", 2, "ok", json!({}));
        assert!(!ledger.contains_all(&[oldest]));
        assert!(ledger.contains_all(&[retained]));
    }
}
