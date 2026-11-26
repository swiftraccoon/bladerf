# IEEE 802.15.4 monitor

Prepare bounded bladeRF IQ captures for passive IEEE 802.15.4 decoding and
independently inspect raw decoder candidates. The stable tool validates a
retained SC16_Q11 capture and its sidecar, translates selected IEEE channels or
one arbitrary in-band frequency to baseband, filters it, decimates from 20 MSps
to 4 MSps, and validates the separate file-only worker's candidate archive.
Neither tool opens a radio, listens on a network socket or transmits.

## Channelize a capture

Build from the workspace root with a C toolchain for `ring`'s SHA-256 backend.
Supply a new output directory:

```sh
cargo build --locked --release -p ieee802154-monitor
target/release/ieee802154-monitor channelize captures/wifi-session/iq.sc16q11 \
  --channels 16,17,18 \
  --max-samples 20000000 \
  --output artifacts/ieee802154-channelize/session
```

The input sidecar must be adjacent to the IQ file as `INPUT.json` (for example,
`iq.sc16q11.json`). It must describe little-endian interleaved SC16_Q11 at a
20 MHz sample rate and bandwidth, and its sample count must exactly match the IQ
byte length. Every processed component is checked against the Q11 range. Input
and metadata size limits are checked before derived output is created; an RF
component error encountered while streaming leaves clearly named partial files
inside the new private output directory.

The defaults are every fully covered channel for a 2437 MHz, 20 MSps source:
16 at 2430 MHz, 17 at 2435 MHz and 18 at 2440 MHz. IEEE 802.15.4 2.4 GHz channel
centers follow `2405 + 5 * (channel - 11)` MHz. Requested channels normally need
positive source Nyquist margin beyond the 2 MHz stopband edge. Add
`--channels 19 --allow-nyquist-edge` at 2437 MHz to admit exactly zero margin.
The manifest records that margin. Analog reception at the edge may vary.

For protocol-neutral follow-up at one exact frequency, use the additive path:

```sh
target/release/ieee802154-monitor channelize-frequency captures/session/iq.sc16q11 \
  --frequency-hz 2438000000 \
  --max-samples 20000000 \
  --output artifacts/frequency-channelize/session
```

This writes `frequency-2438000000.cf32` and
`channelize-frequency.json`. The separate schema-1 manifest hashes the
acquisition sidecar, processed source prefix and output; records the exact
target, filter and source-to-output sample-index mapping; and does not change
the established IEEE `channelize` files or manifest. The same strict 2 MHz
stopband-margin, resource, private-mode and no-replace rules apply. Its derived
sample indices establish neither RF time, source continuity nor protocol
identity.

## Derived files and signal contract

| File | Meaning |
| --- | --- |
| `channel-N.cf32` | Little-endian complex `f32` I,Q samples centered on channel N at 4 MSps |
| `channelize.json` | Validated source settings, filter contract, counts, hashes and relative amplitude summaries |
| `channel-N.cf32.partial` | Incomplete derived output retained when streaming or publication fails |
| `channelize.json.partial` | Complete manifest staging name retained when publication or cleanup fails |

The channelizer uses a normalized 225-tap Kaiser-windowed sinc filter with beta
7.85726, a 1.75 MHz design cutoff, a declared 1.5 MHz passband edge and a 2 MHz
stopband edge before decimation by five. Its 112-source-sample group delay and
44 zero-history-affected output samples are retained and recorded. The first
output follows source sample index 4, and the finite input is not tail-flushed.
Host tests require at most 0.05 dB
passband loss, at least 70 dB numerical stopband attenuation and at least 60 dB
end-to-end rejection of a decimation-alias tone.

Output sample count is exactly `floor(processed_input_samples / 5)`. Hashes cover
the processed raw prefix and every derived stream. Files are mode 0600 within a
new mode-0700 directory, synchronized, and published without replacing an
existing name. A concurrent same-length mutation of an input file is not an
immutable-snapshot guarantee; the manifest records that source continuity is
unknown. Channel files are published sequentially before the manifest, so an
I/O failure can leave a valid subset rather than an atomic multi-file result.
Publication requires same-filesystem hard-link support; there is no overwrite
fallback.

The pinned FutureSDR Zigbee example expects channel-centered native `Complex32`
at 4 MSps. `CF32_LE` has that in-memory byte representation on the supported
little-endian host. The isolated worker in `futuresdr-worker/` reuses only the
bounded file receive chain, retains every raw PSDU candidate that decoder emits,
and exposes no live-radio or network interface. It has its own exact nightly,
lockfile and [worker guide](../futuresdr-worker/README.md) because pinned FutureSDR
currently requires nightly Rust.

Channelized samples are evidence for later signal processing, not proof of a
protocol, packet, transmitter, capture continuity or channel occupancy. Reported
RMS and peak values are relative normalized sample amplitudes, not calibrated
dBm. No RF timestamp is assigned to derived samples.

