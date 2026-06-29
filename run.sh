export SAFE_AUTONOMY_MODE_CONFIG_PATH=$PWD/safe/autonomy_mode_config.json
export SAFE_USE_TELEM_TS_NOW=true
export SAFE_RUNTIME_CONFIG_PATH=$PWD/safe/safe.yaml
export SAFE_METRIC_BASE_PATH=/tmp/safe

RUST_LOG=info cargo run --bin safe
