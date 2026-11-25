//! Isolated fixture, native backends and device-free qualification for profile 2.

#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod backend;
mod loader_fixture;
mod loader_process_check;
mod loader_server;
mod loader_worker;
mod native_abi;
mod recovery_fixture;
mod recovery_native;
mod recovery_process_check;
mod sealed_image;
mod server;
mod session_fixture;
mod session_native;
mod session_process_check;
mod session_server;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use bladerf_profile2_ipc::ServerMode as RegisterServerMode;
use bladerf_profile2_loader_ipc::ServerMode as LoaderServerMode;
use bladerf_profile2_recovery_ipc::ServerMode as RecoveryServerMode;
use bladerf_profile2_session_ipc::ServerMode as SessionServerMode;
use clap::{Parser, Subcommand};

use backend::{FixtureBackend, NativeBackend};
use loader_fixture::{PrimaryFixtureConfig, RecoveryFixtureConfig};
use loader_process_check::check_profile2_loader_process;
use recovery_fixture::RecoveryFixtureConfig as FreshRecoveryFixtureConfig;
use recovery_native::RecoveryNativeConfig;
use recovery_process_check::check_profile2_recovery_process;
use sealed_image::{CustomImage, ExpectedImage, StagedImages, StockImage};
use session_fixture::SessionFixtureConfig;
use session_native::SessionNativeConfig;
use session_process_check::{
    SessionProcessCheck, SessionProcessQualification, check_profile2_session_process,
};

