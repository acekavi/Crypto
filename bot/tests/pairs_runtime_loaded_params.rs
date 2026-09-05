use std::fs;
use std::path::PathBuf;

use bot::pairs_report::{read_runtime_state, runtime_snapshot_path, LoadedParamsView};
use serde_json::json;
use tempfile::TempDir;

fn seed_snapshot(dir: &TempDir, body: serde_json::Value) -> PathBuf {
    let db_path = dir.path().join("bot.db");
    let snap = runtime_snapshot_path(db_path.to_str().unwrap());
    fs::write(&snap, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    db_path
}

#[test]
fn runtime_snapshot_reader_preserves_loaded_params() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = seed_snapshot(&dir, json!({
        "aave_eth": {
            "last_bar_ms": 1700000000000i64,
            "last_loop_wall_time": 1700000000.0,
            "last_seen_z": 1.25,
            "last_seen_signal": "short_spread",
            "last_guard_reason": null,
            "position": null,
            "loaded_params": {
                "rolling_window": 210,
                "entry_z": "3",
                "stop_z": "4.5",
                "target_z": "0",
                "max_hold_bars": 48,
                "risk_pct_of_equity": "0.03"
            },
            "snapshot_age_s": null,
            "snapshot_stale": false
        }
    }));
    let got = read_runtime_state(db_path.to_str().unwrap(), "aave_eth", 600.0).unwrap();
    assert_eq!(
        got.loaded_params,
        Some(LoadedParamsView {
            rolling_window: 210,
            entry_z: "3".into(),
            stop_z: "4.5".into(),
            target_z: "0".into(),
            max_hold_bars: 48,
            risk_pct_of_equity: "0.03".into(),
        })
    );
}
