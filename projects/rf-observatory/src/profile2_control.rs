//! Device-independent host contract for profile-2 control and capture agreement.
//!
//! The eventual native adapter owns the device handle. This module owns the
//! serialized multi-call register protocol and accepts only an exclusive mutable
//! register interface. Individual native calls being thread-safe is not enough:
//! callers must also ensure that no other process or handle accesses these
//! registers during one `Controller::apply_stopped` transaction.

use std::fmt;

use anyhow::{Context, Result, bail, ensure};

use crate::transport_v2::{SOURCE_SAMPLE_RATE_HZ, ValidatedStream};

const PROFILE2_ID: u32 = 0x5246_0201;
const PROFILE2_CAPABILITIES: u32 = 0x0000_1f04;
const COMMIT_MAGIC: u32 = 0x434d_4954;
const MAX_STATUS_POLLS: usize = 4096;

pub(crate) fn validate_status_poll_bound(max_status_polls: usize) -> Result<()> {
    ensure!(
        (1..=MAX_STATUS_POLLS).contains(&max_status_polls),
        "profile-2 status poll bound must be 1 through {MAX_STATUS_POLLS}"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegisterAddress(u32);

impl RegisterAddress {
    pub(crate) const fn byte_offset(self) -> u32 {
        self.0
    }
}

const ID: RegisterAddress = RegisterAddress(0x00);
const CAPABILITIES: RegisterAddress = RegisterAddress(0x04);
const SHADOW: [RegisterAddress; 4] = [
    RegisterAddress(0x10),
    RegisterAddress(0x14),
    RegisterAddress(0x18),
    RegisterAddress(0x1c),
];
const COMMIT: RegisterAddress = RegisterAddress(0x20);
const STATUS: RegisterAddress = RegisterAddress(0x24);
const APPLIED_EPOCH: RegisterAddress = RegisterAddress(0x28);
const OUTCOME_EPOCH: RegisterAddress = RegisterAddress(0x2c);
const APPLIED: [RegisterAddress; 4] = [
    RegisterAddress(0x30),
    RegisterAddress(0x34),
    RegisterAddress(0x38),
    RegisterAddress(0x3c),
];

fn validate_register_abi() -> Result<()> {
    ensure!(PROFILE2_ID == 0x5246_0201, "profile-2 ID constant drifted");
    ensure!(
        PROFILE2_CAPABILITIES == 0x0000_1f04,
        "profile-2 capability constant drifted"
    );
    ensure!(
        COMMIT_MAGIC == 0x434d_4954,
        "profile-2 commit magic drifted"
    );
    ensure!(
        [
            ID.0,
            CAPABILITIES.0,
            COMMIT.0,
            STATUS.0,
            APPLIED_EPOCH.0,
            OUTCOME_EPOCH.0
        ] == [0x00, 0x04, 0x20, 0x24, 0x28, 0x2c]
            && SHADOW.map(|address| address.0) == [0x10, 0x14, 0x18, 0x1c]
            && APPLIED.map(|address| address.0) == [0x30, 0x34, 0x38, 0x3c],
        "profile-2 host byte-offset map drifted"
    );
    Ok(())
}

pub(crate) trait RegisterIo {
    fn read(&mut self, address: RegisterAddress) -> Result<u32>;
    fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Receiver {
    Rx1,
    Rx2,
}

impl Receiver {
    fn is_rx2(self) -> bool {
        self == Self::Rx2
    }
}

impl TryFrom<u8> for Receiver {
    type Error = anyhow::Error;

    fn try_from(number: u8) -> Result<Self, Self::Error> {
        match number {
            1 => Ok(Self::Rx1),
            2 => Ok(Self::Rx2),
            _ => bail!("profile-2 receiver must be RX1 or RX2"),
        }
    }
}

impl fmt::Display for Receiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Rx1 => "RX1",
            Self::Rx2 => "RX2",
        })
    }
}

