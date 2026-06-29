use std::env::var;
use std::path::{Path, PathBuf};

const RUNTIME_CONFIG_CWD_CANDIDATE: &str = "safe/safe.yaml";
const RUNTIME_CONFIG_FALLBACK: &str = "/opt/safe/safe.yaml";
const MODE_CONFIG_CWD_CANDIDATE: &str = "safe/autonomy_mode_config.json";
const MODE_CONFIG_FALLBACK: &str = "/opt/safe/autonomy_mode_config.json";

pub(crate) fn resolve_runtime_config_path() -> PathBuf {
    var("SAFE_RUNTIME_CONFIG")
        .or_else(|_| var("SAFE_RUNTIME_CONFIG_PATH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_runtime_config_path())
}

pub(crate) fn resolve_autonomy_mode_config_path() -> PathBuf {
    var("SAFE_AUTONOMY_MODE_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_mode_config_path())
}

pub(crate) fn resolve_path_from_base(base_dir: &Path, path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    base_dir.join(path)
}

fn default_runtime_config_path() -> PathBuf {
    let cwd_candidate = PathBuf::from(RUNTIME_CONFIG_CWD_CANDIDATE);
    if cwd_candidate.exists() {
        return cwd_candidate;
    }
    PathBuf::from(RUNTIME_CONFIG_FALLBACK)
}

fn default_mode_config_path() -> PathBuf {
    let cwd_candidate = PathBuf::from(MODE_CONFIG_CWD_CANDIDATE);
    if cwd_candidate.exists() {
        return cwd_candidate;
    }
    PathBuf::from(MODE_CONFIG_FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_from_base_keeps_empty_path_empty() {
        let out = resolve_path_from_base(Path::new("/tmp/base"), Path::new(""));
        assert!(out.as_os_str().is_empty());
    }

    #[test]
    fn resolve_path_from_base_resolves_relative_paths() {
        let out = resolve_path_from_base(Path::new("/tmp/base"), Path::new("a/b"));
        assert_eq!(out, PathBuf::from("/tmp/base/a/b"));
    }

    #[test]
    fn resolve_path_from_base_preserves_absolute_paths() {
        let out = resolve_path_from_base(Path::new("/tmp/base"), Path::new("/x/y"));
        assert_eq!(out, PathBuf::from("/x/y"));
    }
}
