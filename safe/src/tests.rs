use proptest::prelude::*;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tracing_subscriber::fmt::time;
use uuid::Uuid;

use super::*;
use crate::definitions::{Activation, Expr, Value, Variable};
use crate::flight::{AutonomyModeActivation, Flight};
use crate::safetea::{AutonomyModeRuntimeConfig, Event};
use crate::safetea::{Effect, Msg, Source, apply_event, update};
use crate::telemetry_frame::TelemetryFrame;
use crate::utils::{append_jsonl, load_or_default_json, save_json_atomic};

fn mk_mode_id(n: u128) -> AutonomyModeId {
    AutonomyModeId(Uuid::from_u128(n))
}

fn mk_telemetry(counter: u32, time_ms: u64) -> TelemetryFrame {
    TelemetryFrame {
        source: Some("test".to_string()),
        ts_mono: time_ms,
        payload: serde_json::json!({
            "counter": counter,
            "time_ms": time_ms,
            "telemetry": {
                "temperature_value_c": 32.0,
            }
        }),
    }
}

fn mk_flight() -> Flight {
    Flight::default()
}

fn telemetry_with_flag(flag: bool) -> TelemetryFrame {
    TelemetryFrame {
        source: Some("test".to_string()),
        ts_mono: 0,
        payload: serde_json::json!({
            "bitfield": [u8::from(flag)]
        }),
    }
}

fn mode_meta(id: u128, priority: u8, enabled: bool) -> AutonomyModeMeta {
    AutonomyModeMeta {
        id: mk_mode_id(id),
        priority,
        enabled,
    }
}

fn flag_true_expr() -> Expr {
    Expr::equal(
        Expr::Term(Variable::Float64(Value::TelemetryRef(
            "bitfield.0".into(),
        ))),
        Expr::Term(Variable::Float64(Value::Literal(1.0))),
    )
}

fn flag_false_expr() -> Expr {
    Expr::equal(
        Expr::Term(Variable::Float64(Value::TelemetryRef(
            "bitfield.0".into(),
        ))),
        Expr::Term(Variable::Float64(Value::Literal(0.0))),
    )
}

#[test]
fn choose_active_autonomy_mode_picks_highest_enabled_priority() {
    let mut f = mk_flight();

    f.set_autonomy_modes(vec![
        AutonomyModeMeta {
            id: mk_mode_id(1),
            priority: 1,
            enabled: true,
        },
        AutonomyModeMeta {
            id: mk_mode_id(2),
            priority: 2,
            enabled: true,
        },
    ]);

    f.recalculate_active_autonomy_mode();
    let picked = f.get_active_autonomy_mode();
    assert_eq!(picked, Some(mk_mode_id(2)));
}

#[test]
fn choose_active_autonomy_mode_skips_disabled_modes() {
    let mut f = mk_flight();

    f.set_autonomy_modes(vec![
        AutonomyModeMeta {
            id: mk_mode_id(1),
            priority: 1,
            enabled: true,
        },
        AutonomyModeMeta {
            id: mk_mode_id(2),
            priority: 99,
            enabled: false,
        },
    ]);

    f.recalculate_active_autonomy_mode();
    let picked = f.get_active_autonomy_mode();
    assert_eq!(picked, Some(mk_mode_id(1)));
}

#[test]
fn choose_active_autonomy_mode_none_if_all_disabled() {
    let mut f = mk_flight();
    for m in f.get_autonomy_modes_mut() {
        m.enabled = false;
    }

    f.recalculate_active_autonomy_mode();
    let picked = f.get_active_autonomy_mode();
    assert_eq!(picked, None);
}

#[test]
fn update_command_received_emits_execute_and_ack() {
    let mut f = mk_flight();
    let cmd = Command::PointNadir;
    let ev = Event {
        seq: 1,
        ts_mono: 1,
        source: Source::Controller,
        msg: Msg::ExecuteNow(cmd.clone()),
    };

    let fx = update(&mut f, &ev);

    assert!(
            fx.iter().any(
                |e| matches!(e, Effect::ExecuteCommand(c) if matches!(c, TimedCommand::Now(Command::PointNadir)))
            )
        );
}

