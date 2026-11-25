use std::path::Path;

use anyhow::{Result, ensure};
use bladerf_profile2_ipc::{
    COMMIT_BYTE_OFFSET, COMMIT_MAGIC, PROFILE2_CAPABILITIES, PROFILE2_ID, ReadRegister,
    ShadowChannel,
};

use crate::native_abi::NativeDevice;

const REASON_NONE: u8 = 0;
const REASON_INCOMPLETE: u8 = 2;
const REASON_EPOCH_EXHAUSTED: u8 = 4;

pub(crate) trait Backend {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32>;
    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    // Server error cleanup can call this after an explicit shutdown response
    // fails to publish, so implementations must make repeated calls harmless.
    fn shutdown(&mut self) -> Result<()>;
}

trait RegisterDevice {
    fn read_register(&mut self, byte_offset: u32) -> Result<u32>;
    fn write_register(&mut self, byte_offset: u32, value: u32) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
}

impl RegisterDevice for NativeDevice {
    fn read_register(&mut self, byte_offset: u32) -> Result<u32> {
        NativeDevice::read_register(self, byte_offset)
    }

    fn write_register(&mut self, byte_offset: u32, value: u32) -> Result<()> {
        NativeDevice::write_register(self, byte_offset, value)
    }

    fn shutdown(&mut self) -> Result<()> {
        NativeDevice::shutdown(self);
        Ok(())
    }
}

pub(crate) struct NativeBackend<D = NativeDevice> {
    device: D,
}

impl NativeBackend {
    pub(crate) fn open(library: &Path, expected_sha256: &str, serial: &str) -> Result<Self> {
        Ok(Self {
            device: NativeDevice::open(library, expected_sha256, serial)?,
        })
    }
}

impl<D> NativeBackend<D> {
    #[cfg(test)]
    fn from_device(device: D) -> Self {
        Self { device }
    }
}

impl<D: RegisterDevice> Backend for NativeBackend<D> {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        self.device.read_register(register.byte_offset())
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        self.device.write_register(channel.byte_offset(), value)
    }

    fn commit(&mut self) -> Result<()> {
        self.device.write_register(COMMIT_BYTE_OFFSET, COMMIT_MAGIC)
    }

    fn shutdown(&mut self) -> Result<()> {
        self.device.shutdown()
    }
}

#[derive(Debug, Default)]
pub(crate) struct FixtureBackend {
    shadow_words: [u32; 4],
    written_mask: u8,
    applied_epoch: u32,
    applied_words: [u32; 4],
    pending_epoch: u32,
    pending_words: [u32; 4],
    busy_reads_remaining: u8,
    outcome_valid: bool,
    outcome_success: bool,
    outcome_reason: u8,
    outcome_epoch: u32,
    shutdown: bool,
}

impl FixtureBackend {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn ensure_open(&self) -> Result<()> {
        ensure!(!self.shutdown, "fixture backend is shut down");
        Ok(())
    }

    fn busy(&self) -> bool {
        self.pending_epoch != 0
    }

    fn status(&mut self) -> u32 {
        if self.busy_reads_remaining > 0 {
            self.busy_reads_remaining -= 1;
            return (1 << 31) | (u32::from(self.written_mask) << 20);
        }
        if self.pending_epoch != 0 {
            self.applied_epoch = self.pending_epoch;
            self.applied_words = self.pending_words;
            self.pending_epoch = 0;
            self.outcome_valid = true;
            self.outcome_success = true;
            self.outcome_reason = REASON_NONE;
        }
        (u32::from(self.outcome_valid) << 30)
            | (u32::from(self.outcome_success) << 29)
            | (u32::from(self.outcome_reason) << 24)
            | (u32::from(self.written_mask) << 20)
    }
}

