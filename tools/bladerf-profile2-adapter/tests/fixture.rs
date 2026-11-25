use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use bladerf_profile2_ipc::{
    FixtureClient, PROFILE2_CAPABILITIES, PROFILE2_ID, ReadRegister, ShadowChannel,
};
use bladerf_profile2_loader_ipc::{
    ErrorCode as LoaderErrorCode, Request as LoaderRequest,
    RequestOperation as LoaderRequestOperation, Response as LoaderResponse,
    ResponseKind as LoaderResponseKind, ServerMode as LoaderServerMode,
    read_frame as read_loader_frame, write_frame as write_loader_frame,
};
use bladerf_profile2_recovery_ipc::{
    ErrorCode as RecoveryErrorCode, Request as RecoveryRequest,
    RequestOperation as RecoveryRequestOperation, Response as RecoveryResponse,
    ResponseKind as RecoveryResponseKind, ServerMode as RecoveryServerMode,
    read_frame as read_recovery_frame, write_frame as write_recovery_frame,
};
use bladerf_profile2_session_ipc::{
    ErrorCode as SessionErrorCode, Request as SessionRequest,
    RequestOperation as SessionRequestOperation, Response as SessionResponse,
    ResponseKind as SessionResponseKind, ServerMode as SessionServerMode,
    read_frame as read_session_frame, write_frame as write_session_frame,
};