## Demodulate a channel file

Apply the tracked narrow FutureSDR patch as described in the
[worker guide](../futuresdr-worker/README.md), build that isolated workspace, and
run it on one derived channel:

```sh
projects/ieee802154-monitor/futuresdr-worker/target/release/ieee802154-futuresdr-worker \
  artifacts/ieee802154-channelize/session/channel-16.cf32 \
  --output artifacts/ieee802154-channelize/session/channel-16.i154raw \
  --channel 16 \
  --max-input-mib 64 \
  --max-frames 1000000 \
  --max-output-mib 64
```

Repeat for each derived channel. A successful header-only archive means the
decoder produced no candidate from that finite input; it does not establish that
the RF channel was inactive or that every possible frame would have been
recoverable. Validate each archive with `inspect-candidates` below before making
any FCS or MAC-field claim.

## Inspect demodulator candidates

The file-only demodulator writes every PSDU candidate, including failed-FCS
candidates, to a bounded versioned archive. Inspect such an archive without
opening a radio or network socket:

```sh
target/release/ieee802154-monitor inspect-candidates candidates.i154raw \
  --max-input-mib 64 \
  --max-output-mib 512 \
  --max-records 1000000 \
  --output artifacts/ieee802154-candidates/session
```

Version 1 uses little-endian integers. Its file header is exactly 24 bytes:

| Offset | Size | Field |
| --- | --- | --- |
| `0..8` | 8 | `I154RAW\0` magic |
| `8..10` | 2 | version, exactly `1` |
| `10..12` | 2 | header length, exactly `24` |
| `12` | 1 | IEEE 802.15.4 channel, `11..=26` |
| `13` | 1 | file flags, exactly zero |
| `14..16` | 2 | reserved, exactly zero |
| `16..20` | 4 | channel sample rate, exactly `4_000_000` Hz |
| `20..24` | 4 | recovered chip rate, exactly `2_000_000` chips/s |

Every record has this exact 20-byte header, followed immediately by its PSDU:

| Record offset | Size | Field |
| --- | --- | --- |
| `0..4` | 4 | `PSDU` magic |
| `4..8` | 4 | total record length, exactly `20 + psdu_length` |
| `8..16` | 8 | positive recovered-chip count at candidate completion |
| `16..18` | 2 | PSDU length, `1..=127` |
| `18..20` | 2 | record flags/reserved, exactly zero |
| `20..record_length` | variable | complete PSDU, including transmitted FCS |

Recovered-chip counts must be strictly increasing throughout the archive.

The inspector computes the IEEE 802.15.4 reflected CRC-16 independently using
polynomial `0x8408`, initialization zero and no final XOR. Every candidate is
classified in `packets.jsonl`; only full PSDUs whose trailing little-endian FCS
matches are copied to nanosecond-resolution classic PCAP with DLT 195.
`summary.json` reports counts, limits and hashes. All three files use synchronized
`.partial` staging names and no-replace hard-link publication in a new private
directory. Publication is sequential rather than an all-files transaction:
`packets.jsonl` and `validated.pcap` are published before `summary.json`. A failed
run may therefore leave a distinguishable subset of final and `.partial` files;
the summary's presence marks completion.

Input and combined-output limits are each `1..=8192` MiB, and the record limit is
`1..=10_000_000`. The output limit includes `packets.jsonl`, `validated.pcap` and
`summary.json`. Parsing uses fixed-size I/O and PSDU buffers; a rejected run may
retain private `.partial` files for diagnosis.

