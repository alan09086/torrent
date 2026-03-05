use std::process::Command;

#[test]
fn test_no_args_shows_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .output()
        .expect("failed to run torrent");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Usage") || stderr.contains("usage"),
        "expected usage text, got: {stderr}"
    );
}

#[test]
fn test_help_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .arg("--help")
        .output()
        .expect("failed to run torrent");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("download"));
    assert!(stdout.contains("create"));
    assert!(stdout.contains("info"));
}

#[test]
fn test_download_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args(["download", "--help"])
        .output()
        .expect("failed to run torrent");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--output"));
    assert!(stdout.contains("--seed"));
    assert!(stdout.contains("--port"));
}

#[test]
fn test_create_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args(["create", "--help"])
        .output()
        .expect("failed to run torrent");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--tracker"));
    assert!(stdout.contains("--private"));
}

#[test]
fn test_info_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args(["info", "--help"])
        .output()
        .expect("failed to run torrent");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lower = stdout.to_lowercase();
    assert!(lower.contains("path"), "expected 'path' in info help, got: {stdout}");
}

#[test]
fn test_info_nonexistent_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args(["info", "/tmp/nonexistent_torrent_test_12345.torrent"])
        .output()
        .expect("failed to run torrent");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to read") || stderr.contains("Error"),
        "expected error message, got: {stderr}"
    );
}

#[test]
fn test_create_and_info_roundtrip() {
    let dir = std::env::temp_dir().join("torrent_cli_test_create");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let test_file = dir.join("test.txt");
    std::fs::write(&test_file, "hello torrent").unwrap();

    let torrent_path = dir.join("test.torrent");

    // Create torrent
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args([
            "create",
            test_file.to_str().unwrap(),
            "-o",
            torrent_path.to_str().unwrap(),
            "-t",
            "http://tracker.example.com/announce",
        ])
        .output()
        .expect("failed to run torrent create");
    assert!(output.status.success(), "create failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(torrent_path.exists());

    // Info on created torrent
    let output = Command::new(env!("CARGO_BIN_EXE_torrent"))
        .args(["info", torrent_path.to_str().unwrap()])
        .output()
        .expect("failed to run torrent info");
    assert!(output.status.success(), "info failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("test.txt"), "expected filename in info output, got: {stdout}");
    assert!(stdout.contains("tracker.example.com"), "expected tracker in info output, got: {stdout}");

    let _ = std::fs::remove_dir_all(&dir);
}