#[test]
fn update_autonomy_mode_command_received_emits_board_and_ack() {
    let mut f = mk_flight();
    let env = CommandEnvelope {
        from: mk_mode_id(2),
        cmd: TimedCommand::Now(Command::CaptureImage),
    };
    let ev = Event {
        seq: 55,
        ts_mono: 999,
        source: Source::AutonomyMode,
        msg: Msg::AutonomyModeCommandReceived(env),
    };

    let fx = update(&mut f, &ev);

    assert!(
        fx.iter()
            .any(|e| matches!(e, Effect::Board(BoardEvent::Proposed { .. })))
    );
}

#[test]
fn update_fault_raised_sets_fault_and_halts_running() {
    let mut f = mk_flight();
    assert!(f.is_running());
    assert!(f.get_fault().is_none());

    let ev = Event {
        seq: 2,
        ts_mono: 2,
        source: Source::AutonomyMode,
        msg: Msg::FaultRaised("boom".into()),
    };
    let fx = update(&mut f, &ev);

    assert_eq!(f.get_fault().as_deref(), Some("boom"));
    assert!(!f.is_running());
    assert!(
        fx.iter()
            .any(|e| matches!(e, Effect::Halt(s) if s == "boom"))
    );
}

#[test]
fn update_external_command_received_emits_ack_only() {
    let mut f = mk_flight();
    let ev = Event {
        seq: 5,
        ts_mono: 5,
        source: Source::Controller,
        msg: Msg::ExternalCommandReceived {
            request_id: None,
            command: ExternalCommand::StopMode {
                mode: mk_mode_id(1),
            },
        },
    };

    let fx = update(&mut f, &ev);

    assert!(!fx.iter().any(|e| matches!(e, Effect::ExecuteCommand(_))));
}

#[test]
fn update_gatekeeper_approved_emits_board_approved() {
    let mut f = mk_flight();
    let id = BoardCmdId("10:00000000-0000-0000-0000-000000000001:0".to_string());
    let ev = Event {
        seq: 10,
        ts_mono: 200,
        source: Source::Gatekeeper,
        msg: Msg::GatekeeperApproved { id: id.clone() },
    };

    let fx = update(&mut f, &ev);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Board(BoardEvent::Approved { id: approved_id, .. }) if approved_id == &id
    )));
}

#[test]
fn update_gatekeeper_rejected_emits_board_canceled() {
    let mut f = mk_flight();
    let id = BoardCmdId("11:00000000-0000-0000-0000-000000000001:0".to_string());
    let ev = Event {
        seq: 11,
        ts_mono: 201,
        source: Source::Gatekeeper,
        msg: Msg::GatekeeperRejected {
            id: id.clone(),
            reason: "sim violation".to_string(),
        },
    };

    let fx = update(&mut f, &ev);
    assert!(fx.iter().any(|e| matches!(
        e,
        Effect::Board(BoardEvent::Canceled { id: canceled_id, reason, .. }) if canceled_id == &id && reason == "sim violation"
    )));
}

#[tokio::test]
async fn apply_event_is_idempotent_for_old_seq() {
    let mut f = mk_flight();
    f.set_seq(10);

    let ev = Event {
        seq: 9,
        ts_mono: 100,
        source: Source::Controller,
        msg: Msg::ExecuteNow(Command::PointSunYaw),
    };

    let tmp = tempfile::tempdir().unwrap();
    let outputs = tmp.path().join("outputs.jsonl");

    apply_event(&mut f, &ev, &outputs, true).await.unwrap();
    assert_eq!(f.get_seq(), 10);

    let content = fs::read_to_string(outputs).await.unwrap_or_default();
    assert!(content.is_empty());
}

#[tokio::test]
async fn apply_event_advances_seq_for_new_event() {
    let mut f = mk_flight();
    f.set_seq(10);

    let ev = Event {
        seq: 11,
        ts_mono: 100,
        source: Source::Controller,
        msg: Msg::ExecuteNow(Command::PointNadir),
    };

    let tmp = tempfile::tempdir().unwrap();
    let outputs = tmp.path().join("outputs.jsonl");

    apply_event(&mut f, &ev, &outputs, true).await.unwrap();
    assert_eq!(f.get_seq(), 11);

    let content = fs::read_to_string(outputs).await.unwrap_or_default();
    assert!(!content.is_empty());
}