To inspect part of a channel replay, pass `--source-index INDEX.i154idx`
from the [same hard-worker invocation](../futuresdr-worker/docs/guide.md#candidate-source-positions),
`--source-complex-samples N` for the full CF32 input length, and
`--owned-start-channel-sample A --owned-end-channel-sample B`. The inspector
checks every record and publishes candidates with a required input sample
in `[A, B)`. Schema 2 summarizes the index identity and the FCS counts
outside that interval. PCAP contains only validated packets in the interval.

Classic PCAP requires timestamps. These records use relative candidate-completion
time derived from the software decoder's recovered chip count at 2 Mchip/s, whose
500 ns period is represented exactly. They are not wall-clock, hardware-sample or
RF timestamps. Classic PCAP limits these relative seconds to values below `2^32`;
the inspector rejects an archive if any candidate completion time cannot be
represented.

Basic MAC fields are conservative observations only. The JSON retains raw frame
type and frame-version codes. Universal type names are used only for codes 0..=3,
type 4 remains reserved, and codes 5..=7 remain
`version_dependent_or_reserved`. Sequence-suppression and information-element
semantics are applied only to ordinary 2015 frames; old-version reserved bits do
not suppress the sequence number. Reserved frame versions and version-dependent
types are retained as raw frame-control observations rather than interpreted as
ordinary MAC headers, and malformed/truncated states remain explicit. Valid FCS
checks consistency but does not authenticate a sender, identify Zigbee, decrypt
content, prove continuity or establish channel occupancy.

For FCS-valid packets with a complete frame control field, `mac.addressing`
reports the address layout status. An `observed` layout includes the transmitted
source and destination PAN IDs and addresses when present. The supported layouts
are beacon, data, acknowledgment and MAC command frames with version codes 0–2.
Each address has a `mode` and `value`. Short addresses and PAN IDs are 16-bit
integers. Extended addresses are 16 lowercase hexadecimal digits with leading
zeros preserved. Use a short address with its PAN context. PAN compression can
omit fields from the frame. The report retains the compression bit and lists
only transmitted PAN IDs. A truncated or unsupported layout reports its status
without address values. Sender attribution requires independent evidence.

## Compare hard and soft decoder lanes

Run the hard and soft worker lanes separately, retain their explicit lane-bearing
completion output, and compare their archives without treating decoder overlap as
two RF observations:

```sh
target/release/ieee802154-monitor compare-candidates \
  channel-17-hard.i154raw channel-17-soft.i154raw \
  --max-input-mib 64 \
  --max-records 1000000 \
  --output artifacts/ieee802154-candidates/channel-17-comparison
```

The comparer is file-only, rejects symlink inputs, requires matching version-1
channel/rate headers, and streams both strictly position-ordered archives with
bounded memory. It hashes both complete inputs while reading and detects ordinary
path, identity, length or modification-time changes before publishing a private
mode-0600 `summary.json` in a new mode-0700 directory. The configured byte and
record bounds apply independently to each lane; the fixed-size summary is capped
at 64 KiB.

Exact overlap means the channel, recovered-chip completion position, PSDU length
and every PSDU byte all match. The same PSDU at different positions is distinct,
and different PSDUs at the same position remain one hard-only plus one soft-only
identity. The summary binds the role-specific archive lengths and SHA-256 hashes,
partitions hard-only, soft-only, overlap and union counts, and independently
recomputes the corresponding FCS-valid partitions. It retains no PSDU bytes or
candidate positions.

This identity union is a decoder-output accounting device, not unique over-air
frame deduplication. It does not establish RF time, sender identity, Zigbee,
decryption, authentication, continuity, completeness or better receiver
sensitivity. A precisely timed adversarial writer capable of restoring the same
file metadata between reads remains outside the ordinary-mutation guarantee.

## Compare carrier-offset hypotheses

Compare exactly six caller-described decoder hypotheses with repeated, separate
`LANE OFFSET_HZ PATH` values. No field is parsed from a delimiter-bearing path:

```sh
target/release/ieee802154-monitor compare-hypotheses \
  --source hard 0 channel-17-offset-0-hard.i154raw \
  --source soft 0 channel-17-offset-0-soft.i154raw \
  --source hard -100000 channel-17-offset-negative-100000-hard.i154raw \
  --source soft -100000 channel-17-offset-negative-100000-soft.i154raw \
  --source hard 100000 channel-17-offset-positive-100000-hard.i154raw \
  --source soft 100000 channel-17-offset-positive-100000-soft.i154raw \
  --max-input-mib 64 \
  --max-records 1000000 \
  --output artifacts/ieee802154-candidates/channel-17-hypotheses
```

Lane values are `hard` or `soft`; offsets are signed integers bounded to
`-100000..=100000` Hz; and every `(offset, lane)` descriptor must be unique.
Each source must also be a distinct direct regular file; reusing one inode under
multiple descriptors is rejected. Source order is preserved. Bits 0 through 5
in each nonzero membership mask correspond to the six `--source` groups in
caller order, and a deterministic mask histogram reports how many exact
identities occur in each source subset.

The comparer performs an N-way streaming union with memory bounded independently
of archive length. Its identity is exactly channel, recovered-chip completion
position, PSDU length and all PSDU bytes. Repeated equal PSDUs at different
positions remain distinct, as do a position-shifted detection in another
hypothesis and different PSDUs at the same position. It therefore cannot merge
distinct real transmissions merely because their complete PSDUs are equal, but
it may conservatively retain multiple outputs that came from one physical frame
when decoder hypotheses report different completion positions.

FCS is recomputed independently into valid, invalid and invalid-length states.
The summary includes ordered source descriptors, input lengths and SHA-256
digests; raw-source sums, exact-identity union counts and duplicate counts; and
the same three-state FCS partition for every aggregate. Closure checks require
the membership histogram to reproduce both the union and every source total.
The private fixed-size summary contains no input paths, PSDU bytes or candidate
positions.

Version-1 archives do not embed lane or carrier-offset metadata. The descriptor
is therefore a caller provenance claim bound to that source's digest in the
summary, not a claim verified from the archive. All file-only input, mutation,
size, record-count, output-collision and interpretation limits described for the
two-lane comparer also apply here, independently to each of the six sources.