#[derive(Debug, Parser)]
#[command(
    about = "Isolated profile-2 fixture, image staging, loader qualification, native register backend and trusted-library preflight"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the deterministic CPU register fixture over framed stdin/stdout.
    Fixture,
    /// Open one exact, already configured profile-2 xA9 and serve typed registers.
    ///
    /// This claims the selected USB device and changes its transport alternate
    /// settings. It does not load an FPGA image or configure RF, capture or TX.
    Native {
        /// Exact specialized libbladeRF shared-library file to qualify and retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Exact 32-character lowercase hexadecimal bladeRF serial.
        #[arg(long)]
        serial: String,
    },
    /// Serve a role-fixed fake primary loader after lazy bundle qualification.
    ///
    /// This command accepts no serial or device selector. It retains the exact
    /// loader-neutral table but runs only deterministic fake loader calls.
    LoaderFixturePrimary {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Serve a role-fixed fake stock-recovery loader with independent staging.
    ///
    /// This command accepts neither a custom image nor a serial/device selector.
    LoaderFixtureRecovery {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Serve one complete fake session after lazy exact-bundle qualification.
    ///
    /// This child entry point accepts no serial or device selector. It retains
    /// the exact loader-neutral table but gives its one worker only model calls.
    #[command(hide = true)]
    SessionFixturePrimary {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Serve one serial-selected native complete session over framed IPC.
    ///
    /// After an exact hello, this opens the selected USB device. Valid session
    /// requests can RAM-load custom and stock FPGA images, configure RX, and
    /// capture. No transmit, flash, bias-tee or loopback mutation is exposed.
    #[command(hide = true)]
    SessionNativePrimary {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
        /// Exact 32-character lowercase hexadecimal bladeRF serial.
        #[arg(long)]
        serial: String,
    },
    /// Serve one stock-only fake recovery worker over dedicated recovery IPC.
    ///
    /// This child accepts no serial or custom image. It stages stock freshly,
    /// retains B2L1, and gives its guarded worker deterministic model calls.
    #[command(hide = true)]
    RecoveryFixture {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Serve one serial-selected native stock-only recovery worker.
    ///
    /// After an exact native recovery Hello, this opens the selected USB
    /// device and permits RAM-loading only the frozen stock image. It exposes
    /// no custom image, register, RF, capture, transmit, flash or bias action.
    #[command(hide = true)]
    RecoveryNative {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
        /// Exact 32-character lowercase hexadecimal bladeRF serial.
        #[arg(long)]
        serial: String,
    },
    /// Exercise complete primary and recovery subprocesses with fake calls only.
    ///
    /// This command accepts no serial or device selector. It supervises two
    /// role-fixed children and validates their exact value-only attestations.
    CheckProfile2LoaderProcess {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict profile-2 manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Exercise one dedicated fresh-recovery subprocess with fake calls only.
    ///
    /// This command accepts no serial or device selector. Its role-fixed child
    /// independently stages stock and completes restore, verification and exit.
    CheckProfile2RecoveryProcess {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Exercise one complete session subprocess using model calls only.
    ///
    /// This command accepts no serial or device selector. It supervises one
    /// role-fixed child and independently verifies the raw model capture.
    CheckProfile2SessionProcess {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
        /// Device-free maximum: stream exactly 320,000,032 bytes through an
        /// anonymous file and forced reader pause under the finite deadline.
        #[arg(long)]
        maximum_pipe_stress: bool,
    },
    /// Hash-check and inspect one trusted libbladeRF without invoking a device API.
    CheckNativeLibrary {
        /// Exact libbladeRF shared-library file to inspect.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Require the patched bladeRF 2 configuration-neutral open contract.
        #[arg(long, conflicts_with = "require_bladerf2_loader_neutral_open")]
        require_bladerf2_config_neutral_open: bool,
        /// Require the patched bladeRF 2 loader-neutral open contract.
        #[arg(long, conflicts_with = "require_bladerf2_config_neutral_open")]
        require_bladerf2_loader_neutral_open: bool,
    },
    /// Stage exact custom and stock FPGA images without opening a device.
    CheckProfile2Images {
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Qualify the retained loader table and guarded worker with fake calls only.
    CheckProfile2LoaderWorker {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
    /// Qualify the complete receive-session worker with fake calls only.
    ///
    /// This retains the receive-only loader-neutral ABI but accepts no serial
    /// or device selector and invokes only deterministic model operations.
    CheckProfile2SessionWorker {
        /// Exact loader-neutral libbladeRF shared-library file to retain.
        #[arg(long)]
        library: PathBuf,
        /// Expected lowercase hexadecimal SHA-256 of the library file.
        #[arg(long)]
        sha256: String,
        /// Absolute path to the strictly verified profile-2 RBF.
        #[arg(long)]
        custom_image: PathBuf,
        /// Exact lowercase SHA-256 from the strict profile-2 manifest.
        #[arg(long)]
        custom_sha256: String,
        /// Exact custom-image byte count from the strict manifest.
        #[arg(long)]
        custom_bytes: u64,
        /// Absolute path to the frozen stock xA9 v0.16.0 RBF.
        #[arg(long)]
        stock_image: PathBuf,
    },
}

const fn session_process_qualification(maximum_pipe_stress: bool) -> SessionProcessQualification {
    if maximum_pipe_stress {
        SessionProcessQualification::MaximumPipeStress
    } else {
        SessionProcessQualification::Reference
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Fixture => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            let mut backend = FixtureBackend::new();
            server::serve(&mut input, &mut output, &mut backend)
        }
        Command::Native {
            library,
            sha256,
            serial,
        } => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            server::serve_with_factory(&mut input, &mut output, RegisterServerMode::Native, || {
                NativeBackend::open(&library, &sha256, &serial)
            })
        }
        Command::LoaderFixturePrimary {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            let config = PrimaryFixtureConfig::new(
                library,
                sha256,
                custom_image,
                custom_sha256,
                custom_bytes,
                stock_image,
            );
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            loader_server::serve_with_factory(
                &mut input,
                &mut output,
                LoaderServerMode::FixturePrimary,
                move || config.open(),
            )
        }
        Command::LoaderFixtureRecovery {
            library,
            sha256,
            stock_image,
        } => {
            let config = RecoveryFixtureConfig::new(library, sha256, stock_image);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            loader_server::serve_with_factory(
                &mut input,
                &mut output,
                LoaderServerMode::FixtureRecovery,
                move || config.open(),
            )
        }
        Command::SessionFixturePrimary {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            let config = SessionFixtureConfig::new(
                library,
                sha256,
                custom_image,
                custom_sha256,
                custom_bytes,
                stock_image,
            );
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            session_server::serve_with_factory(
                &mut input,
                &mut output,
                SessionServerMode::FixturePrimary,
                move || config.open(),
            )
        }
        Command::SessionNativePrimary {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
            serial,
        } => {
            let config = SessionNativeConfig::new(
                library,
                sha256,
                custom_image,
                custom_sha256,
                custom_bytes,
                stock_image,
                serial,
            );
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            session_server::serve_with_factory(
                &mut input,
                &mut output,
                SessionServerMode::NativePrimary,
                move || config.open(),
            )
        }
        Command::RecoveryFixture {
            library,
            sha256,
            stock_image,
        } => {
            let config = FreshRecoveryFixtureConfig::new(library, sha256, stock_image);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            bladerf_profile2_recovery_ipc::serve_with_factory(
                &mut input,
                &mut output,
                RecoveryServerMode::FixtureRecovery,
                move || config.open(),
            )?;
            Ok(())
        }
        Command::RecoveryNative {
            library,
            sha256,
            stock_image,
            serial,
        } => {
            let config = RecoveryNativeConfig::new(library, sha256, stock_image, serial);
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut input = stdin.lock();
            let mut output = stdout.lock();
            bladerf_profile2_recovery_ipc::serve_with_factory(
                &mut input,
                &mut output,
                RecoveryServerMode::NativeRecovery,
                move || config.open(),
            )?;
            Ok(())
        }
        Command::CheckProfile2LoaderProcess {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            let executable = std::env::current_exe()
                .context("resolve this adapter executable for loader fixture children")?;
            let report = check_profile2_loader_process(
                &executable,
                &library,
                &sha256,
                &custom_image,
                &custom_sha256,
                custom_bytes,
                &stock_image,
            )?;
            println!(
                "Profile-2 loader-process qualification passed: separate primary and recovery children attested libbladeRF {}/B2L1 from SHA-256 {}, custom {} bytes/SHA-256 {}, and frozen stock {} bytes/SHA-256 {}. Each child staged its own stock snapshot; the primary completed custom verification and same-handle stock restoration, and recovery independently restored stock. Shared-object initialization ran in each child. The adapter invoked only bladerf_version and the B2L1 marker from each retained table; all open/load/identity transitions were fake, with no device-facing libbladeRF, USB, FPGA-load, RF, capture, bias-tee, flash or TX operation.",
                bladerf_profile2_loader_ipc::REQUIRED_LIBRARY_VERSION,
                report.library_sha256(),
                report.custom_bytes(),
                report.custom_sha256(),
                bladerf_profile2_loader_ipc::REQUIRED_STOCK_BYTES,
                bladerf_profile2_loader_ipc::REQUIRED_STOCK_SHA256,
            );
            Ok(())
        }
        Command::CheckProfile2RecoveryProcess {
            library,
            sha256,
            stock_image,
        } => {
            let executable = std::env::current_exe()
                .context("resolve this adapter executable for fresh-recovery fixture child")?;
            let report =
                check_profile2_recovery_process(&executable, &library, &sha256, &stock_image)?;
            println!(
                "Profile-2 fresh-recovery process qualification passed: one role-fixed child independently staged frozen stock {} bytes/SHA-256 {}, retained libbladeRF {}/B2L1 from SHA-256 {}, and completed fake stock dispatch, configured/host-source/version 0.16.0 identity readback, orderly close and reaping. This existing identity contract is not cryptographic FPGA-configuration readback. Shared-object initialization ran, but only bladerf_version and the B2L1 marker were invoked from the retained table. No serial or device was selected; every open/load/identity transition was fake, with no device-facing libbladeRF, USB, physical FPGA load, RF, capture, bias-tee, flash or TX operation.",
                bladerf_profile2_recovery_ipc::REQUIRED_STOCK_BYTES,
                bladerf_profile2_recovery_ipc::REQUIRED_STOCK_SHA256,
                bladerf_profile2_recovery_ipc::REQUIRED_LIBRARY_VERSION,
                report.library_sha256(),
            );
            Ok(())
        }
        Command::CheckProfile2SessionProcess {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
            maximum_pipe_stress,
        } => {
            let executable = std::env::current_exe()
                .context("resolve this adapter executable for session fixture child")?;
            let report = check_profile2_session_process(SessionProcessCheck {
                program: &executable,
                library: &library,
                library_sha256: &sha256,
                custom_image: &custom_image,
                custom_sha256: &custom_sha256,
                custom_bytes,
                stock_image: &stock_image,
                qualification: session_process_qualification(maximum_pipe_stress),
            })?;
            let sink_receipt = report.reader_pause().map_or_else(
                || "private replayed file sink; no forced reader pause requested".to_owned(),
                |pause| {
                    format!(
                        "fixed-size count/hash working state plus one anonymous mode-0600 320,000,032-byte temporary file was flushed and synchronized; forced parent sink/reader pause entered and released after {:.3} s, exercising the finite-pipe path; the file was independently reread and is removed on close",
                        pause.as_secs_f64()
                    )
                },
            );
            println!(
                "Device-free Profile-2 session-process qualification passed: one role-fixed child retained libbladeRF {}/B2L1 from SHA-256 {}, staged custom {} bytes/SHA-256 {} and frozen stock {} bytes/SHA-256 {}, then kept one fake handle across custom verification, typed control, fixed RX2 configuration/readback, receive start, raw streaming, stop, same-handle stock restoration, verification and shutdown. The parent accepted {} DATA records and the plan-derived {}-sample synthetic model extent after independently counting and hashing {} bytes as SHA-256 {}; that parent digest agreed with the client count/hash and child completion, and no writable capture path crossed IPC. Capture completed in {:.3} s with {:.3} s of the 60-s transaction deadline remaining. Parent sink receipt: {}. The deterministic fixture stream contains one clean CONFIG with the model-applied epoch/phase words followed by continuous RX2 DATA records; it remains CPU-generated evidence, not a physical capture. Shared-object initialization ran, but the adapter invoked only bladerf_version and the B2L1 marker from the retained table. Every device-facing transition was a model call: no device or serial was selected, and no USB, physical FPGA load, RF configuration, physical capture, bias-tee, flash or TX operation occurred.",
                bladerf_profile2_session_ipc::REQUIRED_LIBRARY_VERSION,
                report.library_sha256(),
                report.custom_bytes(),
                report.custom_sha256(),
                bladerf_profile2_session_ipc::REQUIRED_STOCK_BYTES,
                bladerf_profile2_session_ipc::REQUIRED_STOCK_SHA256,
                report.data_records(),
                report.capture().transport_samples(),
                report.capture().raw_bytes(),
                report.capture().sha256(),
                report.capture_elapsed().as_secs_f64(),
                report.deadline_headroom().as_secs_f64(),
                sink_receipt,
            );
            Ok(())
        }
        Command::CheckNativeLibrary {
            library,
            sha256,
            require_bladerf2_config_neutral_open,
            require_bladerf2_loader_neutral_open,
        } => {
            let report = match (
                require_bladerf2_config_neutral_open,
                require_bladerf2_loader_neutral_open,
            ) {
                (true, false) => {
                    native_abi::check_bladerf2_config_neutral_library(&library, &sha256)?
                }
                (false, true) => {
                    native_abi::check_bladerf2_loader_neutral_library(&library, &sha256)?
                }
                (false, false) => native_abi::check_library(&library, &sha256)?,
                (true, true) => {
                    anyhow::bail!("specialized bladeRF 2 open contracts are mutually exclusive")
                }
            };
            if let Some(marker) = report.bladerf2_config_neutral_open_marker() {
                println!(
                    "libbladeRF native ABI preflight passed: path={}, SHA-256={}, version={}, bladeRF2-config-neutral-open-marker={marker:#010x}",
                    report.path().display(),
                    report.sha256(),
                    report.version()
                );
            } else if let Some(marker) = report.bladerf2_loader_neutral_open_marker() {
                println!(
                    "libbladeRF native ABI preflight passed: path={}, SHA-256={}, version={}, bladeRF2-loader-neutral-open-marker={marker:#010x}",
                    report.path().display(),
                    report.sha256(),
                    report.version()
                );
            } else {
                println!(
                    "libbladeRF native ABI preflight passed: path={}, SHA-256={}, version={}, specialized-bladeRF2-open=not-checked",
                    report.path().display(),
                    report.sha256(),
                    report.version()
                );
            }
            Ok(())
        }
        Command::CheckProfile2Images {
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            let report = sealed_image::check_profile2_images(
                &custom_image,
                &custom_sha256,
                custom_bytes,
                &stock_image,
            )?;
            println!(
                "Profile-2 image staging passed: custom={} bytes/SHA-256 {}, stock={} bytes/SHA-256 {}. Both snapshots used typed sealed descriptors. No libbladeRF library or device API was loaded or called, and no USB, FPGA-load, RF, capture, bias-tee, flash or TX operation occurred.",
                report.custom_bytes(),
                report.custom_sha256(),
                report.stock_bytes(),
                report.stock_sha256()
            );
            Ok(())
        }
        Command::CheckProfile2LoaderWorker {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            // Validate every image expectation before loading the shared object
            // or accessing any image source path.
            let custom =
                ExpectedImage::<CustomImage>::custom(&custom_image, &custom_sha256, custom_bytes)?;
            let stock = ExpectedImage::<StockImage>::stock(&stock_image)?;
            StagedImages::validate_roles(&custom, &stock)?;
            let (retained_library, library_report) =
                native_abi::prepare_bladerf2_loader_neutral_library(&library, &sha256)?;
            let image_report = loader_worker::verify_loader_worker(&custom, &stock)?;
            // Keep the exact sealed mapping and typed loader pointer alive for
            // the complete fake-worker qualification interval.
            drop(retained_library);
            println!(
                "Profile-2 loader-worker qualification passed: libbladeRF {}/B2L1 retained from SHA-256 {}; custom {} bytes/SHA-256 {}; stock {} bytes/SHA-256 {}. Same-handle restoration and ambiguous-load fresh-recovery fake traces passed. Shared-object initialization ran. The adapter invoked only bladerf_version and the B2L1 marker; it invoked no device-facing libbladeRF function or USB/FPGA/RF/capture/bias-tee/flash/TX operation.",
                library_report.version(),
                library_report.sha256(),
                image_report.custom_bytes(),
                image_report.custom_sha256(),
                image_report.stock_bytes(),
                image_report.stock_sha256()
            );
            Ok(())
        }
        Command::CheckProfile2SessionWorker {
            library,
            sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        } => {
            // Validate every image expectation before loading the shared object
            // or accessing any image source path.
            let custom =
                ExpectedImage::<CustomImage>::custom(&custom_image, &custom_sha256, custom_bytes)?;
            let stock = ExpectedImage::<StockImage>::stock(&stock_image)?;
            StagedImages::validate_roles(&custom, &stock)?;
            let (retained_library, library_report) =
                native_abi::prepare_bladerf2_loader_neutral_library(&library, &sha256)?;
            let (image_report, capture_report) =
                loader_worker::verify_session_worker(&custom, &stock)?;
            // Retain the exact sealed mapping and complete receive-only table
            // across the whole fake image/control/RX/restoration trace.
            drop(retained_library);
            println!(
                "Profile-2 session-worker qualification passed: libbladeRF {}/B2L1 retained from SHA-256 {}; custom {} bytes/SHA-256 {}; stock {} bytes/SHA-256 {}; model capture {} samples/{} bytes/SHA-256 {}. One fake handle completed typed control, fixed RX2 configuration/readback, receive start/capture/stop and same-handle stock restoration. Shared-object initialization ran. The adapter invoked only bladerf_version and the B2L1 marker; every device-facing open/load/register/RF/capture/stop transition was fake, with no USB, FPGA-load, RF, capture, bias-tee, flash or TX operation.",
                library_report.version(),
                library_report.sha256(),
                image_report.custom_bytes(),
                image_report.custom_sha256(),
                image_report.stock_bytes(),
                image_report.stock_sha256(),
                capture_report.samples(),
                capture_report.bytes(),
                capture_report.sha256(),
            );
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("profile-2 adapter command failed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parse_maximum_pipe_stress(extra: &[&str]) -> bool {
        let mut arguments = vec![
            "bladerf-profile2-adapter",
            "check-profile2-session-process",
            "--library",
            "/tmp/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--custom-image",
            "/tmp/custom.rbf",
            "--custom-sha256",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "--custom-bytes",
            "12858972",
            "--stock-image",
            "/tmp/stock.rbf",
        ];
        arguments.extend_from_slice(extra);
        let cli = Cli::try_parse_from(arguments).expect("qualification command should parse");
        let Command::CheckProfile2SessionProcess {
            maximum_pipe_stress,
            ..
        } = cli.command
        else {
            panic!("parsed the wrong command")
        };
        maximum_pipe_stress
    }

    #[test]
    fn maximum_pipe_stress_is_explicit_and_defaults_off() {
        let default = parse_maximum_pipe_stress(&[]);
        assert!(!default);
        assert_eq!(
            session_process_qualification(default),
            SessionProcessQualification::Reference
        );

        let requested = parse_maximum_pipe_stress(&["--maximum-pipe-stress"]);
        assert!(requested);
        assert_eq!(
            session_process_qualification(requested),
            SessionProcessQualification::MaximumPipeStress
        );
    }
}