impl Backend for FixtureBackend {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        self.ensure_open()?;
        match register {
            ReadRegister::Id => Ok(PROFILE2_ID),
            ReadRegister::Capabilities => Ok(PROFILE2_CAPABILITIES),
            ReadRegister::Status => Ok(self.status()),
            ReadRegister::AppliedEpoch => Ok(self.applied_epoch),
            ReadRegister::OutcomeEpoch => Ok(self.outcome_epoch),
            ReadRegister::Shadow0 => Ok(self.shadow_words[0]),
            ReadRegister::Shadow1 => Ok(self.shadow_words[1]),
            ReadRegister::Shadow2 => Ok(self.shadow_words[2]),
            ReadRegister::Shadow3 => Ok(self.shadow_words[3]),
            ReadRegister::Applied0 => Ok(self.applied_words[0]),
            ReadRegister::Applied1 => Ok(self.applied_words[1]),
            ReadRegister::Applied2 => Ok(self.applied_words[2]),
            ReadRegister::Applied3 => Ok(self.applied_words[3]),
        }
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        self.ensure_open()?;
        self.shadow_words[channel.index()] = value;
        self.written_mask |= 1 << channel.index();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.ensure_open()?;
        if self.busy() {
            return Ok(());
        }
        self.outcome_epoch = self.applied_epoch.saturating_add(1);
        if self.written_mask != 0x0f {
            self.outcome_valid = true;
            self.outcome_success = false;
            self.outcome_reason = REASON_INCOMPLETE;
            return Ok(());
        }
        if self.applied_epoch == u32::MAX {
            self.outcome_epoch = u32::MAX;
            self.outcome_valid = true;
            self.outcome_success = false;
            self.outcome_reason = REASON_EPOCH_EXHAUSTED;
            return Ok(());
        }
        self.pending_epoch = self.applied_epoch + 1;
        self.pending_words = self.shadow_words;
        self.written_mask = 0;
        self.busy_reads_remaining = 1;
        self.outcome_valid = false;
        self.outcome_success = false;
        self.outcome_reason = REASON_NONE;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.shutdown = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RegisterCall {
        Read(u32),
        Write(u32, u32),
        Shutdown,
    }

    #[derive(Debug, Default)]
    struct FakeRegisterDevice {
        calls: Vec<RegisterCall>,
        shutdown: bool,
    }

    impl RegisterDevice for FakeRegisterDevice {
        fn read_register(&mut self, byte_offset: u32) -> Result<u32> {
            ensure!(!self.shutdown, "fake register device is shut down");
            self.calls.push(RegisterCall::Read(byte_offset));
            Ok(byte_offset ^ 0xa5a5_5a5a)
        }

        fn write_register(&mut self, byte_offset: u32, value: u32) -> Result<()> {
            ensure!(!self.shutdown, "fake register device is shut down");
            self.calls.push(RegisterCall::Write(byte_offset, value));
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            if !self.shutdown {
                self.calls.push(RegisterCall::Shutdown);
                self.shutdown = true;
            }
            Ok(())
        }
    }

    #[test]
    fn native_backend_maps_every_typed_read_to_its_frozen_byte_offset() {
        let registers = [
            (ReadRegister::Id, 0x00),
            (ReadRegister::Capabilities, 0x04),
            (ReadRegister::Shadow0, 0x10),
            (ReadRegister::Shadow1, 0x14),
            (ReadRegister::Shadow2, 0x18),
            (ReadRegister::Shadow3, 0x1c),
            (ReadRegister::Status, 0x24),
            (ReadRegister::AppliedEpoch, 0x28),
            (ReadRegister::OutcomeEpoch, 0x2c),
            (ReadRegister::Applied0, 0x30),
            (ReadRegister::Applied1, 0x34),
            (ReadRegister::Applied2, 0x38),
            (ReadRegister::Applied3, 0x3c),
        ];
        let mut backend = NativeBackend::from_device(FakeRegisterDevice::default());

        for (register, byte_offset) in registers {
            assert_eq!(
                backend.read_register(register).unwrap(),
                byte_offset ^ 0xa5a5_5a5a
            );
        }

        assert_eq!(
            backend.device.calls,
            registers
                .map(|(_, byte_offset)| RegisterCall::Read(byte_offset))
                .to_vec()
        );
    }

