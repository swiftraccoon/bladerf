# LoRa file replay

The worker uses the GPL-3.0-only LoRa receiver in FutureSDR. It is an
independent Cargo workspace and exchanges files with the Observatory.
Its only subcommand, `decode`, reads saved complex samples.

## Build

Initialize the [pinned FutureSDR checkout](../../../repos/README.md) at
[42832f5473ec](https://github.com/FutureSDR/FutureSDR/tree/42832f5473ec19ce641c58a4792befdc57945ff5).
From the repository root, check and apply
[the file replay patch](../../../patches/futuresdr-lora-file-replay.patch).

```sh
git -C repos/FutureSDR apply --check ../../patches/futuresdr-lora-file-replay.patch
git -C repos/FutureSDR apply ../../patches/futuresdr-lora-file-replay.patch
```

The patch makes the radio dependencies optional and adds observed sync
symbols to the receiver's annotated output. Check an already patched
checkout with `git -C repos/FutureSDR apply --reverse --check ../../patches/futuresdr-lora-file-replay.patch`.

Build from `tools/bladerf-lora-worker/` to use the
[pinned nightly](../rust-toolchain.toml) and [lockfile](../Cargo.lock).
Fetch dependency sources with `cargo fetch --locked` when populating a
fresh Cargo cache.

```sh
cargo build --locked --offline --release
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
```

The build disables FutureSDR's radio and control-port features. Restore the
pinned source after building with
`git -C repos/FutureSDR apply --reverse ../../patches/futuresdr-lora-file-replay.patch`
from the repository root.

## Decode

Provide a direct regular, nonempty file of little-endian `f32` samples
interleaved in I/Q order. Every component must be finite. The file must
contain up to 200,000,000 complete complex samples, at exactly four times the
selected bandwidth. The worker supports 62.5, 125, 250, and 500 kHz bandwidths
and spreading factors 7 through 12. The channel center is a positive integer
in hertz that fits in an unsigned 32-bit integer. The compact sync word is a
decimal byte.

Shift the desired channel to zero frequency and resample it before replay.
The receiver checks explicit headers and independently recomputes the payload
CRC. It also checks the observed sync symbol pair against the requested
compact sync word. The first symbol is eight times the high nibble. The
second is eight times the low nibble.

```sh
target/release/bladerf-lora-worker decode INPUT.cf32 \
  --channel-hz 906875000 --bandwidth-hz 250000 --spreading-factor 11 \
  --sync-word 43 --max-frames 64 --output OUTPUT.json
```

This example requires 1 MSps of channel-centered IQ. Use a new output file
in an existing directory. The worker streams through an 8,192-sample read
buffer and verifies the complete admitted extent and EOF. It writes a
private temporary receipt, syncs it, rechecks the original descriptor and
path, and publishes without replacing an existing file. A directory sync
error after publication leaves the completed receipt in place. Require a
successful worker exit.

## Receipt

Schema 1 uses operation `decode_lora_cf32` and source format `CF32_LE`.
The receipt contains the original sample count, the SHA-256 of the complete
input file, the selected receive settings and the expected sync pair.

`validated_frames` includes all accepted frames, including any above the
retention limit. `--max-frames` accepts 1 through 4096. The receipt counts
the valid frames omitted after the cap. It separately counts annotated
frames lacking a CRC, failing the independent CRC check or carrying a
mismatched sync pair. The receiver can discard other candidates before
producing annotated output.

Each retained record contains the observed sync pair, coding-rate index
(1 for 4/5 through 4 for 4/8), payload bytes in lowercase hexadecimal, and
numeric payload CRC. `synchronization_sample` is a file-relative sample
index near the header boundary supplied by FutureSDR. Zero validated frames
means the worker found no matching CRC-valid packet in the supplied samples.
Store receipts privately since the payload may contain radio traffic.