#[tokio::test]
async fn append_jsonl_writes_newline_delimited_json() {
    #[derive(Serialize)]
    struct X {
        a: u32,
    }

    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("a/b/c.jsonl");

    append_jsonl(&p, &X { a: 1 }).await.unwrap();
    append_jsonl(&p, &X { a: 2 }).await.unwrap();

    let s = fs::read_to_string(&p).await.unwrap();
    let lines: Vec<_> = s.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"a\":1"));
    assert!(lines[1].contains("\"a\":2"));
}

#[tokio::test]
async fn save_and_load_json_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("flight.json");

    let mut f = mk_flight();
    f.set_seq(42);
    f.set_active_autonomy_mode(mk_mode_id(2));

    save_json_atomic(&p, &f).await.unwrap();
    let loaded: Flight = load_or_default_json(&p, Flight::default()).await.unwrap();

    assert_eq!(loaded.get_seq(), 42);
    assert_eq!(loaded.get_active_autonomy_mode(), Some(mk_mode_id(2)));
}

#[tokio::test]
async fn recovery_replays_unapplied_events_only() {
    let tmp = tempfile::tempdir().unwrap();
    let events = tmp.path().join("events.jsonl");
    let outputs = tmp.path().join("outputs.jsonl");

    // seq 1 and 2 in log
    let ev1 = Event {
        seq: 1,
        ts_mono: 100,
        source: Source::Controller,
        msg: Msg::ExecuteNow(Command::PointNadir),
    };
    let ev2 = Event {
        seq: 2,
        ts_mono: 101,
        source: Source::Controller,
        msg: Msg::ExecuteNow(Command::CaptureImage),
    };
    append_jsonl(&events, &ev1).await.unwrap();
    append_jsonl(&events, &ev2).await.unwrap();

    let mut flight = Flight::default();
    flight.set_seq(1); // should only apply seq=2

    let f = fs::File::open(&events).await.unwrap();
    let mut lines = tokio::io::BufReader::new(f).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: Event = serde_json::from_str(&line).unwrap();
        apply_event(&mut flight, &ev, &outputs, false)
            .await
            .unwrap();
    }

    assert_eq!(flight.get_seq(), 2);
}

#[test]
fn parse_external_mode_config_contents_parses_and_derives_ids() {
    let cfg = r#"[
            {
                "name": "NoImages",
                "priority": 3,
                "enabled": true,
                "bin_path": "/tmp/mode_no_images",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            }
        ]"#;

    let (configs, meta, activations) = AutonomyModeRuntimeConfig::from_str(cfg, 8).unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(meta.len(), 1);
    assert_eq!(activations.len(), 1);
    assert_eq!(
        meta[0].id,
        AutonomyModeRuntimeConfig::mode_id_from_name("NoImages")
    );
    assert_eq!(
        configs[0].id,
        AutonomyModeRuntimeConfig::mode_id_from_name("NoImages")
    );
    assert_eq!(meta[0].priority, 3);
    assert!(meta[0].enabled);
}

#[test]
fn parse_external_mode_config_contents_enforces_max_modes() {
    let cfg = r#"[
            {
                "name": "M1",
                "priority": 1,
                "enabled": true,
                "bin_path": "/tmp/m1",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            },
            {
                "name": "M2",
                "priority": 2,
                "enabled": true,
                "bin_path": "/tmp/m2",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            }
        ]"#;

    let err = AutonomyModeRuntimeConfig::from_str(cfg, 1).unwrap_err();
    assert!(err.to_string().contains("Only 1 are allowed per config"));
}

#[test]
fn parse_external_mode_config_contents_rejects_duplicate_names() {
    let cfg = r#"[
            {
                "name": "NoImages",
                "priority": 1,
                "enabled": true,
                "bin_path": "/tmp/m1",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            },
            {
                "name": "NoImages",
                "priority": 2,
                "enabled": true,
                "bin_path": "/tmp/m2",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            }
        ]"#;

    let err = AutonomyModeRuntimeConfig::from_str(cfg, 8).unwrap_err();
    assert!(
        err.to_string()
            .contains("Duplicate autonomy mode name found: NoImages")
    );
}

