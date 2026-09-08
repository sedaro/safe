# SAFE Observability Stack

This starts Prometheus, an OpenTelemetry Collector, and Grafana. Prometheus is
configured to accept remote-write data from the Collector, and Grafana
automatically provisions the `SAFE Flight Metrics` dashboard.

Start the stack from this directory:

```bash
docker compose up -d
```

Replay a flight from the repository root:

```bash
./scripts/replay_metrics.py /path/to/metrics.bin
```

Open Grafana at <http://localhost:3000> with `admin` / `admin`. Prometheus is
available at <http://localhost:9090>, and the replay script sends OTLP/HTTP to
<http://localhost:4318/v1/metrics>.

The dashboard uses the Prometheus names generated from SAFE's metric names:
`sandbox_memory_usage_B` and `sandbox_cpu_utilization_percent`. Memory is
emitted once per process, so the total-memory panels sum the process series.

Stop the stack with:

```bash
docker compose down
```
