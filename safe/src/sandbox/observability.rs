use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "otel-metrics")]
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{AnyValue, KeyValue, any_value},
    logs::v1::{LogRecord, SeverityNumber},
    metrics::v1::{
        Gauge, Metric as ProtoMetric, NumberDataPoint, ResourceMetrics as ProtoResourceMetrics,
        ScopeMetrics as ProtoScopeMetrics, Sum, metric::Data, number_data_point::Value,
    },
};
#[cfg(feature = "otel-metrics")]
use prost::Message;
use sysinfo::{Pid, ProcessesToUpdate, System};
#[cfg(feature = "otel-metrics")]
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use crate::runtime::ProcessResourceSnapshot;
use crate::{AutonomyModeId, ModeResourceSnapshot};

#[cfg(feature = "otel-metrics")]
pub fn create_counter(
    name: &str,
    desc: &str,
    unit: &str,
    value: Value,
    start_nanos: u64,
    now_nanos: u64,
    attributes: &Vec<KeyValue>,
) -> ProtoMetric {
    let data_point = NumberDataPoint {
        start_time_unix_nano: start_nanos,
        time_unix_nano: now_nanos,
        value: Some(value),
        attributes: attributes.clone(),
        ..Default::default()
    };

    ProtoMetric {
        name: name.into(),
        description: desc.into(),
        unit: unit.into(),
        data: Some(Data::Sum(Sum {
            data_points: vec![data_point],
            aggregation_temporality: 2, // 2 = CUMULATIVE
            is_monotonic: true,
        })),
        ..Default::default()
    }
}

#[cfg(feature = "otel-metrics")]
pub fn create_gauge(
    name: &str,
    desc: &str,
    unit: &str,
    value: Value,
    now_nanos: u64,
    attributes: &Vec<KeyValue>,
) -> ProtoMetric {
    let data_point = NumberDataPoint {
        time_unix_nano: now_nanos,
        value: Some(value),
        attributes: attributes.clone(),
        ..Default::default()
    };

    ProtoMetric {
        name: name.into(),
        description: desc.into(),
        unit: unit.into(),
        data: Some(Data::Gauge(Gauge {
            data_points: vec![data_point],
        })),
        ..Default::default()
    }
}

#[cfg(feature = "otel-metrics")]
pub fn create_log_record(
    message: &str,
    severity_num: SeverityNumber,
    severity_text: &str,
    now_nanos: u64,
    attributes: &Vec<KeyValue>,
) -> LogRecord {
    LogRecord {
        time_unix_nano: now_nanos,
        observed_time_unix_nano: now_nanos,
        severity_number: severity_num.into(),
        severity_text: severity_text.into(),
        body: Some(AnyValue {
            value: Some(any_value::Value::StringValue(message.into())),
        }),
        attributes: attributes.clone(),
        dropped_attributes_count: 0,
        flags: 0,
        trace_id: vec![],
        span_id: vec![],
        ..Default::default()
    }
}

fn get_all_descendants(sys: &System, target_pid: Pid) -> HashSet<Pid> {
    let mut parent_to_children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (&pid, process) in sys.processes() {
        if let Some(parent_pid) = process.parent() {
            parent_to_children.entry(parent_pid).or_default().push(pid);
        }
    }

    let mut descendants = HashSet::new();
    descendants.insert(target_pid);
    let mut stack = vec![target_pid];
    while let Some(current_pid) = stack.pop() {
        if let Some(children) = parent_to_children.get(&current_pid) {
            for &child_pid in children {
                if descendants.insert(child_pid) {
                    stack.push(child_pid);
                }
            }
        }
    }

    descendants
}