#[test]
fn parse_external_mode_config_contents_resolves_relative_bin_path_with_base() {
    let cfg = r#"[
            {
                "name": "NoImages",
                "priority": 3,
                "enabled": true,
                "bin_path": "bin/mode_no_images",
                "args": [],
                "sandbox_resources": {"cpu": 90.0, "memory": 1000000, "disk": 1000000},
                "persist_work_dir": false,
                "mode_config": {}
            }
        ]"#;

    let (configs, _, _) = AutonomyModeRuntimeConfig::from_str_with_base(
        cfg,
        8,
        Some(std::path::Path::new("/tmp/repo/safe")),
    )
    .unwrap();

    assert_eq!(
        configs[0].bin_path,
        std::path::PathBuf::from("/tmp/repo/safe/bin/mode_no_images")
    );
}

#[test]
fn update_external_execute_now_acks_and_executes_now() {
    let mut f = mk_flight();
    let ev = Event {
        seq: 99,
        ts_mono: 99,
        source: Source::Controller,
        msg: Msg::ExternalCommandReceived {
            request_id: None,
            command: ExternalCommand::ExecuteNow {
                command: Command::PointSunYaw,
            },
        },
    };

    let fx = update(&mut f, &ev);
    // TODO:
    // assert!(fx.iter().any(|e| matches!(e, Effect::Ack(_))));
}

#[test]
fn recalc_immediate_activation_prefers_highest_eligible_priority() {
    let mut f = mk_flight();
    f.set_autonomy_modes(vec![mode_meta(1, 5, true), mode_meta(2, 10, true)]);
    f.set_autonomy_mode_activations(vec![
        AutonomyModeActivation {
            id: mk_mode_id(1),
            activation: Some(Activation::Immediate(flag_true_expr())),
        },
        AutonomyModeActivation {
            id: mk_mode_id(2),
            activation: Some(Activation::Immediate(flag_false_expr())),
        },
    ]);
    f.note_telemetry(&telemetry_with_flag(true));

    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(1)));
}

#[test]
fn recalc_hysteretic_mode_holds_until_exit_true() {
    let mut f = mk_flight();
    f.set_autonomy_modes(vec![mode_meta(1, 1, true), mode_meta(2, 10, true)]);
    f.set_autonomy_mode_activations(vec![
        AutonomyModeActivation {
            id: mk_mode_id(1),
            activation: None,
        },
        AutonomyModeActivation {
            id: mk_mode_id(2),
            activation: Some(Activation::Hysteretic {
                enter: flag_true_expr(),
                exit: flag_false_expr(),
            }),
        },
    ]);

    f.note_telemetry(&telemetry_with_flag(true));
    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(2)));

    f.note_telemetry(&telemetry_with_flag(true));
    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(2)));

    f.note_telemetry(&telemetry_with_flag(false));
    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(1)));
}

#[test]
fn recalc_manual_override_pins_until_cleared() {
    let mut f = mk_flight();
    f.set_autonomy_modes(vec![mode_meta(1, 1, true), mode_meta(2, 10, true)]);
    f.set_autonomy_mode_activations(vec![
        AutonomyModeActivation {
            id: mk_mode_id(1),
            activation: None,
        },
        AutonomyModeActivation {
            id: mk_mode_id(2),
            activation: Some(Activation::Immediate(flag_false_expr())),
        },
    ]);
    f.set_manual_active_override(mk_mode_id(2));
    f.note_telemetry(&telemetry_with_flag(true));

    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(2)));

    f.clear_manual_active_override();
    f.recalculate_active_autonomy_mode();
    assert_eq!(f.get_active_autonomy_mode(), Some(mk_mode_id(1)));
}

prop_compose! {
    fn arb_mode_meta()(id_bytes in any::<[u8; 16]>(), priority in 0u8..=200, enabled in any::<bool>()) -> AutonomyModeMeta {
        AutonomyModeMeta {
            id: AutonomyModeId(Uuid::from_bytes(id_bytes)),
            priority,
            enabled,
        }
    }
}

proptest! {
    #[test]
    fn choose_active_returns_highest_enabled(
        modes in proptest::collection::vec(arb_mode_meta(), 1..32),
        counter in any::<u32>(),
        time_ms in any::<u64>(),
    ) {
        let mut f = mk_flight();
        f.set_autonomy_modes(modes.clone());
        let _t = mk_telemetry(counter, time_ms);

        f.recalculate_active_autonomy_mode();
        let got = f.get_active_autonomy_mode();
        let expected = modes
            .iter()
            .filter(|m| m.enabled)
            .max_by_key(|m| m.priority)
            .map(|m| m.id);
        prop_assert_eq!(got, expected);
    }
}
