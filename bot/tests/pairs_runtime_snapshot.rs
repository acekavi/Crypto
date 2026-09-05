use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use bot::pairs_report::{read_runtime_state, runtime_snapshot_path, RuntimeStateView};
use serde_json::json;
use tempfile::TempDir;

fn seed_snapshot(dir: &TempDir, body: serde_json::Value) -> PathBuf {
    let db_path = dir.path().join("bot.db");
    let snap = runtime_snapshot_path(db_path.to_str().unwrap());
    fs::write(&snap, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    db_path
}

#[test]
fn missing_snapshot_reads_as_empty_and_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bot.db");
    let got = read_runtime_state(db_path.to_str().unwrap(), "aave_eth", 600.0).unwrap();
    assert!(got.snapshot_stale);
    assert!(got.snapshot_age_s.is_none());
    assert!(got.last_bar_ms.is_none());
}

#[test]
fn snapshot_reader_preserves_runtime_fields_and_computes_age() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seed_snapshot(&dir, json!({
        "aave_eth": {
            "last_bar_ms": 1700000000000i64,
            "last_loop_wall_time": 1700000000.0,
            "last_seen_z": 1.25,
            "last_seen_signal": "short_spread",
            "last_guard_reason": null,
            "position": null,
            "snapshot_age_s": null,
            "snapshot_stale": false
        }
    }));
    let got = read_runtime_state(db_path.to_str().unwrap(), "aave_eth", 600.0).unwrap();
    assert_eq!(got.last_bar_ms, Some(1700000000000));
    assert_eq!(got.last_seen_signal.as_deref(), Some("short_spread"));
    assert!(got.snapshot_age_s.is_some());
    assert!(!got.snapshot_stale);
}

#[test]
fn old_snapshot_is_marked_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seed_snapshot(&dir, json!({
        "aave_eth": RuntimeStateView {
            last_bar_ms: Some(1700000000000),
            last_loop_wall_time: Some(1700000000.0),
            last_seen_z: Some(1.25),
            last_seen_signal: Some("short_spread".into()),
            last_guard_reason: None,
            position: None,
            loaded_params: None,
            snapshot_age_s: None,
            snapshot_stale: false,
        }
    }));
    let snap = runtime_snapshot_path(db_path.to_str().unwrap());
    let old = filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3600));
    filetime::set_file_mtime(&snap, old).unwrap();
    let got = read_runtime_state(db_path.to_str().unwrap(), "aave_eth", 600.0).unwrap();
    assert!(got.snapshot_stale);
    assert!(got.snapshot_age_s.unwrap() > 600.0);
}