#[test]
fn client_runs_a_complete_typed_fixture_transaction_and_shuts_down() {
    let mut client = FixtureClient::spawn(
        env!("CARGO_BIN_EXE_bladerf-profile2-adapter"),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(client.read_register(ReadRegister::Id).unwrap(), PROFILE2_ID);
    assert_eq!(
        client.read_register(ReadRegister::Capabilities).unwrap(),
        PROFILE2_CAPABILITIES
    );

    let channels = [
        ShadowChannel::Channel0,
        ShadowChannel::Channel1,
        ShadowChannel::Channel2,
        ShadowChannel::Channel3,
    ];
    let shadows = [
        ReadRegister::Shadow0,
        ReadRegister::Shadow1,
        ReadRegister::Shadow2,
        ReadRegister::Shadow3,
    ];
    let applied = [
        ReadRegister::Applied0,
        ReadRegister::Applied1,
        ReadRegister::Applied2,
        ReadRegister::Applied3,
    ];
    let words = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
    for ((channel, register), word) in channels.into_iter().zip(shadows).zip(words) {
        client.write_shadow(channel, word).unwrap();
        assert_eq!(client.read_register(register).unwrap(), word);
    }
    client.commit().unwrap();
    assert_eq!(
        client.read_register(ReadRegister::Status).unwrap(),
        0x8000_0000
    );
    assert_eq!(
        client.read_register(ReadRegister::Status).unwrap(),
        0x6000_0000
    );
    assert_eq!(client.read_register(ReadRegister::OutcomeEpoch).unwrap(), 1);
    assert_eq!(client.read_register(ReadRegister::AppliedEpoch).unwrap(), 1);
    for (register, word) in applied.into_iter().zip(words) {
        assert_eq!(client.read_register(register).unwrap(), word);
    }
    client.shutdown().unwrap();
}

#[test]
fn help_and_invalid_invocation_do_not_start_a_native_session() {
    let help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("native register backend"));
    assert!(help_text.contains("fixture"));
    assert!(help_text.contains("native"));
    assert!(help_text.contains("check-native-library"));
    assert!(help_text.contains("check-profile2-images"));
    assert!(help_text.contains("check-profile2-loader-worker"));
    assert!(help_text.contains("check-profile2-session-worker"));
    assert!(help_text.contains("check-profile2-loader-process"));
    assert!(help_text.contains("check-profile2-recovery-process"));
    assert!(help_text.contains("check-profile2-session-process"));
    assert!(help_text.contains("loader-fixture-primary"));
    assert!(help_text.contains("loader-fixture-recovery"));
    assert!(!help_text.contains("session-fixture-primary"));
    assert!(!help_text.contains("session-native-primary"));
    assert!(!help_text.contains("recovery-fixture"));
    assert!(!help_text.contains("recovery-native"));

    let library_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-native-library", "--help"])
        .output()
        .unwrap();
    assert!(library_help.status.success());
    let library_help_text = String::from_utf8(library_help.stdout).unwrap();
    assert!(library_help_text.contains("--require-bladerf2-config-neutral-open"));
    assert!(library_help_text.contains("--require-bladerf2-loader-neutral-open"));

    let native_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["native", "--help"])
        .output()
        .unwrap();
    assert!(native_help.status.success());
    let native_help_text = String::from_utf8(native_help.stdout).unwrap();
    assert!(native_help_text.contains("claims the selected USB device"));
    assert!(native_help_text.contains("--library"));
    assert!(native_help_text.contains("--sha256"));
    assert!(native_help_text.contains("--serial"));

    let image_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-images", "--help"])
        .output()
        .unwrap();
    assert!(image_help.status.success());
    let image_help_text = String::from_utf8(image_help.stdout).unwrap();
    assert!(image_help_text.contains("without opening a device"));
    assert!(image_help_text.contains("--custom-image"));
    assert!(image_help_text.contains("--custom-sha256"));
    assert!(image_help_text.contains("--custom-bytes"));
    assert!(image_help_text.contains("--stock-image"));

    let loader_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-loader-worker", "--help"])
        .output()
        .unwrap();
    assert!(loader_help.status.success());
    let loader_help_text = String::from_utf8(loader_help.stdout).unwrap();
    assert!(loader_help_text.contains("fake calls only"));
    assert!(loader_help_text.contains("--library"));
    assert!(loader_help_text.contains("--sha256"));
    assert!(loader_help_text.contains("--custom-image"));
    assert!(loader_help_text.contains("--custom-sha256"));
    assert!(loader_help_text.contains("--custom-bytes"));
    assert!(loader_help_text.contains("--stock-image"));

    let session_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-session-worker", "--help"])
        .output()
        .unwrap();
    assert!(session_help.status.success());
    let session_help_text = String::from_utf8(session_help.stdout).unwrap();
    assert!(session_help_text.contains("receive-session worker"));
    assert!(session_help_text.contains("fake calls only"));
    assert!(session_help_text.contains("--library"));
    assert!(session_help_text.contains("--sha256"));
    assert!(session_help_text.contains("--custom-image"));
    assert!(session_help_text.contains("--custom-sha256"));
    assert!(session_help_text.contains("--custom-bytes"));
    assert!(session_help_text.contains("--stock-image"));
    assert!(!session_help_text.contains("--serial"));

    let primary_loader_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["loader-fixture-primary", "--help"])
        .output()
        .unwrap();
    assert!(primary_loader_help.status.success());
    let primary_loader_help = String::from_utf8(primary_loader_help.stdout).unwrap();
    assert!(primary_loader_help.contains("fake primary loader"));
    assert!(primary_loader_help.contains("--library"));
    assert!(primary_loader_help.contains("--custom-image"));
    assert!(primary_loader_help.contains("--stock-image"));
    assert!(!primary_loader_help.contains("--serial"));

    let recovery_loader_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["loader-fixture-recovery", "--help"])
        .output()
        .unwrap();
    assert!(recovery_loader_help.status.success());
    let recovery_loader_help = String::from_utf8(recovery_loader_help.stdout).unwrap();
    assert!(recovery_loader_help.contains("stock-recovery loader"));
    assert!(recovery_loader_help.contains("--library"));
    assert!(recovery_loader_help.contains("--stock-image"));
    assert!(!recovery_loader_help.contains("--custom-image"));
    assert!(!recovery_loader_help.contains("--serial"));

    let process_check_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-loader-process", "--help"])
        .output()
        .unwrap();
    assert!(process_check_help.status.success());
    let process_check_help = String::from_utf8(process_check_help.stdout).unwrap();
    assert!(process_check_help.contains("primary and recovery subprocesses"));
    assert!(process_check_help.contains("--library"));
    assert!(process_check_help.contains("--custom-image"));
    assert!(process_check_help.contains("--stock-image"));
    assert!(!process_check_help.contains("--serial"));

    let session_process_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-session-process", "--help"])
        .output()
        .unwrap();
    assert!(session_process_help.status.success());
    let session_process_help = String::from_utf8(session_process_help.stdout).unwrap();
    assert!(session_process_help.contains("complete session subprocess"));
    assert!(session_process_help.contains("model calls only"));
    assert!(session_process_help.contains("--library"));
    assert!(session_process_help.contains("--custom-image"));
    assert!(session_process_help.contains("--stock-image"));
    assert!(session_process_help.contains("--maximum-pipe-stress"));
    assert!(session_process_help.contains("Device-free maximum"));
    assert!(session_process_help.contains("320,000,032 bytes"));
    assert!(session_process_help.contains("anonymous file"));
    assert!(!session_process_help.contains("--serial"));
    assert!(!session_process_help.contains("--device"));

    let hidden_session_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["session-fixture-primary", "--help"])
        .output()
        .unwrap();
    assert!(hidden_session_help.status.success());
    let hidden_session_help = String::from_utf8(hidden_session_help.stdout).unwrap();
    assert!(hidden_session_help.contains("complete fake session"));
    assert!(hidden_session_help.contains("--library"));
    assert!(hidden_session_help.contains("--custom-image"));
    assert!(hidden_session_help.contains("--stock-image"));
    assert!(!hidden_session_help.contains("--serial"));
    assert!(!hidden_session_help.contains("--device"));

    let hidden_native_session_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["session-native-primary", "--help"])
        .output()
        .unwrap();
    assert!(hidden_native_session_help.status.success());
    let hidden_native_session_help = String::from_utf8(hidden_native_session_help.stdout).unwrap();
    assert!(hidden_native_session_help.contains("opens the selected USB device"));
    assert!(hidden_native_session_help.contains("RAM-load"));
    assert!(hidden_native_session_help.contains("configure RX"));
    assert!(hidden_native_session_help.contains("capture"));
    assert!(hidden_native_session_help.contains("--library"));
    assert!(hidden_native_session_help.contains("--custom-image"));
    assert!(hidden_native_session_help.contains("--stock-image"));
    assert!(hidden_native_session_help.contains("--serial"));
    assert!(!hidden_native_session_help.contains("--device"));

    let recovery_process_help = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["check-profile2-recovery-process", "--help"])
        .output()
        .unwrap();
    assert!(recovery_process_help.status.success());
    let recovery_process_help = String::from_utf8(recovery_process_help.stdout).unwrap();
    assert!(recovery_process_help.contains("fresh-recovery subprocess"));
    assert!(recovery_process_help.contains("fake calls only"));
    assert!(recovery_process_help.contains("--library"));
    assert!(recovery_process_help.contains("--stock-image"));
    assert!(!recovery_process_help.contains("--custom-image"));
    assert!(!recovery_process_help.contains("--serial"));

    let hidden_recovery_fixture = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["recovery-fixture", "--help"])
        .output()
        .unwrap();
    assert!(hidden_recovery_fixture.status.success());
    let hidden_recovery_fixture = String::from_utf8(hidden_recovery_fixture.stdout).unwrap();
    assert!(hidden_recovery_fixture.contains("stock-only fake recovery"));
    assert!(hidden_recovery_fixture.contains("--library"));
    assert!(hidden_recovery_fixture.contains("--stock-image"));
    assert!(!hidden_recovery_fixture.contains("--custom-image"));
    assert!(!hidden_recovery_fixture.contains("--serial"));

    let hidden_recovery_native = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args(["recovery-native", "--help"])
        .output()
        .unwrap();
    assert!(hidden_recovery_native.status.success());
    let hidden_recovery_native = String::from_utf8(hidden_recovery_native.stdout).unwrap();
    assert!(hidden_recovery_native.contains("selected USB"));
    assert!(hidden_recovery_native.contains("RAM-loading"));
    assert!(hidden_recovery_native.contains("--library"));
    assert!(hidden_recovery_native.contains("--stock-image"));
    assert!(hidden_recovery_native.contains("--serial"));
    assert!(!hidden_recovery_native.contains("--custom-image"));
    assert!(!hidden_recovery_native.contains("--device"));

    let invalid = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .arg("native")
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let diagnostic = String::from_utf8(invalid.stderr).unwrap();
    assert!(diagnostic.contains("required arguments"));
    assert!(!diagnostic.contains("bladerf_open"));

    let conflicting_contracts = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-native-library",
            "--library",
            "/this/path/must/not-be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--require-bladerf2-config-neutral-open",
            "--require-bladerf2-loader-neutral-open",
        ])
        .output()
        .unwrap();
    assert!(!conflicting_contracts.status.success());
    assert!(conflicting_contracts.stdout.is_empty());
    let diagnostic = String::from_utf8(conflicting_contracts.stderr).unwrap();
    assert!(
        diagnostic.contains("--require-bladerf2-config-neutral-open"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("--require-bladerf2-loader-neutral-open"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
}

#[cfg(target_os = "linux")]
#[test]
fn malformed_image_expectation_fails_before_path_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-profile2-images",
            "--custom-image",
            "/this/path/must/not/be-opened/custom.rbf",
            "--custom-sha256",
            "not-a-sha256",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/this/path/must/not/be-opened/stock.rbf",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn malformed_loader_worker_image_expectation_precedes_library_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-profile2-loader-worker",
            "--library",
            "/this/path/must/not-be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--custom-image",
            "/this/path/must/not-be-opened/custom.rbf",
            "--custom-sha256",
            "not-a-sha256",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/this/path/must/not-be-opened/stock.rbf",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");

    let aliased_roles = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-profile2-loader-worker",
            "--library",
            "/this/path/must/not-be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--custom-image",
            "/this/path/must/not-be-opened/custom.rbf",
            "--custom-sha256",
            "3004467110e2b2790c8f6128f998c2149905a88eed91a87f11af3db524d60c65",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/this/path/must/not-be-opened/stock.rbf",
        ])
        .output()
        .unwrap();

    assert!(!aliased_roles.status.success());
    assert!(aliased_roles.stdout.is_empty());
    let diagnostic = String::from_utf8(aliased_roles.stderr).unwrap();
    assert!(diagnostic.contains("must differ"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn malformed_session_worker_image_expectation_precedes_library_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-profile2-session-worker",
            "--library",
            "/this/path/must/not-be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--custom-image",
            "/this/path/must/not-be-opened/custom.rbf",
            "--custom-sha256",
            "not-a-sha256",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/this/path/must/not-be-opened/stock.rbf",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn malformed_session_process_image_expectation_precedes_library_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-profile2-session-process",
            "--library",
            "/this/path/must/not-be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--custom-image",
            "/this/path/must/not-be-opened/custom.rbf",
            "--custom-sha256",
            "not-a-sha256",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/this/path/must/not-be-opened/stock.rbf",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn native_eof_does_not_read_the_library_or_open_a_device() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "native",
            "--library",
            "/this/path/must/not/be-opened/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--serial",
            "0123456789abcdef0123456789abcdef",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("input ended before explicit shutdown"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn malformed_native_library_hash_fails_before_file_or_device_access() {
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"))
        .args([
            "check-native-library",
            "--library",
            "/this/path/must/not/be-opened/libbladeRF.so.2",
            "--sha256",
            "not-a-sha256",
            "--require-bladerf2-config-neutral-open",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

fn primary_loader_command(custom_sha256: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "loader-fixture-primary",
        "--library",
        "/this/path/must/not/be-opened/libbladeRF.so.2",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--custom-image",
        "/this/path/must/not/be-opened/custom.rbf",
        "--custom-sha256",
        custom_sha256,
        "--custom-bytes",
        "12858972",
        "--stock-image",
        "/this/path/must/not/be-opened/stock.rbf",
    ]);
    command
}

fn recovery_loader_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "loader-fixture-recovery",
        "--library",
        "/this/path/must/not/be-opened/libbladeRF.so.2",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--stock-image",
        "/this/path/must/not/be-opened/stock.rbf",
    ]);
    command
}

fn raw_loader_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

fn session_fixture_command(custom_sha256: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "session-fixture-primary",
        "--library",
        "/this/path/must/not-be-opened/libbladeRF.so.2",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--custom-image",
        "/this/path/must/not-be-opened/custom.rbf",
        "--custom-sha256",
        custom_sha256,
        "--custom-bytes",
        "12858972",
        "--stock-image",
        "/this/path/must/not-be-opened/stock.rbf",
    ]);
    command
}

