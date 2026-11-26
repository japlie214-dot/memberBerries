use std::process::Command;

fn memb(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_memb"))
        .args(args)
        .env("MEMB_DATA_DIR", dir)
        .env_remove("MEMB_GEMMA_MODEL_DIR")
        .env_remove("MEMB_EMBED_PORT")
        .env("MEMB_EMBED_SERVICE", "0")
        .output()
        .unwrap()
}

#[test]
fn status_and_stop_report_a_service_that_is_not_running_and_a_service_needs_a_model() {
    let dir = std::env::temp_dir().join(format!("memb-embsvc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = memb(&dir, &["embed-service", "status"]);
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("not running"));
    let out = memb(&dir, &["embed-service", "stop"]);
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("not running"));
    let out = memb(&dir, &["embed-service"]);
    assert_eq!(out.status.code(), Some(1), "no model configured, nothing to serve");
    assert!(String::from_utf8_lossy(&out.stderr).contains("memb models install"));
    assert_eq!(memb(&dir, &["embed-service", "bogus"]).status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}
