use std::fs;
use std::process::Command;

#[test]
fn publishes_a_basename_output_in_the_current_directory() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("input.cf32"), vec![0u8; 4096 * 8]).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ieee802154-futuresdr-worker"))
        .current_dir(directory.path())
        .args([
            "input.cf32",
            "--output",
            "output.i154raw",
            "--channel",
            "17",
            "--max-input-mib",
            "1",
            "--max-frames",
            "16",
            "--max-output-mib",
            "1",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "worker failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let archive = fs::read(directory.path().join("output.i154raw")).unwrap();
    assert_eq!(archive.len(), 24);
    assert_eq!(&archive[..8], b"I154RAW\0");
    assert!(!directory.path().join("output.i154raw.partial").exists());
}