/// The only variable profile-2 settings. All other transport settings are fixed:
/// one RX input, 20 MSps, 16-bit raw binary, baseband mux, and no metadata,
/// packet, highly-packed, trigger, bias-tee, or TX operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CaptureRequest {
    phase_words: [u32; 4],
    receiver: Receiver,
}

impl CaptureRequest {
    pub(crate) fn new(phase_words: [u32; 4], receiver: Receiver) -> Self {
        Self {
            phase_words,
            receiver,
        }
    }

    pub(crate) fn phase_words(self) -> [u32; 4] {
        self.phase_words
    }

    pub(crate) fn receiver(self) -> Receiver {
        self.receiver
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReceiveReadback {
    sample_rate_hz: u32,
    receiver: Receiver,
}

impl ReceiveReadback {
    pub(crate) fn new(sample_rate_hz: u32, receiver: Receiver) -> Self {
        Self {
            sample_rate_hz,
            receiver,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AppliedConfiguration {
    epoch: u32,
    phase_words: [u32; 4],
}

impl AppliedConfiguration {
    pub(crate) fn epoch(self) -> u32 {
        self.epoch
    }

    pub(crate) fn phase_words(self) -> [u32; 4] {
        self.phase_words
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reason {
    None,
    BadMagic,
    Incomplete,
    RxActive,
    EpochExhausted,
    ApplyRejected,
}

impl Reason {
    fn decode(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::BadMagic),
            2 => Ok(Self::Incomplete),
            3 => Ok(Self::RxActive),
            4 => Ok(Self::EpochExhausted),
            5 => Ok(Self::ApplyRejected),
            _ => bail!("profile-2 status contains unknown reason {value}"),
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::BadMagic => "bad commit magic",
            Self::Incomplete => "incomplete phase-word bank",
            Self::RxActive => "RX active",
            Self::EpochExhausted => "epoch exhausted",
            Self::ApplyRejected => "RX-domain apply rejected",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Status {
    busy: bool,
    outcome_valid: bool,
    outcome_success: bool,
    rx_active: bool,
    reason: Reason,
    written_mask: u8,
}

impl Status {
    fn decode(value: u32) -> Result<Self> {
        ensure!(
            value & 0x000f_ffff == 0,
            "profile-2 status reserved bits are nonzero"
        );
        let status = Self {
            busy: value & (1 << 31) != 0,
            outcome_valid: value & (1 << 30) != 0,
            outcome_success: value & (1 << 29) != 0,
            rx_active: value & (1 << 28) != 0,
            reason: Reason::decode(((value >> 24) & 0x0f) as u8)?,
            written_mask: ((value >> 20) & 0x0f) as u8,
        };
        ensure!(
            !(status.busy && status.outcome_valid),
            "profile-2 status is both busy and outcome-valid"
        );
        if status.outcome_success {
            ensure!(
                status.outcome_valid && status.reason == Reason::None,
                "profile-2 success status is internally inconsistent"
            );
        } else if status.outcome_valid {
            ensure!(
                status.reason != Reason::None,
                "profile-2 failed outcome lacks a reason"
            );
        } else {
            ensure!(
                status.reason == Reason::None,
                "profile-2 status has a reason without an outcome"
            );
        }
        Ok(status)
    }
}

pub(crate) struct Controller<'a, B> {
    io: &'a mut B,
    max_status_polls: usize,
}

impl<'a, B: RegisterIo> Controller<'a, B> {
    pub(crate) fn new(io: &'a mut B, max_status_polls: usize) -> Result<Self> {
        validate_status_poll_bound(max_status_polls)?;
        Ok(Self {
            io,
            max_status_polls,
        })
    }

    fn read(&mut self, address: RegisterAddress, label: &str) -> Result<u32> {
        self.io.read(address).with_context(|| {
            format!(
                "read profile-2 {label} at byte offset 0x{:02x}",
                address.byte_offset()
            )
        })
    }

    fn write(&mut self, address: RegisterAddress, value: u32, label: &str) -> Result<()> {
        self.io.write(address, value).with_context(|| {
            format!(
                "write profile-2 {label} at byte offset 0x{:02x}",
                address.byte_offset()
            )
        })
    }

    fn status(&mut self) -> Result<Status> {
        Status::decode(self.read(STATUS, "status")?)
    }

    /// Apply a complete bank while RX is stopped. The exclusive borrow covers
    /// ID/capability validation, four fresh writes, commit, bounded polling, and
    /// all applied-state readback checks.
    ///
    /// An error never authorizes capture. There is no rollback: an error after a
    /// shadow write can leave a partial fresh-write mask, and a timeout after
    /// commit leaves the apply outcome indeterminate. A caller must keep RX
    /// stopped and use an explicit reset or separately qualified reconciliation
    /// procedure before attempting another complete transaction.
    pub(crate) fn apply_stopped(
        &mut self,
        request: CaptureRequest,
    ) -> Result<AppliedConfiguration> {
        let requested_words = request.phase_words();
        ensure!(
            self.read(ID, "ID")? == PROFILE2_ID,
            "FPGA does not expose the profile-2 register layout"
        );
        ensure!(
            self.read(CAPABILITIES, "capabilities")? == PROFILE2_CAPABILITIES,
            "FPGA profile-2 capabilities differ from the host contract"
        );

        let initial = self.status()?;
        ensure!(!initial.busy, "profile-2 control is already busy");
        ensure!(!initial.rx_active, "profile-2 apply requires stopped RX");
        ensure!(
            initial.written_mask == 0,
            "profile-2 shadows were modified by another or incomplete transaction"
        );
        let previous_epoch = self.read(APPLIED_EPOCH, "previous applied epoch")?;
        let expected_epoch = previous_epoch
            .checked_add(1)
            .context("profile-2 epoch is exhausted")?;

        for (channel, (&address, &word)) in SHADOW.iter().zip(requested_words.iter()).enumerate() {
            self.write(address, word, &format!("shadow channel {channel}"))?;
            ensure!(
                self.read(address, &format!("shadow channel {channel}"))? == word,
                "profile-2 shadow channel {channel} readback differs from the request"
            );
        }
        let staged = self.status()?;
        ensure!(!staged.busy, "profile-2 control became busy before commit");
        ensure!(
            !staged.rx_active,
            "RX became active before profile-2 commit"
        );
        ensure!(
            staged.written_mask == 0x0f,
            "profile-2 fresh-write mask is incomplete before commit"
        );

        self.write(COMMIT, COMMIT_MAGIC, "commit")?;
        let mut outcome = None;
        for _ in 0..self.max_status_polls {
            let status = self.status()?;
            if !status.busy && status.outcome_valid {
                outcome = Some(status);
                break;
            }
        }
        let outcome = outcome.context("profile-2 apply did not complete within the poll bound")?;
        ensure!(
            self.read(OUTCOME_EPOCH, "outcome epoch")? == expected_epoch,
            "profile-2 outcome epoch differs from the requested transaction"
        );
        ensure!(
            outcome.outcome_success,
            "profile-2 apply failed: {}",
            outcome.reason
        );
        ensure!(
            !outcome.rx_active,
            "RX became active during the profile-2 apply transaction"
        );
        ensure!(
            outcome.written_mask == 0,
            "profile-2 accepted commit did not consume the fresh-write mask"
        );
        ensure!(
            outcome.reason == Reason::None,
            "profile-2 successful apply has a rejection reason"
        );

        ensure!(
            self.read(APPLIED_EPOCH, "applied epoch")? == expected_epoch,
            "profile-2 applied epoch differs from the successful outcome"
        );
        let mut applied_words = [0u32; 4];
        for (channel, &address) in APPLIED.iter().enumerate() {
            applied_words[channel] = self.read(address, &format!("applied channel {channel}"))?;
        }
        ensure!(
            applied_words == requested_words,
            "profile-2 applied phase words differ from the request"
        );
        Ok(AppliedConfiguration {
            epoch: expected_epoch,
            phase_words: applied_words,
        })
    }
}

/// Reconcile independently observed RFIC settings and strict CONFIG/DATA bytes
/// with the requested and applied register state. A CONFIG-only diagnostic has
/// already failed `ValidatedStream` construction and cannot reach this function.
pub(crate) fn reconcile_capture(
    request: CaptureRequest,
    applied: AppliedConfiguration,
    readback: ReceiveReadback,
    stream: &ValidatedStream,
) -> Result<()> {
    ensure!(
        applied.phase_words() == request.phase_words(),
        "profile-2 requested and applied phase words disagree"
    );
    ensure!(
        readback.sample_rate_hz == SOURCE_SAMPLE_RATE_HZ,
        "RFIC sample-rate readback differs from the profile-2 contract"
    );
    ensure!(
        readback.receiver == request.receiver(),
        "RFIC receive connector differs from the profile-2 request"
    );
    ensure!(
        stream.config.epoch == applied.epoch(),
        "profile-2 CONFIG epoch differs from the applied epoch"
    );
    ensure!(
        stream.config.phase_words == applied.phase_words(),
        "profile-2 CONFIG phase words differ from applied readback"
    );
    ensure!(
        stream.config.required_source_sample_rate_hz == readback.sample_rate_hz,
        "profile-2 CONFIG source rate differs from RFIC readback"
    );
    ensure!(
        stream.config.status.rx2 == request.receiver().is_rx2(),
        "profile-2 CONFIG receive connector differs from the request"
    );
    Ok(())
}

/// CPU-only reference exercise used by the maintained verifier. It proves the
/// host transaction and wire-reconciliation code against a bounded register
/// model; it is not a native USB, generated-Qsys, or hardware test.
pub(crate) fn exercise_reference_contract(
    request: CaptureRequest,
    previous_epoch: u32,
) -> Result<AppliedConfiguration> {
    validate_register_abi()?;
    let mut model = ReferenceRegisters::new(previous_epoch);
    let applied = Controller::new(&mut model, 4)?.apply_stopped(request)?;
    ensure!(
        model.operations == 22,
        "profile-2 reference transaction performed an unexpected operation count"
    );
    Ok(applied)
}

struct ReferenceRegisters {
    operations: usize,
    shadow: [u32; 4],
    written_mask: u8,
    applied_epoch: u32,
    applied_words: [u32; 4],
    pending_epoch: u32,
    pending_words: [u32; 4],
    busy: bool,
    busy_reads_remaining: u8,
    outcome_valid: bool,
    outcome_success: bool,
}

impl ReferenceRegisters {
    fn new(applied_epoch: u32) -> Self {
        Self {
            operations: 0,
            shadow: [0; 4],
            written_mask: 0,
            applied_epoch,
            applied_words: [0; 4],
            pending_epoch: 0,
            pending_words: [0; 4],
            busy: false,
            busy_reads_remaining: 0,
            outcome_valid: false,
            outcome_success: false,
        }
    }

    fn status(&mut self) -> u32 {
        if self.busy {
            if self.busy_reads_remaining > 0 {
                self.busy_reads_remaining -= 1;
            } else {
                self.busy = false;
                self.outcome_valid = true;
                self.outcome_success = true;
                self.applied_epoch = self.pending_epoch;
                self.applied_words = self.pending_words;
            }
        }
        (u32::from(self.busy) << 31)
            | (u32::from(self.outcome_valid) << 30)
            | (u32::from(self.outcome_success) << 29)
            | (u32::from(self.written_mask) << 20)
    }
}

impl RegisterIo for ReferenceRegisters {
    fn read(&mut self, address: RegisterAddress) -> Result<u32> {
        self.operations += 1;
        match address {
            ID => Ok(PROFILE2_ID),
            CAPABILITIES => Ok(PROFILE2_CAPABILITIES),
            STATUS => Ok(self.status()),
            APPLIED_EPOCH => Ok(self.applied_epoch),
            OUTCOME_EPOCH => Ok(if self.outcome_valid {
                self.pending_epoch
            } else {
                0
            }),
            _ => {
                if let Some(channel) = SHADOW.iter().position(|candidate| *candidate == address) {
                    return Ok(self.shadow[channel]);
                }
                if let Some(channel) = APPLIED.iter().position(|candidate| *candidate == address) {
                    return Ok(self.applied_words[channel]);
                }
                bail!("reference model rejected read address 0x{:02x}", address.0)
            }
        }
    }

    fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()> {
        self.operations += 1;
        if let Some(channel) = SHADOW.iter().position(|candidate| *candidate == address) {
            ensure!(!self.busy, "reference model shadow write while busy");
            self.shadow[channel] = value;
            self.written_mask |= 1 << channel;
            return Ok(());
        }
        ensure!(address == COMMIT, "reference model rejected write address");
        ensure!(
            value == COMMIT_MAGIC,
            "reference model rejected commit magic"
        );
        ensure!(
            !self.busy,
            "reference model received a second commit while busy"
        );
        ensure!(
            self.written_mask == 0x0f,
            "reference model received an incomplete commit"
        );
        self.pending_epoch = self
            .applied_epoch
            .checked_add(1)
            .context("reference model epoch exhausted")?;
        self.pending_words = self.shadow;
        self.written_mask = 0;
        self.busy = true;
        self.busy_reads_remaining = 1;
        self.outcome_valid = false;
        self.outcome_success = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::transport_v2;

    #[derive(Debug)]
    enum Step {
        Read(RegisterAddress, u32),
        Write(RegisterAddress, u32),
    }

    struct ScriptIo {
        steps: VecDeque<Step>,
    }

    impl ScriptIo {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
            }
        }

        fn finish(self) {
            assert!(self.steps.is_empty(), "unconsumed scripted register steps");
        }
    }

    impl RegisterIo for ScriptIo {
        fn read(&mut self, address: RegisterAddress) -> Result<u32> {
            match self.steps.pop_front().context("unexpected register read")? {
                Step::Read(expected, value) => {
                    ensure!(address == expected, "unexpected register read address");
                    Ok(value)
                }
                Step::Write(_, _) => bail!("register read occurred where write was expected"),
            }
        }

        fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()> {
            match self
                .steps
                .pop_front()
                .context("unexpected register write")?
            {
                Step::Write(expected_address, expected_value) => {
                    ensure!(
                        address == expected_address,
                        "unexpected register write address"
                    );
                    ensure!(value == expected_value, "unexpected register write value");
                    Ok(())
                }
                Step::Read(_, _) => bail!("register write occurred where read was expected"),
            }
        }
    }

    const WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];

    fn request() -> CaptureRequest {
        CaptureRequest::new(WORDS, Receiver::Rx2)
    }

    fn prefix(previous_epoch: u32) -> Vec<Step> {
        let mut steps = vec![
            Step::Read(ID, PROFILE2_ID),
            Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
            Step::Read(STATUS, 0),
            Step::Read(APPLIED_EPOCH, previous_epoch),
        ];
        for (address, word) in SHADOW.into_iter().zip(WORDS) {
            steps.push(Step::Write(address, word));
            steps.push(Step::Read(address, word));
        }
        steps.push(Step::Read(STATUS, 0x00f0_0000));
        steps.push(Step::Write(COMMIT, COMMIT_MAGIC));
        steps
    }

    fn success(previous_epoch: u32) -> Vec<Step> {
        let epoch = previous_epoch + 1;
        let mut steps = prefix(previous_epoch);
        steps.push(Step::Read(STATUS, 0x8000_0000));
        steps.push(Step::Read(STATUS, 0x6000_0000));
        steps.push(Step::Read(OUTCOME_EPOCH, epoch));
        steps.push(Step::Read(APPLIED_EPOCH, epoch));
        for (address, word) in APPLIED.into_iter().zip(WORDS) {
            steps.push(Step::Read(address, word));
        }
        steps
    }

    fn apply(steps: Vec<Step>, polls: usize) -> (Result<AppliedConfiguration>, ScriptIo) {
        let mut io = ScriptIo::new(steps);
        let result = Controller::new(&mut io, polls)
            .and_then(|mut controller| controller.apply_stopped(request()));
        (result, io)
    }

    #[test]
    fn exact_successful_transaction_uses_byte_offsets_and_verified_readback() {
        validate_register_abi().unwrap();
        assert_eq!(PROFILE2_ID, 0x5246_0201);
        assert_eq!(PROFILE2_CAPABILITIES, 0x0000_1f04);
        assert_eq!(COMMIT_MAGIC, 0x434d_4954);
        assert_eq!(ID.byte_offset(), 0x00);
        assert_eq!(CAPABILITIES.byte_offset(), 0x04);
        assert_eq!(
            SHADOW.map(RegisterAddress::byte_offset),
            [0x10, 0x14, 0x18, 0x1c]
        );
        assert_eq!(COMMIT.byte_offset(), 0x20);
        assert_eq!(STATUS.byte_offset(), 0x24);
        assert_eq!(APPLIED_EPOCH.byte_offset(), 0x28);
        assert_eq!(OUTCOME_EPOCH.byte_offset(), 0x2c);
        assert_eq!(
            APPLIED.map(RegisterAddress::byte_offset),
            [0x30, 0x34, 0x38, 0x3c]
        );

        let (result, io) = apply(success(7), 4);
        assert_eq!(
            result.unwrap(),
            AppliedConfiguration {
                epoch: 8,
                phase_words: WORDS
            }
        );
        io.finish();
        assert_eq!(
            exercise_reference_contract(request(), 7).unwrap().epoch(),
            8
        );
    }

    #[test]
    fn identity_capability_and_preflight_fail_before_shadow_writes() {
        for steps in [
            vec![Step::Read(ID, 0)],
            vec![Step::Read(ID, PROFILE2_ID), Step::Read(CAPABILITIES, 0)],
            vec![
                Step::Read(ID, PROFILE2_ID),
                Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
                Step::Read(STATUS, 0x8000_0000),
            ],
            vec![
                Step::Read(ID, PROFILE2_ID),
                Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
                Step::Read(STATUS, 0x1000_0000),
            ],
            vec![
                Step::Read(ID, PROFILE2_ID),
                Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
                Step::Read(STATUS, 0x0010_0000),
            ],
        ] {
            let (result, io) = apply(steps, 4);
            assert!(result.is_err());
            io.finish();
        }
    }

    #[test]
    fn rejects_shadow_mismatch_timeout_and_failed_outcomes() {
        let mut mismatch = vec![
            Step::Read(ID, PROFILE2_ID),
            Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
            Step::Read(STATUS, 0),
            Step::Read(APPLIED_EPOCH, 0),
            Step::Write(SHADOW[0], WORDS[0]),
            Step::Read(SHADOW[0], !WORDS[0]),
        ];
        let (result, io) = apply(std::mem::take(&mut mismatch), 4);
        assert!(result.is_err());
        io.finish();

        let mut timeout = prefix(0);
        timeout.extend([
            Step::Read(STATUS, 0x8000_0000),
            Step::Read(STATUS, 0x8000_0000),
        ]);
        let (result, io) = apply(timeout, 2);
        assert!(result.is_err());
        io.finish();

        for reason in 1u32..=5 {
            let mut rejected = prefix(0);
            rejected.push(Step::Read(STATUS, 0x4000_0000 | (reason << 24)));
            rejected.push(Step::Read(OUTCOME_EPOCH, 1));
            let (result, io) = apply(rejected, 1);
            let error = result.unwrap_err().to_string();
            assert!(error.contains("profile-2 apply failed"), "{error}");
            io.finish();
        }

        let exhausted = vec![
            Step::Read(ID, PROFILE2_ID),
            Step::Read(CAPABILITIES, PROFILE2_CAPABILITIES),
            Step::Read(STATUS, 0),
            Step::Read(APPLIED_EPOCH, u32::MAX),
        ];
        let (result, io) = apply(exhausted, 1);
        assert!(result.is_err());
        io.finish();
    }

    #[test]
    fn rejects_unknown_status_and_epoch_or_applied_word_disagreement() {
        for status in [
            0x0000_0001,
            0x4000_0000,
            0x4600_0000,
            0x2000_0000,
            0xc000_0000,
        ] {
            assert!(Status::decode(status).is_err(), "status {status:#010x}");
        }

        let mut bad_outcome_epoch = success(0);
        let outcome = bad_outcome_epoch
            .iter_mut()
            .find(|step| matches!(step, Step::Read(address, _) if *address == OUTCOME_EPOCH))
            .unwrap();
        *outcome = Step::Read(OUTCOME_EPOCH, 2);
        let (result, io) = apply(bad_outcome_epoch, 4);
        assert!(result.is_err());
        assert_eq!(io.steps.len(), 5);

        let mut bad_applied = success(0);
        let last = bad_applied.last_mut().unwrap();
        *last = Step::Read(APPLIED[3], !WORDS[3]);
        let (result, io) = apply(bad_applied, 4);
        assert!(result.is_err());
        io.finish();
    }

    #[test]
    fn successful_outcome_rejects_receiver_or_fresh_mask_races() {
        let mut steps = prefix(0);
        steps.push(Step::Read(STATUS, 0x7000_0000));
        steps.push(Step::Read(OUTCOME_EPOCH, 1));
        let (result, io) = apply(steps, 1);
        assert!(result.is_err());
        io.finish();

        let mut steps = prefix(0);
        steps.push(Step::Read(STATUS, 0x6010_0000));
        steps.push(Step::Read(OUTCOME_EPOCH, 1));
        let (result, io) = apply(steps, 1);
        assert!(result.is_err());
        io.finish();
    }

    fn valid_stream() -> ValidatedStream {
        let config = [
            0x46, 0x52, 0x42, 0x4f, 0x31, 0x20, 0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2d,
            0x31, 0x01, 0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab, 0x89, 0x01, 0x00, 0x00, 0x80,
            0xef, 0xbe, 0xad, 0xde,
        ];
        let data = [
            0x46, 0x52, 0x42, 0x4f, 0x21, 0x20, 0x02, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        transport_v2::validate_stream(&[config, data].concat()).unwrap()
    }

    #[test]
    fn requested_applied_rfic_and_wire_state_must_all_agree() {
        let request = request();
        let applied = AppliedConfiguration {
            epoch: 1,
            phase_words: WORDS,
        };
        let readback = ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx2);
        let stream = valid_stream();
        reconcile_capture(request, applied, readback, &stream).unwrap();

        let wrong_words = AppliedConfiguration {
            phase_words: [0; 4],
            ..applied
        };
        assert!(reconcile_capture(request, wrong_words, readback, &stream).is_err());
        let wrong_epoch = AppliedConfiguration {
            epoch: 2,
            ..applied
        };
        assert!(reconcile_capture(request, wrong_epoch, readback, &stream).is_err());
        assert!(
            reconcile_capture(
                request,
                applied,
                ReceiveReadback::new(19_999_999, Receiver::Rx2),
                &stream
            )
            .is_err()
        );
        assert!(
            reconcile_capture(
                request,
                applied,
                ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx1),
                &stream
            )
            .is_err()
        );

        let different_words = [1, 2, 3, 4];
        assert!(
            reconcile_capture(
                CaptureRequest::new(different_words, Receiver::Rx2),
                AppliedConfiguration {
                    epoch: 1,
                    phase_words: different_words,
                },
                readback,
                &stream,
            )
            .is_err()
        );
        assert!(
            reconcile_capture(
                CaptureRequest::new(WORDS, Receiver::Rx1),
                applied,
                ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx1),
                &stream,
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_receiver_and_poll_bounds_are_rejected() {
        assert!(Receiver::try_from(0).is_err());
        assert!(Receiver::try_from(3).is_err());
        assert_eq!(Receiver::try_from(1).unwrap(), Receiver::Rx1);
        assert_eq!(Receiver::try_from(2).unwrap(), Receiver::Rx2);
        let mut io = ScriptIo::new(vec![]);
        assert!(Controller::new(&mut io, 0).is_err());
        assert!(Controller::new(&mut io, MAX_STATUS_POLLS + 1).is_err());
    }
}