pub async fn metrics_handler(
    child_id: usize,
    child_uuid: Uuid,
    metrics_tx: mpsc::Sender<(f64, u64, u64)>,
    writable_dir: PathBuf,
) {
    let mut sys = System::new();
    let refresh_kind = sysinfo::ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
        .without_tasks()
        .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
        .with_exe(sysinfo::UpdateKind::OnlyIfNotSet);
    let parent_pid = Pid::from(child_id);
    if !writable_dir.exists() {
        tokio::fs::create_dir_all(&writable_dir)
            .await
            .expect("Failed to create writable directory");
    }
    let metrics_json_path = writable_dir.join("metrics-current.json");
    #[cfg(feature = "otel-metrics")]
    let metrics_bin_path = writable_dir.join("metrics.bin");
    #[cfg(feature = "otel-metrics")]
    let mut metrics_file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(metrics_bin_path) // TODO: make generic over Tokio write trait
        .await
        .expect("blah");

    let mut last_30_min_cpu: VecDeque<f64> = VecDeque::with_capacity(360);
    let mut last_30_min_memory: VecDeque<u64> = VecDeque::with_capacity(360);

    loop {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind);
        if sys.process(parent_pid).is_none() {
            break;
        }

        let process_tree = get_all_descendants(&sys, parent_pid);
        let mut process_snapshots = Vec::with_capacity(process_tree.len());
        let mut total_cpu = 0.0;
        let mut total_memory = 0u64;
        let mut total_disk_read = 0u64;
        let mut total_disk_written = 0u64;
        for pid in process_tree.iter() {
            if let Some(proc) = sys.process(*pid) {
                let cpu_usage = proc.cpu_usage() as f64;
                let memory = proc.memory();
                let disk_usage = proc.disk_usage();

                tracing::debug!(
                    "{}: {}% CPU, {} bytes memory, {} bytes read, {} bytes written",
                    pid,
                    cpu_usage,
                    memory,
                    disk_usage.read_bytes,
                    disk_usage.written_bytes
                );

                total_cpu += cpu_usage;
                total_memory = total_memory.saturating_add(memory);
                total_disk_read = total_disk_read.saturating_add(disk_usage.read_bytes);
                total_disk_written = total_disk_written.saturating_add(disk_usage.written_bytes);

                let sep: OsString = " ".into();
                process_snapshots.push(ProcessResourceSnapshot {
                    pid: pid.as_u32(),
                    parent_pid: proc.parent().map(|p| p.as_u32()),
                    command: proc.cmd().join(&sep).into_string().unwrap_or_default(),
                    cpu_percent: cpu_usage,
                    memory_bytes: memory,
                    disk_read_bytes: disk_usage.read_bytes,
                    disk_written_bytes: disk_usage.written_bytes,
                });

                debug!("{pid}: {cpu_usage} {memory} {}", disk_usage.written_bytes);

                #[cfg(feature = "otel-metrics")]
                {
                    let now_nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as u64;

                    let id_attribute = KeyValue {
                        key: "sandbox_id".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(child_uuid.into())),
                        }),
                    };

                    let proc_id_attribute = KeyValue {
                        key: "sandbox_id".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::IntValue(pid.as_u32() as i64)),
                        }),
                    };

                    let proc_cmd_attribute = KeyValue {
                        key: "proc_cmd".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(
                                proc.cmd().join(&sep).into_string().unwrap(),
                            )),
                        }),
                    };

                    let attributes = vec![id_attribute, proc_cmd_attribute, proc_id_attribute];

                    let cpu_metric = create_gauge(
                        "sandbox.cpu.utilization",
                        "Current CPU utilization",
                        "%",
                        Value::AsDouble(cpu_usage),
                        now_nanos,
                        &attributes,
                    );
                    let memory_metric = create_gauge(
                        "sandbox.memory.usage",
                        "Current memory used",
                        "B",
                        Value::AsInt(memory as i64),
                        now_nanos,
                        &attributes,
                    );
                    // TODO: I think these were causing issues with the OpenTelemetry collector
                    // and I am not sure they are needed right now for OTEL metrics
                    // let disk_read_metric = create_gauge(
                    //     "sandbox.disk.read",
                    //     "Current disk read",
                    //     "B",
                    //     Value::AsInt(disk_usage.read_bytes as i64),
                    //     now_nanos,
                    //     &attributes,
                    // );
                    // let disk_written_metric = create_gauge(
                    //     "sandbox.written.read",
                    //     "Current disk written",
                    //     "B",
                    //     Value::AsInt(disk_usage.written_bytes as i64),
                    //     now_nanos,
                    //     &attributes,
                    // );

                    let scope_metrics = ProtoScopeMetrics {
                        scope: None,
                        metrics: vec![
                            cpu_metric,
                            memory_metric,
                            // disk_read_metric,
                            // disk_written_metric,
                        ],
                        schema_url: String::new(),
                    };

                    let resource_metrics = ProtoResourceMetrics {
                        resource: None,
                        scope_metrics: vec![scope_metrics],
                        schema_url: String::new(),
                    };

                    let request = ExportMetricsServiceRequest {
                        resource_metrics: vec![resource_metrics],
                    };

                    let mut buf = Vec::new();
                    if let Err(e) = request.encode(&mut buf) {
                        eprintln!("Failed to encode metrics: {}", e);
                        panic!("wtf");
                    }
                    metrics_file.write_all(&buf).await.expect("basl");
                }
            }
        }

        let _ = metrics_tx
            .send((total_cpu, total_memory, total_disk_written))
            .await;

        if !process_tree.is_empty() {
            last_30_min_cpu.push_back(total_cpu);
            last_30_min_memory.push_back(total_memory);
            if last_30_min_cpu.len() > 360 {
                last_30_min_cpu.pop_front();
            }
            if last_30_min_memory.len() > 360 {
                last_30_min_memory.pop_front();
            }

            let snapshot = ModeResourceSnapshot {
                mode_id: AutonomyModeId(child_uuid),
                timestamp_unix_ms: (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis()) as u64,
                cpu_percent: total_cpu,
                memory_bytes: total_memory,
                disk_read_bytes: total_disk_read,
                disk_written_bytes: total_disk_written,
                process_count: process_tree.len() as u32,
                processes: process_snapshots,
                min_cpu_30_min: last_30_min_cpu
                    .iter()
                    .cloned()
                    .filter(|m| *m > 0.0)
                    .fold(f64::INFINITY, f64::min),
                max_cpu_30_min: last_30_min_cpu
                    .iter()
                    .cloned()
                    .fold(f64::NEG_INFINITY, f64::max),
                avg_cpu_30_min: last_30_min_cpu.iter().cloned().sum::<f64>()
                    / (last_30_min_cpu.len() as f64),
                min_memory_30_min: last_30_min_memory
                    .iter()
                    .cloned()
                    .filter(|m| *m > 0)
                    .min()
                    .unwrap_or(0),
                max_memory_30_min: last_30_min_memory.iter().cloned().max().unwrap_or(0),
                avg_memory_30_min: last_30_min_memory.iter().cloned().sum::<u64>()
                    / (last_30_min_memory.len() as u64),
            };

            if let Ok(json) = serde_json::to_vec_pretty(&snapshot) {
                let _ = tokio::fs::write(&metrics_json_path, json).await;
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
