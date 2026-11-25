//! Lazy construction of the complete, device-free session fixture backend.
//!
//! Construction stages both exact image roles and retains the exact B2L1
//! library table, but the complete worker receives only [`ModelCalls`]. The
//! retained native table is never passed to the worker or used for a device
//! call.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bladerf_profile2_session_ipc::{BundleFacts, ImageFacts, LibraryFacts};

use crate::loader_fixture::validate_library_expectation;
use crate::loader_worker::{
    ActiveReceiveReadback, CaptureReport, ConfiguredReceiveReadback, MODEL_SERIAL, ModelCalls,
    PrimaryLoaderWorker, ReceivePlan,
};
use crate::native_abi::{LoaderDynamicCalls, prepare_bladerf2_loader_neutral_library};
use crate::sealed_image::{CustomImage, ExpectedImage, StagedImages, StockImage};
use crate::session_server::SessionBackend;

const SESSION_FIXTURE_HANDLE: u32 = 0x3001;

/// Files and exact expectations needed to open one fake complete session.
pub(crate) struct SessionFixtureConfig {
    library: PathBuf,
    library_sha256: String,
    custom_image: PathBuf,
    custom_sha256: String,
    custom_bytes: u64,
    stock_image: PathBuf,
}

impl SessionFixtureConfig {
    pub(crate) fn new(
        library: PathBuf,
        library_sha256: String,
        custom_image: PathBuf,
        custom_sha256: String,
        custom_bytes: u64,
        stock_image: PathBuf,
    ) -> Self {
        Self {
            library,
            library_sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        }
    }

    /// Qualify all files and create the fake session only after session hello.
    pub(crate) fn open(self) -> Result<SessionFixture> {
        validate_library_expectation(&self.library, &self.library_sha256)?;
        let custom = ExpectedImage::<CustomImage>::custom(
            &self.custom_image,
            &self.custom_sha256,
            self.custom_bytes,
        )?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;
        StagedImages::validate_roles(&custom, &stock)?;

        // Stage before loading a shared object so malformed or drifting image
        // inputs cannot run its initializer. All work remains inside the child
        // and therefore within the parent's bounded hello transaction.
        let images = StagedImages::stage(&custom, &stock)?;
        let image_report = images.report();
        let (retained_library, library_report) =
            prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
        let marker = library_report
            .bladerf2_loader_neutral_open_marker()
            .context("qualified session library lacks the required B2L1 marker")?;
        let bundle = BundleFacts::new(
            LibraryFacts::new(
                library_report.version().to_string(),
                library_report.sha256(),
                marker,
            ),
            ImageFacts::new(image_report.custom_sha256(), image_report.custom_bytes()),
            ImageFacts::new(image_report.stock_sha256(), image_report.stock_bytes()),
        )
        .context("construct exact qualified session bundle")?;
        let worker = PrimaryLoaderWorker::open(
            ModelCalls::fixture(SESSION_FIXTURE_HANDLE),
            images,
            MODEL_SERIAL,
        )?;
        Ok(SessionFixture {
            worker,
            _retained_library: retained_library,
            bundle,
        })
    }
}

/// One fake complete-session backend retaining exactly one guarded worker.
pub(crate) struct SessionFixture {
    // Drop the guarded fake worker before releasing the retained B2L1 table.
    worker: PrimaryLoaderWorker<ModelCalls>,
    _retained_library: LoaderDynamicCalls,
    bundle: BundleFacts,
}

impl SessionFixture {
    pub(crate) const fn bundle(&self) -> &BundleFacts {
        &self.bundle
    }
}

impl SessionBackend for SessionFixture {
    fn bundle(&self) -> &BundleFacts {
        self.bundle()
    }

    fn load_profile2(&mut self) -> Result<()> {
        self.worker.load_profile2()
    }

    fn verify_profile2(&mut self) -> Result<()> {
        self.worker.verify_profile2()
    }

    fn read_register(&mut self, register: bladerf_profile2_ipc::ReadRegister) -> Result<u32> {
        self.worker.read_register(register)
    }

    fn write_shadow(
        &mut self,
        channel: bladerf_profile2_ipc::ShadowChannel,
        value: u32,
    ) -> Result<()> {
        self.worker.write_shadow(channel, value)
    }

    fn commit(&mut self) -> Result<()> {
        self.worker.commit()
    }

    fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
        self.worker.configure_receive(plan)
    }

    fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
        self.worker.start_receive()
    }

    fn capture_receive(&mut self, output: &mut dyn Write) -> Result<CaptureReport> {
        self.worker.capture_receive(output)
    }

    fn stop_receive(&mut self) -> Result<()> {
        self.worker.stop_receive()
    }

    fn restore_stock(&mut self) -> Result<()> {
        self.worker.restore_stock()
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.worker.verify_stock()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.worker.shutdown()
    }
}