fn native_session_command(serial: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "session-native-primary",
        "--library",
        "/this/path/must/not/be-opened/libbladeRF.so.2",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--custom-image",
        "/this/path/must/not/be-opened/custom.rbf",
        "--custom-sha256",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "--custom-bytes",
        "12858972",
        "--stock-image",
        "/this/path/must/not/be-opened/stock.rbf",
        "--serial",
        serial,
    ]);
    command
}

fn recovery_fixture_command(library_sha256: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "recovery-fixture",
        "--library",
        "/this/path/must/not-be-opened/libbladeRF.so.2",
        "--sha256",
        library_sha256,
        "--stock-image",
        "/this/path/must/not-be-opened/stock.rbf",
    ]);
    command
}

fn native_recovery_command(serial: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bladerf-profile2-adapter"));
    command.args([
        "recovery-native",
        "--library",
        "/this/path/must/not-be-opened/libbladeRF.so.2",
        "--sha256",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--stock-image",
        "/this/path/must/not-be-opened/stock.rbf",
        "--serial",
        serial,
    ]);
    command
}

#[test]
fn dedicated_recovery_eof_does_not_access_any_path_or_device() {
    for output in [
        recovery_fixture_command(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .output()
        .unwrap(),
        native_recovery_command("0123456789abcdef0123456789abcdef")
            .output()
            .unwrap(),
    ] {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(
            diagnostic.contains("recovery input ended before explicit shutdown"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("No such file"), "{diagnostic}");
        assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
    }
}

#[test]
fn wrong_dedicated_recovery_family_is_rejected_before_path_or_device_access() {
    let mut command = native_recovery_command("0123456789abcdef0123456789abcdef");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&raw_loader_frame(
            br#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"hello","mode":"native_recovery"}}"#,
        ))
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("decode recovery frame"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn wrong_native_recovery_mode_is_rejected_before_path_or_device_access() {
    let mut command = native_recovery_command("0123456789abcdef0123456789abcdef");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_recovery_frame(
        child.stdin.as_mut().unwrap(),
        &RecoveryRequest::new(
            1,
            RecoveryRequestOperation::Hello {
                mode: RecoveryServerMode::FixtureRecovery,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: RecoveryResponse = read_recovery_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(response.id(), 1);
    assert!(matches!(
        response.result(),
        RecoveryResponseKind::Error {
            code: RecoveryErrorCode::Protocol,
            ..
        }
    ));
    assert!(
        read_recovery_frame::<_, RecoveryResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn malformed_native_recovery_serial_fails_before_path_or_device_access() {
    let mut command = native_recovery_command("BAD-SERIAL");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_recovery_frame(
        child.stdin.as_mut().unwrap(),
        &RecoveryRequest::new(
            1,
            RecoveryRequestOperation::Hello {
                mode: RecoveryServerMode::NativeRecovery,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: RecoveryResponse = read_recovery_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(response.id(), 1);
    assert!(matches!(
        response.result(),
        RecoveryResponseKind::Error {
            code: RecoveryErrorCode::Backend,
            message,
        } if message.contains("serial") && !message.contains("BAD-SERIAL")
    ));
    assert!(
        read_recovery_frame::<_, RecoveryResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("serial"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn exact_recovery_hello_correlates_static_validation_failure() {
    let mut command = recovery_fixture_command("not-a-sha256");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_recovery_frame(
        child.stdin.as_mut().unwrap(),
        &RecoveryRequest::new(
            1,
            RecoveryRequestOperation::Hello {
                mode: RecoveryServerMode::FixtureRecovery,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: RecoveryResponse = read_recovery_frame(&mut stdout).unwrap().unwrap();
    assert!(matches!(
        response.result(),
        RecoveryResponseKind::Error {
            code: RecoveryErrorCode::Backend,
            message,
        } if message.contains("SHA-256") && !message.contains("not-a-sha256")
    ));
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn session_fixture_eof_does_not_access_any_bundle_path() {
    let output =
        session_fixture_command("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .output()
            .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("session input ended before explicit shutdown"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn wrong_session_family_is_rejected_before_bundle_access() {
    let mut command =
        session_fixture_command("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&raw_loader_frame(
            br#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
        ))
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("decode session frame"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn native_session_eof_does_not_access_any_path_or_device() {
    let output = native_session_command("0123456789abcdef0123456789abcdef")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("session input ended before explicit shutdown"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn wrong_native_session_family_is_rejected_before_path_or_device_access() {
    let mut command = native_session_command("0123456789abcdef0123456789abcdef");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&raw_loader_frame(
            br#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"hello","mode":"native_primary"}}"#,
        ))
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("decode session frame"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn wrong_native_session_mode_is_rejected_before_path_or_device_access() {
    let mut command = native_session_command("0123456789abcdef0123456789abcdef");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_session_frame(
        child.stdin.as_mut().unwrap(),
        &SessionRequest::new(
            1,
            SessionRequestOperation::Hello {
                mode: SessionServerMode::FixturePrimary,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: SessionResponse = read_session_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(response.id(), 1);
    assert!(matches!(
        response.result(),
        SessionResponseKind::Error {
            code: SessionErrorCode::InvalidState,
            ..
        }
    ));
    assert!(
        read_session_frame::<_, SessionResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn malformed_native_session_serial_fails_before_path_or_device_access() {
    let mut command = native_session_command("BAD-SERIAL");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_session_frame(
        child.stdin.as_mut().unwrap(),
        &SessionRequest::new(
            1,
            SessionRequestOperation::Hello {
                mode: SessionServerMode::NativePrimary,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: SessionResponse = read_session_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(response.id(), 1);
    assert!(matches!(
        response.result(),
        SessionResponseKind::Error {
            code: SessionErrorCode::Backend,
            message,
        } if message.contains("serial")
    ));
    assert!(
        read_session_frame::<_, SessionResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("serial"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn loader_fixture_eof_does_not_access_any_bundle_path() {
    let primary =
        primary_loader_command("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .output()
            .unwrap();
    assert!(!primary.status.success());
    assert!(primary.stdout.is_empty());
    let diagnostic = String::from_utf8(primary.stderr).unwrap();
    assert!(
        diagnostic.contains("loader input ended before explicit shutdown"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");

    let recovery = recovery_loader_command().output().unwrap();
    assert!(!recovery.status.success());
    assert!(recovery.stdout.is_empty());
    let diagnostic = String::from_utf8(recovery.stderr).unwrap();
    assert!(
        diagnostic.contains("loader input ended before explicit shutdown"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn wrong_loader_mode_is_rejected_before_bundle_access() {
    let mut command =
        primary_loader_command("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_loader_frame(
        child.stdin.as_mut().unwrap(),
        &LoaderRequest::new(
            1,
            LoaderRequestOperation::Hello {
                mode: LoaderServerMode::FixtureRecovery,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: LoaderResponse = read_loader_frame(&mut stdout).unwrap().unwrap();
    assert!(matches!(
        response.result(),
        LoaderResponseKind::Error {
            code: LoaderErrorCode::InvalidState,
            ..
        }
    ));
    assert!(
        read_loader_frame::<_, LoaderResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}

#[test]
fn missing_or_wrong_loader_family_is_rejected_before_bundle_access() {
    for payload in [
        br#"{"version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#.as_slice(),
        br#"{"family":"wrong","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#.as_slice(),
    ] {
        let mut command = primary_loader_command(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&raw_loader_frame(payload))
            .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.contains("decode loader protocol frame"), "{diagnostic}");
        assert!(!diagnostic.contains("No such file"), "{diagnostic}");
        assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
    }
}

#[test]
fn exact_loader_hello_correlates_static_validation_failure() {
    let mut command = primary_loader_command("not-a-sha256");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    write_loader_frame(
        child.stdin.as_mut().unwrap(),
        &LoaderRequest::new(
            1,
            LoaderRequestOperation::Hello {
                mode: LoaderServerMode::FixturePrimary,
            },
        ),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let mut stdout = output.stdout.as_slice();
    let response: LoaderResponse = read_loader_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(response.id(), 1);
    assert!(matches!(
        response.result(),
        LoaderResponseKind::Error {
            code: LoaderErrorCode::Backend,
            message,
        } if message.contains("SHA-256")
    ));
    assert!(
        read_loader_frame::<_, LoaderResponse>(&mut stdout)
            .unwrap()
            .is_none()
    );
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
    assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    assert!(!diagnostic.contains("bladerf_open"), "{diagnostic}");
}
