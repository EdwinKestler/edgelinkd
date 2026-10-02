use std::path::PathBuf;

pub fn ui_static_dir() -> PathBuf {
    let mut candidates = vec![
        PathBuf::from("target").join("ui_static"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui_static"),
        PathBuf::from("../../target/ui_static"),
    ];
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("ui_static"));
        if let Some(target) = exe_dir.parent() {
            candidates.push(target.join("ui_static"));
        }
    }
    for candidate in candidates {
        if candidate.join("nodes").is_dir() {
            return candidate;
        }
    }
    PathBuf::from("target").join("ui_static")
}
