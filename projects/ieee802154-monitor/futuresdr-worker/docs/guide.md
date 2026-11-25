# FutureSDR IEEE 802.15.4 file worker

The worker demodulates a complete channel-centered, little-endian CF32 file
at 4 MSps on a little-endian host. It runs entirely on files and writes raw
PSDU candidates. Use the [independent inspector](../../docs/guide.md#inspect-demodulator-candidates)
to check each candidate's FCS and publish validated packets.

## Build and run

[Initialize the workspace submodules](../../../../repos/README.md), then apply
both patches to the pinned FutureSDR checkout in order from the repository root.

```console
git -C repos/FutureSDR apply ../../patches/futuresdr-zigbee-file-worker.patch
git -C repos/FutureSDR apply ../../patches/futuresdr-zigbee-source-index.patch
```

From the repository root, check an already patched checkout with
`git -C repos/FutureSDR apply --reverse --check ../../patches/futuresdr-zigbee-source-index.patch`.
Build from `projects/ieee802154-monitor/futuresdr-worker` to select its
[pinned nightly](../rust-toolchain.toml) and [lockfile](../Cargo.lock).

```console
cargo build --locked --offline --release
target/release/ieee802154-futuresdr-worker INPUT.cf32 \
  --output OUTPUT.i154raw --channel 17 \
  --max-input-mib 256 --max-frames 1000000 --max-output-mib 64
```

The channel and all three limits are required. The input must be a regular,
nonempty file of complete complex samples with finite components. The worker
checks the samples before and during decoding.

- `--decoder-lane hard` (default) uses the pinned FutureSDR decoder with
  threshold 6. `--decoder-lane soft` uses signed normalized DSSS correlation.
  Acquisition and header symbols require a unique score above `sqrt(0.6)`.
  Payload symbols use the unique finite maximum. The correlation is a relative
  metric.
- `--carrier-offset-hz` accepts a measured signed input offset from
  `-100000` through `100000` Hz. The worker applies the opposite rotation,
  starting at phase zero on the first sample. For an input measured at
  −100 kHz, use `--carrier-offset-hz=-100000`. The default of zero bypasses
  correction. To run multiple offsets, follow the
  [carrier hypothesis guide](../../docs/guide.md#compare-carrier-offset-hypotheses).
- `--executor-threads` accepts 1–8 unpinned threads. Omit it to use FutureSDR's
  default scheduler sizing.

Keep the exact invocation and a distinct output path for each lane or carrier
hypothesis. The [I154RAW v1 format](../../docs/guide.md#inspect-demodulator-candidates)
stores the channel and candidate chip counts. Lane and offset provenance come
from the invocation.

## Candidate source positions

The hard lane accepts `--source-index-output OUTPUT.i154idx`. I154IDX v1 uses
little-endian fields and a 24-byte header.

| Offset | Field |
| --- | --- |
| 0–7 | `I154IDX\0` |
| 8–9 | version `1` (`u16`) |
| 10–11 | header length `24` (`u16`) |
| 12–15 | reserved `0` (`u32`) |
| 16–19 | input rate `4_000_000` samples/s (`u32`) |
| 20–23 | recovered chip rate `2_000_000` chips/s (`u32`) |

Each 16-byte record pairs the archive's one-based recovered-chip count (`u64`)
with the zero-based CF32 input sample index through which clock recovery
required input to emit that chip (`u64`). The latter includes clock lookahead,
is nondecreasing and lies within the input file. It is a file sample position.
Use the archive and index from the same successful invocation and verify that
record counts and corresponding chip counts match.

The worker writes mode-0600 `.partial` files before syncing and publishing each
output without replacement. It publishes the archive before the optional
index. A failure can leave partial files or just one final output. Require a
zero exit after the completion summary before using the outputs.

## Host checks

Run `cargo test --locked --offline` from the worker package directory. The
[decoder tests](../src/decoder_test.rs) cover exact frame recovery, graded
synthetic noise, explicit carrier correction, the three-hypothesis search and
the 20-to-4 MSps channelizer handoff. These fixtures exercise file processing
and decoder behavior. RF sensitivity, occupancy and capture continuity
require separate measurements.
