use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::telemetry_frame::TelemetryFrame;

pub const AUTONOMY_MODE_PROTOCOL_VERSION: u16 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    SetPidControllerGains(f64, f64, f64, f64),
    IridiumPowerOn,
    IridiumPowerOff,
    IridiumTransmitMsg(String),
    PointSunYaw,
    PointNadir,
    PointQuaternion { x: f64, y: f64, z: f64, w: f64 },
    CaptureImage,
    PointThruster,
    ThrusterOn,
    ThrusterOff,
}

impl Into<String> for &Command {
    fn into(self) -> String {
        match self {
            Command::SetPidControllerGains(p, i, d, f) => {
                format!("SetPidControllerGains({p}, {i}, {d}, {f})")
            }
            Command::IridiumPowerOn => "IridiumPowerOn".to_string(),
            Command::IridiumPowerOff => "IridiumPowerOff".to_string(),
            Command::IridiumTransmitMsg(msg) => format!("IridiumTransmitMsg({msg})"),
            Command::PointSunYaw => "PointSunYaw".to_string(),
            Command::PointNadir => "PointNadir".to_string(),
            Command::PointQuaternion { x, y, z, w } => {
                format!("PointQuaternion({x}, {y}, {z}, {w})")
            }
            Command::CaptureImage => "CaptureImage".to_string(),
            Command::PointThruster => "PointThruster".to_string(),
            Command::ThrusterOn => "ThrusterOn".to_string(),
            Command::ThrusterOff => "ThrusterOff".to_string(),
        }
    }
}

