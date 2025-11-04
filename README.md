# Sedaro Autonomy Framework for Edge (SAFE)

Sedaro Autonomy Framework for Edge (SAFE) is an open-source flight software autonomy framework that integrates Sedaro's Edge Deployable Simulators (EDS's) alongside third-party software to achieve trusted satellite autonomy across the mission lifecycle.

![SAFE](safe.png)

When paired with Sedaro EDS's, SAFE delivers trusted satellite autonomy over long durations without ground contact. SAFE realizes a flexible framework that can be readily applied to a diverse array of missions and edge compute devices.  Features:

1. Autonomy modes offering various levels of intelligence and risk posture interface to core flight software using a built in native C2 interface to consume current system state from telemetry and issue commands to their host vehicle. 
2. A Router sets the active autonomy mode based on telemetry so that developers and mission planners can design an array of modes that incorporate various autonomy approaches for each mission phase, potential state of the vehicle, and potential state of its operating
environment. 
3. Batteries-included developer libraries for multi-simulation, multi-parameter optimization and risk analysis, in addition to turnkey support for the integration of EDS’s, enables streamlined development of SAFE autonomy modes.

#### Getting Started

```bash
cd examples
cargo run
```


#### Project Layout

- [`safe`](./safe/): SAFE implementation
  - [`sedaro`](./safe/sedaro/): Utilities and drivers for integrating [Sedaro](https://sedaro.com) Edge Deployable Simulators (EDS)
- [`safectl`](./safectl/): A CLI for interacting with a running SAFE process
- [`examples`](./examples/): Example autonomy implementations and reference designs


#### License

This project is licensed under the [Apache-2.0 License](./LICENSE).