    #[test]
    fn native_backend_maps_typed_writes_and_the_fixed_commit() {
        let channels = [
            (ShadowChannel::Channel0, 0x10, 0x0102_0304),
            (ShadowChannel::Channel1, 0x14, 0x1112_1314),
            (ShadowChannel::Channel2, 0x18, 0x2122_2324),
            (ShadowChannel::Channel3, 0x1c, 0x3132_3334),
        ];
        let mut backend = NativeBackend::from_device(FakeRegisterDevice::default());

        for (channel, _, value) in channels {
            backend.write_shadow(channel, value).unwrap();
        }
        backend.commit().unwrap();

        let mut expected = channels
            .map(|(_, byte_offset, value)| RegisterCall::Write(byte_offset, value))
            .to_vec();
        expected.push(RegisterCall::Write(0x20, 0x434d_4954));
        assert_eq!(backend.device.calls, expected);
    }

    #[test]
    fn native_backend_repeated_shutdown_is_harmless() {
        let mut backend = NativeBackend::from_device(FakeRegisterDevice::default());

        backend.shutdown().unwrap();
        backend.shutdown().unwrap();

        assert!(backend.device.shutdown);
        assert_eq!(backend.device.calls, [RegisterCall::Shutdown]);
        assert!(backend.read_register(ReadRegister::Id).is_err());
    }

    #[test]
    fn incomplete_and_exhausted_commits_match_register_status() {
        let mut fixture = FixtureBackend::new();
        fixture.write_shadow(ShadowChannel::Channel0, 1).unwrap();
        fixture.commit().unwrap();
        assert_eq!(
            fixture.read_register(ReadRegister::Status).unwrap(),
            0x4210_0000
        );
        assert_eq!(
            fixture.read_register(ReadRegister::OutcomeEpoch).unwrap(),
            1
        );

        let mut exhausted = FixtureBackend {
            applied_epoch: u32::MAX,
            ..FixtureBackend::new()
        };
        for channel in [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ] {
            exhausted
                .write_shadow(channel, channel.index() as u32)
                .unwrap();
        }
        exhausted.commit().unwrap();
        assert_eq!(
            exhausted.read_register(ReadRegister::Status).unwrap(),
            0x44f0_0000
        );
        assert_eq!(
            exhausted.read_register(ReadRegister::OutcomeEpoch).unwrap(),
            u32::MAX
        );
    }

    #[test]
    fn busy_writes_stage_the_next_bank_without_changing_the_frozen_apply() {
        let mut fixture = FixtureBackend::new();
        let first = [1, 2, 3, 4];
        for (channel, word) in [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ]
        .into_iter()
        .zip(first)
        {
            fixture.write_shadow(channel, word).unwrap();
        }
        fixture.commit().unwrap();

        let second = [0xfeed_beef, 6, 7, 8];
        for (channel, word) in [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ]
        .into_iter()
        .zip(second)
        {
            fixture.write_shadow(channel, word).unwrap();
        }
        assert_eq!(
            fixture.read_register(ReadRegister::Status).unwrap(),
            0x80f0_0000
        );
        fixture.commit().unwrap();
        assert_eq!(
            fixture.read_register(ReadRegister::Status).unwrap(),
            0x60f0_0000
        );
        assert_eq!(
            fixture.read_register(ReadRegister::Shadow0).unwrap(),
            0xfeed_beef
        );
        for (register, word) in [
            ReadRegister::Applied0,
            ReadRegister::Applied1,
            ReadRegister::Applied2,
            ReadRegister::Applied3,
        ]
        .into_iter()
        .zip(first)
        {
            assert_eq!(fixture.read_register(register).unwrap(), word);
        }
    }

    #[test]
    fn shutdown_is_idempotent_and_closes_register_access() {
        let mut fixture = FixtureBackend::new();
        fixture.shutdown().unwrap();
        fixture.shutdown().unwrap();
        assert!(fixture.read_register(ReadRegister::Id).is_err());
    }
}