impl Into<String> for Command {
    fn into(self) -> String {
        match self {
            Command::SetPidControllerGains(p, i, d, f) => {
                format!("SetPidControllerGains({p}, {i}, {d}, {f})")
            }
            Command::IridiumPowerOn => "IridiumPowerOn".to_string(),
            Command::IridiumPowerOff => "IridiumPowerOff".to_string(),
            Command::IridiumTransmitMsg(msg) => format!("IridiumTransmitMsg({msg})"),
            Command::PointSunYaw => "PointSunYaw".to_string(),
            Command::PointNadir => "PointNadir".to_string(),
            Command::PointQuaternion { x, y, z, w } => {
                format!("PointQuaternion({x}, {y}, {z}, {w})")
            }
            Command::CaptureImage => "CaptureImage".to_string(),
            Command::PointThruster => "PointThruster".to_string(),
            Command::ThrusterOn => "ThrusterOn".to_string(),
            Command::ThrusterOff => "ThrusterOff".to_string(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub enum TimedCommand {
    Now(Command),
    NOOP,
    Scheduled { cmd: Command, gps_time: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AutonomyModeId(pub Uuid);

impl std::fmt::Display for AutonomyModeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for AutonomyModeId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub from: AutonomyModeId,
    pub cmd: TimedCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AutonomyModeInput {
    Telemetry(TelemetryFrame),
    Activate,
    Deactivate,
    Restart,
    Shutdown,
    BoardSnapshot(AutonomyModeBoardState),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AutonomyModeOutput {
    Command(CommandEnvelope),
    Fault(String),
    CancelBoard { id: BoardCmdId, reason: String },
    Lifecycle { state: AutonomyModeLifecycle },
    Heartbeat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyModeLifecycle {
    Ready,
    Active,
    Inactive,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BoardCmdId(pub String);

impl BoardCmdId {
    pub fn from_event(seq: u64, from: AutonomyModeId, local_idx: u32) -> Self {
        Self(format!("{seq}:{from}:{local_idx}"))
    }

    pub fn parse(&self) -> Option<(u64, AutonomyModeId, u32)> {
        let parts: Vec<&str> = self.0.split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        let seq = parts[0].parse::<u64>().ok()?;
        let from = Uuid::parse_str(parts[1]).ok().map(AutonomyModeId)?;
        let local_idx = parts[2].parse::<u32>().ok()?;
        Some((seq, from, local_idx))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BoardEvent {
    Proposed {
        id: BoardCmdId,
        from: AutonomyModeId,
        cmd: TimedCommand,
        ts_mono: u64,
    },
    Canceled {
        id: BoardCmdId,
        by: AutonomyModeId,
        reason: String,
        ts_mono: u64,
    },
    Approved {
        id: BoardCmdId,
        by: AutonomyModeId,
        reason: String,
        ts_mono: u64,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardState {
    pub proposals: HashMap<BoardCmdId, (AutonomyModeId, TimedCommand, u64)>,
    pub rejected: HashMap<BoardCmdId, Vec<(AutonomyModeId, String, u64)>>,
    pub approved: HashMap<BoardCmdId, Vec<(AutonomyModeId, String, u64)>>,
    pub source_of_truth: Vec<BoardCmdId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutonomyModeBoardState {
    pub proposals: HashMap<BoardCmdId, (AutonomyModeId, TimedCommand, u64)>,
    pub rejected: HashMap<BoardCmdId, Vec<(AutonomyModeId, String, u64)>>,
    pub approved: HashMap<BoardCmdId, Vec<(AutonomyModeId, String, u64)>>,
    pub source_of_truth: Vec<BoardCmdId>,
}

impl BoardState {
    pub fn apply(&mut self, ev: &BoardEvent) {
        match ev {
            BoardEvent::Proposed {
                id,
                from,
                cmd,
                ts_mono,
            } => {
                self.proposals
                    .insert(id.clone(), (*from, cmd.clone(), *ts_mono));
                self.recompute();
            }
            BoardEvent::Canceled {
                id,
                by,
                reason,
                ts_mono,
            } => {
                self.rejected
                    .entry(id.clone())
                    .or_default()
                    .push((*by, reason.clone(), *ts_mono));
                self.recompute();
            }
            BoardEvent::Approved {
                id,
                by,
                reason,
                ts_mono,
            } => {
                self.approved
                    .entry(id.clone())
                    .or_default()
                    .push((*by, reason.clone(), *ts_mono));
                self.recompute();
            }
        }
    }

    pub fn recompute(&mut self) {
        self.source_of_truth.clear();
        for id in self.proposals.keys() {
            let is_rejected = self
                .rejected
                .get(id)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let is_approved = self
                .approved
                .get(id)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if !is_rejected && is_approved {
                self.source_of_truth.push(id.clone());
            }
        }
        self.source_of_truth.sort_by(|a, b| a.0.cmp(&b.0));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SafeToMode {
    Hello { expected_mode: AutonomyModeId },
    Input(AutonomyModeInput),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModeToSafe {
    Hello {
        mode: AutonomyModeId,
        protocol_version: u16,
    },
    Output(AutonomyModeOutput),
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;

    fn mk_id(v: u128) -> AutonomyModeId {
        AutonomyModeId(Uuid::from_u128(v))
    }

    #[test]
    fn board_recompute_keeps_accepted_sorted() {
        let mut board = BoardState::default();
        board.apply(&BoardEvent::Proposed {
            id: BoardCmdId("2:b:0".into()),
            from: mk_id(2),
            cmd: TimedCommand::Now(Command::PointNadir),
            ts_mono: 2,
        });
        board.apply(&BoardEvent::Proposed {
            id: BoardCmdId("1:a:0".into()),
            from: mk_id(1),
            cmd: TimedCommand::Now(Command::PointSunYaw),
            ts_mono: 1,
        });
        board.apply(&BoardEvent::Approved {
            id: BoardCmdId("2:b:0".into()),
            by: mk_id(2),
            reason: "approved".to_string(),
            ts_mono: 2,
        });
        board.apply(&BoardEvent::Approved {
            id: BoardCmdId("1:a:0".into()),
            by: mk_id(1),
            reason: "approved".to_string(),
            ts_mono: 1,
        });

        assert_eq!(board.source_of_truth[0].0, "1:a:0");
        assert_eq!(board.source_of_truth[1].0, "2:b:0");
    }

    #[test]
    fn point_quaternion_serializes_and_round_trips() {
        let command = Command::PointQuaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        };

        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "PointQuaternion": {
                    "x": 0.0,
                    "y": 0.0,
                    "z": 0.0,
                    "w": 1.0,
                }
            })
        );

        let from_json: Command = serde_json::from_value(json).unwrap();
        assert!(matches!(
            from_json,
            Command::PointQuaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }
        ));

        let bytes = bincode::serialize(&command).unwrap();
        let from_bincode: Command = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            from_bincode,
            Command::PointQuaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }
        ));

        let rendered: String = (&command).into();
        assert_eq!(rendered, "PointQuaternion(0, 0, 0, 1)");
    }

    prop_compose! {
        fn arb_mode_id()(bytes in any::<[u8; 16]>()) -> AutonomyModeId {
            AutonomyModeId(Uuid::from_bytes(bytes))
        }
    }

    prop_compose! {
        fn arb_cmd()(v in 0u8..=2) -> Command {
            match v {
                0 => Command::PointNadir,
                1 => Command::PointSunYaw,
                _ => Command::CaptureImage,
            }
        }
    }

    prop_compose! {
        fn arb_event()(
            kind in 0u8..=1,
            seq in 0u64..100,
            from in arb_mode_id(),
            by in arb_mode_id(),
            cmd in arb_cmd(),
        ) -> BoardEvent {
            let id = BoardCmdId(format!("{seq}:{from:?}:0"));
            if kind == 0 {
                BoardEvent::Proposed {
                    id,
                    from,
                    cmd: TimedCommand::Now(cmd),
                    ts_mono: seq,
                }
            } else {
                BoardEvent::Canceled {
                    id,
                    by,
                    reason: "test".to_string(),
                    ts_mono: seq,
                }
            }
        }
    }

    proptest! {
        #[test]
        fn accepted_subset_and_not_rejected(events in proptest::collection::vec(arb_event(), 0..200)) {
            let mut board = BoardState::default();
            for ev in events {
                board.apply(&ev);
            }

            for id in &board.source_of_truth {
                prop_assert!(board.proposals.contains_key(id));
                let is_rejected = board.rejected.get(id).map(|v| !v.is_empty()).unwrap_or(false);
                prop_assert!(!is_rejected);
            }

            let mut sorted = board.source_of_truth.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            prop_assert_eq!(board.source_of_truth, sorted);
        }
    }
}
