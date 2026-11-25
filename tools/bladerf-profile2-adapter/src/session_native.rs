//! Lazy construction of one serial-selected native complete-session backend.
//!
//! The exact custom and stock image roles are staged before the B2L1 shared
//! object is loaded. The resulting native table then moves into one guarded
//! worker, which owns the sole device handle for the complete session.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use anyhow::bail;
use bladerf_profile2_session_ipc::BundleFacts;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bladerf_profile2_session_ipc::{ImageFacts, LibraryFacts};

use crate::loader_fixture::validate_library_expectation;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::loader_worker::PrimaryLoaderWorker;
use crate::loader_worker::{
    ActiveReceiveReadback, CaptureReport, ConfiguredReceiveReadback, ReceivePlan,
    validate_requested_serial,
};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::native_abi::{LoaderDynamicCalls, prepare_bladerf2_loader_neutral_library};
use crate::sealed_image::{CustomImage, ExpectedImage, StagedImages, StockImage};
use crate::session_server::SessionBackend;

/// Direct arguments needed to open one native complete session after hello.
pub(crate) struct SessionNativeConfig {
    library: PathBuf,
    library_sha256: String,
    custom_image: PathBuf,
    custom_sha256: String,
    custom_bytes: u64,
    stock_image: PathBuf,
    serial: String,
}

impl SessionNativeConfig {
    pub(crate) fn new(
        library: PathBuf,
        library_sha256: String,
        custom_image: PathBuf,
        custom_sha256: String,
        custom_bytes: u64,
        stock_image: PathBuf,
        serial: String,
    ) -> Self {
        Self {
            library,
            library_sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
            serial,
        }
    }

    /// Stage and qualify the exact bundle, then open the selected device.
    ///
    /// The session server calls this only after decoding an exact native hello.
    pub(crate) fn open(self) -> Result<SessionNative> {
        // The selector must fail before any supplied path can be accessed.
        validate_requested_serial(&self.serial)?;
        validate_library_expectation(&self.library, &self.library_sha256)?;
        let custom = ExpectedImage::<CustomImage>::custom(
            &self.custom_image,
            &self.custom_sha256,
            self.custom_bytes,
        )?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;
        StagedImages::validate_roles(&custom, &stock)?;

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // Stage both roles before mapping the shared object. A malformed,
            // drifting, or missing image therefore cannot run its initializer.
            let images = StagedImages::stage(&custom, &stock)?;
            let image_report = images.report();
            let (calls, library_report) =
                prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
            let marker = library_report
                .bladerf2_loader_neutral_open_marker()
                .context("qualified native session library lacks the required B2L1 marker")?;
            let bundle = BundleFacts::new(
                LibraryFacts::new(
                    library_report.version().to_string(),
                    library_report.sha256(),
                    marker,
                ),
                ImageFacts::new(image_report.custom_sha256(), image_report.custom_bytes()),
                ImageFacts::new(image_report.stock_sha256(), image_report.stock_bytes()),
            )
            .context("construct exact qualified native session bundle")?;

            // Moving the B2L1 table into the worker retains the exact mapping
            // across open, both image transitions, RX, and orderly close.
            let worker = PrimaryLoaderWorker::open(calls, images, &self.serial)?;
            Ok(SessionNative { worker, bundle })
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (custom, stock);
            bail!("native bladeRF sessions are supported only on x86-64 Linux")
        }
    }
}

/// One native complete-session backend with one guarded worker and handle.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) struct SessionNative {
    worker: PrimaryLoaderWorker<LoaderDynamicCalls>,
    bundle: BundleFacts,
}

/// Native backend placeholder for unsupported build targets.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) struct SessionNative;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl SessionBackend for SessionNative {
    fn bundle(&self) -> &BundleFacts {
        &self.bundle
    }

    fn load_profile2(&mut self) -> Result<()> {
        self.with_worker(|worker| worker.load_profile2())
    }

    fn verify_profile2(&mut self) -> Result<()> {
        self.with_worker(|worker| worker.verify_profile2())
    }

    fn read_register(&mut self, register: bladerf_profile2_ipc::ReadRegister) -> Result<u32> {
        self.with_worker(|worker| worker.read_register(register))
    }

    fn write_shadow(
        &mut self,
        channel: bladerf_profile2_ipc::ShadowChannel,
        value: u32,
    ) -> Result<()> {
        self.with_worker(|worker| worker.write_shadow(channel, value))
    }

    fn commit(&mut self) -> Result<()> {
        self.with_worker(|worker| worker.commit())
    }

    fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
        self.with_worker(|worker| worker.configure_receive(plan))
    }

    fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
        self.with_worker(PrimaryLoaderWorker::start_receive)
    }

    fn capture_receive(&mut self, output: &mut dyn Write) -> Result<CaptureReport> {
        self.with_worker(|worker| worker.capture_receive(output))
    }

    fn stop_receive(&mut self) -> Result<()> {
        self.with_worker(PrimaryLoaderWorker::stop_receive)
    }

    fn restore_stock(&mut self) -> Result<()> {
        self.with_worker(PrimaryLoaderWorker::restore_stock)
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.with_worker(PrimaryLoaderWorker::verify_stock)
    }

    fn shutdown(&mut self) -> Result<()> {
        self.with_worker(PrimaryLoaderWorker::shutdown)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl SessionNative {
    fn with_worker<T>(
        &mut self,
        operation: impl FnOnce(&mut PrimaryLoaderWorker<LoaderDynamicCalls>) -> Result<T>,
    ) -> Result<T> {
        operation(&mut self.worker)
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn unsupported<T>() -> Result<T> {
    bail!("native bladeRF sessions are supported only on x86-64 Linux")
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
impl SessionBackend for SessionNative {
    fn bundle(&self) -> &BundleFacts {
        unreachable!("unsupported targets cannot construct a native session")
    }

    fn load_profile2(&mut self) -> Result<()> {
        unsupported()
    }

    fn verify_profile2(&mut self) -> Result<()> {
        unsupported()
    }

    fn read_register(&mut self, _register: bladerf_profile2_ipc::ReadRegister) -> Result<u32> {
        unsupported()
    }

    fn write_shadow(
        &mut self,
        _channel: bladerf_profile2_ipc::ShadowChannel,
        _value: u32,
    ) -> Result<()> {
        unsupported()
    }

    fn commit(&mut self) -> Result<()> {
        unsupported()
    }

    fn configure_receive(&mut self, _plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
        unsupported()
    }

    fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
        unsupported()
    }

    fn capture_receive(&mut self, _output: &mut dyn Write) -> Result<CaptureReport> {
        unsupported()
    }

    fn stop_receive(&mut self) -> Result<()> {
        unsupported()
    }

    fn restore_stock(&mut self) -> Result<()> {
        unsupported()
    }

    fn verify_stock(&mut self) -> Result<()> {
        unsupported()
    }

    fn shutdown(&mut self) -> Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_serial_precedes_every_path_access() {
        let error = SessionNativeConfig::new(
            PathBuf::from("/this/path/must/not/be-opened/libbladeRF.so.2"),
            "not-a-library-digest".to_owned(),
            PathBuf::from("/this/path/must/not/be-opened/custom.rbf"),
            "not-a-custom-digest".to_owned(),
            0,
            PathBuf::from("/this/path/must/not/be-opened/stock.rbf"),
            "BAD-SERIAL".to_owned(),
        )
        .open()
        .err()
        .expect("malformed serial must fail");
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("serial"), "{diagnostic}");
        assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    }
}
