## Status

### `safe`
- The router is currently a rats nest of fan-out and fan-in comms. We need to re-architect this
  - This will likely require thta we use mpsc or mpmc to wire things together within Router
- Need to turn the various constructs into a framework with configurable Transport (among other things) so `safe` starts to feel composable
- See `TODO` comments

### `safectl`
- Hardcoded `rx`/`tx` for testing only and most branches of the CLI are unimplemented