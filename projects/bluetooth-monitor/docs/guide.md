# Bluetooth monitor

Receive Bluetooth traffic with a bladeRF, retain the decoder's packet candidates,
and inspect advertising data with an independent Rust CRC check. The project
provides bounded capture sessions, Wireshark-compatible files and JSON records.
It operates passively; observing a scan request or connection request does not
require this receiver to send one.

## Capture and inspect

Run from the workspace root after building the selected native decoder:

```sh
cargo run --locked -p bluetooth-monitor --release -- capture --help
cargo run --locked -p bluetooth-monitor --release -- capture --duration 15 --output captures/bluetooth-session
```

The default Blue Dragon capture requests 32 MHz centered at 2414 MHz. That window
includes primary advertising channels 37 and 38; channel 39 is outside it.
Select a different window with `--channels` and `--center-mhz`. To request the
wideband mode:

```sh
cargo run --locked -p bluetooth-monitor --release -- capture --all-channels --duration 15 --output captures/bluetooth-wideband
```

To select a narrow window around primary channel 37 and a receive connector:

```sh
cargo run --locked -p bluetooth-monitor --release -- capture --engine blue-dragon --antenna rx1 --channels 8 --center-mhz 2402 --gain 30 --duration 15 --output captures/bluetooth-primary37
```

Blue Dragon accepts `--antenna rx1` or `--antenna rx2` and a manual `--gain`;
the native driver checks the applicable gain range. Match the connector to the
actual receive feed. Select the optional
ICE9 baseline with `--engine ice9`; it supports RX1 with fixed 30 dB gain.

Use a new output directory for each session. Inspect a saved candidate capture
without opening a radio:

```sh
cargo run --locked -p bluetooth-monitor --release -- inspect captures/bluetooth-session/candidates.pcap --output artifacts/bluetooth-inspect
```

Blue Dragon `--save-iq` sessions retain channelized squelch-triggered records
and, when coded scanning is enabled, continuous-scan windows in `bursts.bdb`.
Validate and summarize an existing archive without opening a radio or starting
the decoder:

```sh
cargo run --locked -p bluetooth-monitor --release -- inspect-bursts captures/bluetooth-session/bursts.bdb --max-input-mib 512 --output artifacts/bluetooth-burst-inspect
```

`inspect-bursts` accepts only a stable direct regular archive. Its shared parser
pins the path and open-file identity for the complete validated read.

Classify bounded candidate waveform families in the squelch records of an
existing archive, still without opening a radio or decoder:

```sh
cargo run --locked -p bluetooth-monitor --release -- classify-bursts captures/bluetooth-session/bursts.bdb --max-input-mib 128 --max-samples 50000000 --output artifacts/bluetooth-burst-families
```

`classify-bursts` accepts only a stable direct regular BDB v1 file. It hashes and
validates the complete input, accounts for scan windows without classifying
them, and writes a private schema-2 `burst-families.json` into a new output
directory. Defaults limit the input to 128 MiB, total archive samples to 50
million, records and groups to 100,000 each, and the terminal report to 16 MiB;
the CLI can configure them within hard maxima of 512 MiB, 200 million samples,
100,000 records/groups and 256 MiB of report data. Publication is synced and
no-replace, and an interruption before publication leaves no terminal report.

The families are provisional signal-shape candidates, not protocol identities.
The binary-FSK-like gate combines active-envelope variation, circularly
recentered instantaneous frequency, two-level balance/separation, variance
reduction, within-cluster contrast and transitions. Multibin width is the
maximum number of simultaneously active contiguous 1 MHz records using
half-open, unexpanded record intervals; a temporally chained union cannot become
a wideband candidate. The report contains aggregate numeric distributions and
counts only. Its sparse family-by-frequency rows count records and samples in
each observed 1 MHz bin and count each candidate group once per distinct bin.
No source path, absolute timestamp, IQ, bits, payload or device identifier is
emitted.

For one already classified family/frequency row, an optional aggregate gate
breakdown can explain why its records did or did not pass the binary-FSK-like
feature gates:

```sh
cargo run --locked -p bluetooth-monitor --release -- classify-bursts \
  captures/bluetooth-session/bursts.bdb \
  --feature-breakdown-family narrowband_constant_envelope_unresolved_candidate \
  --feature-breakdown-frequency-mhz 2438 \
  --output artifacts/bluetooth-burst-features
```

The two options are required together, use the closed family names from the
schema-2 report and accept 2402--2480 MHz. The requested row must exist. Without
them, `burst-families.json` is unchanged; with them, a separate private schema-1
`feature-breakdown.json` adds closed row accounting, bounded numeric summaries
and a declared first-failure partition. It still emits no IQ, timestamps,
archive indices, paths, packet content or protocol identity.

Compare one classified 2 MSps row with a separately retained 4 MSps stream to
diagnose phase-frequency aliasing without opening a radio:

```sh
cargo run --locked -p bluetooth-monitor --release -- alias-control \
  artifacts/bluetooth-replay/bursts.bdb artifacts/channelized/frequency-2438000000.cf32 \
  --manifest artifacts/channelized/channelize-frequency.json \
  --replay-manifest artifacts/bluetooth-replay/replay.json \
  --family narrowband_constant_envelope_unresolved_candidate \
  --frequency-mhz 2438 --output artifacts/bluetooth-alias-control
```

The BDB, replay manifest and `channelize-frequency` result must describe the
same complete 20 MSps extent. The action independently hashes the BDB and CF32
files, validates bounded stable inputs, and writes a private schema-3
`alias-control.json` into a new directory. Defaults cap the BDB at 128 MiB and
10 million samples, CF32 at 64 MiB, records/groups at 20,000 each, and output at
16 MiB. Signed direct frequencies are baseband offsets from the selected row.
The bandwidth-matched branch filters one retained stream once, then compares its
4 MSps output with both absolute 2 MSps decimation phases; it is not a bit-exact
Blue Dragon PFB result. Other paired fits remain filter-sensitive waveform
diagnostics, the replay manifest does not cryptographically bind BDB bytes to raw
IQ, and the BDB-to-CF32 offset is provisional. The report does not establish a
protocol, decoder result or device. Closed first-failure partitions and
non-exclusive failure counts explain qualification without per-record features.

Sample fixed windows selected without burst triggers or signal metrics from the
same authenticated 4,000,000-sample CF32 contract without opening a radio:

```sh
cargo run --locked -p bluetooth-monitor --release -- background-controls \
  artifacts/channelized/frequency-2438000000.cf32 \
  --manifest artifacts/channelized/channelize-frequency.json \
  --frequency-mhz 2438 --output artifacts/bluetooth-background-controls
```

The private schema-1 aggregate excludes only the channelizer FIR startup
transient and compares filtered 4 MSps with both absolute 2 MSps phases. Its
fixed windows do not validate receiver settling, continuous coverage, signal
absence or protocol identity, and cannot promote candidates.
The CF32 input is capped at 64 MiB, output defaults to a 16 MiB cap, and
publication never replaces an existing report.

Replay a complete retained 20 MSps bladeRF capture through Blue Dragon without
opening a radio or requesting network or transmit access:

```sh
cargo run --locked -p bluetooth-monitor --release -- replay-iq captures/wifi-session/iq.sc16q11 --center-mhz 2437 --sample-rate 20000000 --max-samples 20000000 --squelch -45 --decoder-timeout 120 --burst-limit-mib 128 --output-limit-mib 512 --output artifacts/bluetooth-replay
```

An opt-in discovery replay can continuously search an in-band primary
advertising channel for CRC-valid LE Coded packets. For example, the 20-channel
grid centered at 2435 MHz contains the 2426 MHz primary channel:

```sh
cargo run --locked -p bluetooth-monitor --release -- replay-iq captures/session/iq.sc16q11 --center-mhz 2435 --sample-rate 20000000 --max-samples 20000000 --coded-scan --output artifacts/bluetooth-coded-replay
```

The wrapper rejects `--coded-scan` before input access or output creation unless
the exact channelizer grid contains 2402, 2426 or 2480 MHz. Schema-2
`replay.json` records whether the scan was requested and the exact primary
frequencies searched. Scan windows and decoded records are observations, not
devices or occupancy, and the bounded window is not an exhaustive decoder for
every valid maximum-length LE Coded packet.

### Selected LE Coded scan

To search one BLE data-channel frequency in saved IQ, use
`--coded-scan-frequency-mhz 2444` on a capture centered at 2437 MHz. This
option selects one interior channelizer bin and uses the existing CRC checks.
Select one of the two scan options per replay. At 20 MSps, the first continuous
scan window uses up to 327,680 complex input samples. Version 2 of selected
file scanning includes the final partial window, retains up to 34,080 channel
samples of overlap, and supports payloads up to 255 bytes. The receipt records
the frequency, window geometry, and retained window count. Continuous scan
can recover multiple CRC-valid packets in each window using a bounded search
for hard and soft preamble matches.
When the initial attempts fail CRC validation, continuous replay can retry FEC
Block 2 within the declared packet length and TERM2. A retry must reproduce
the length and pass CRC. A final S=2 retry limits outlying symbol metrics.
If a selected-file scan window yields no CRC-valid Coded packet, it retries
once with a centered 450 kHz low-pass filter on the same sample grid.
If that retry is empty, the scanner can reduce the weight of large phase
excursions during the S=2 advertising payload check.
Validating a connected data PDU requires an exact access address and CRC
initialization already learned in the same replay.
On the selected bin, a complete CRC-valid Coded `AUX_CONNECT_REQ` can supply
that context for later packets until the request's supervision timeout elapses.
Add `--omit-scan-iq` to keep ordinary burst IQ and drop overlapping scan IQ.
Replay schema 3 records the verified frequency, completed window count and
first and last window spans in 2 MSps channel samples.
Add `--omit-squelch-iq` with `--omit-scan-iq` for packet-only selected replay.
Schema 4 requires full-tail scan coverage and a valid empty BDB v1 archive.
`selected_coded_packets` counts independently CRC-validated S=2 and S=8 packets
labeled with the selected frequency across continuous and squelch decoding.

### Saved IQ input and verification

Replay requires the adjacent acquisition sidecar `INPUT.json` (for example,
`iq.sc16q11.json`). The sidecar and file must describe exact little-endian
interleaved SC16_Q11 I/Q with scale 2048, 20 MSps, the requested center and an
exact byte length of four times its complex-sample count. Every component is
checked to be in `-2048..=2047` before output creation. `--max-samples` is a
full-input acceptance ceiling: a larger file is rejected rather than silently
decoding a prefix. Input, sidecar and decoder file identities are checked again
after the child exits. The pinned decoder file source uses blocks aligned to the
20-channel PFB's ten-complex-sample step; replay also requires a divisible source
extent and accepts `processed_full_input` only when Blue Dragon's final statistics
report the exact sidecar sample count. The terminal manifest records the input,
decoder-reported count, step, aligned block size and total processed steps. It
also records a path-free schema-1 program identity containing the fixed
`blue-dragon` name, executable byte length and SHA-256 digest. Replay first
canonicalizes the selected path, then accepts only that bounded regular
executable target with an execute bit. It hashes the target before creating
output, rechecks it immediately before launch and rehashes it after the child
completes. The private request/replay files retain the resolved path and
filesystem identity for local diagnosis; the program identity can be copied
without those host-specific fields.

The wrapper invokes only Blue Dragon's file source. It supervises wall time and
decoder output, retains a bounded channelized burst archive, and then runs the
same PCAP and burst inspectors used for capture. `replay.json` is written last
through a synced private staging file and no-replace publication, only after
those steps succeed. Its `outcome` distinguishes
`completed_zero_candidates`, `completed_candidates_without_validated_ble` and
`completed_with_validated_ble`. A zero-candidate result is a successful decoder
run, not evidence that the source recording contained no Bluetooth signal.
The 20 MHz window centered at 2437 MHz does not contain any of the three primary
LE advertising centers, so it can observe in-window LE data/secondary traffic
or Classic traffic but is not a normal primary-advertisement discovery window.

The output separates evidence from interpretation:

| File | Purpose |
| --- | --- |
| `candidates.pcap` | Original decoder packet candidates, including unverified or corrupt records |
| `validated.pcap` | BLE packets accepted by the independent CRC validation path |
| `packets.jsonl` | Packet-by-packet classifications and decoded fields |
| `summary.json` | Counts grouped by classification and observable packet content |
| `request.json` | Requested capture/replay settings and decoder arguments |
| `capture.json` | Stop reason, decoder exit status and elapsed session time |
| `decoder.log` | Decoder diagnostics, including processing or sample-drop indications |
| `bursts.bdb` | Blue Dragon channelized IQ windows retained by capture or derived during replay |
| `bursts.json` | Structural validation and aggregate metadata for a Blue Dragon burst archive |
| `burst-families.json` | Aggregate provisional waveform-family candidates from a bounded existing burst archive |
| `feature-breakdown.json` | Optional aggregate feature-gate diagnostics for one requested family/frequency row |
| `alias-control.json` | Evidence-limited 2-versus-4 MSps phase-alias diagnostics for one classified row |
| `replay.json` | Terminal offline result, emitted only after full-input replay and both inspectors succeed |

`capture.json` describes live capture sessions; replay also writes its checked
request and decoder diagnostics to `request.json` and `decoder.log`.
Packet inspection produces the analysis files from an existing candidate PCAP.
Burst inspection produces `bursts.json`; a Blue Dragon capture with `--save-iq`
also produces it automatically after the native decoder stops.

`summary.json` also reports bounded duplicate accounting for independently
CRC-valid BLE packets. A normalized body is the access address plus dewhitened
PDU, excluding its coding indicator, redundant CRC and all capture metadata
(timestamp, RF channel, relative power and pseudoheader), so the same transmitted
body seen on different RF channels or LE Coded schemes counts once. The
duplicate-accounting object retains at
most 4,096 such bodies in memory and serializes only aggregate scalar counts. It
never serializes the bodies, their hashes, addresses or payloads. `exact` remains
true when every validated packet was classified as a first distinct body or a
repeat. If the capacity is filled and another unseen body arrives,
`unresolved_validated_packets` increases and `exact` becomes false; the distinct
and repeat counts are then lower bounds. Distinct bodies are not device, sender
or on-air-message counts: retransmission, address rotation, payload changes and
incomplete reception still limit that interpretation.

The same summary partitions every independently validated BLE packet into
unknown-encapsulation, LE 1M, LE 2M, LE Coded S=8 or LE Coded S=2 counts. For LE
Coded PHDR records, the inspector treats the coding indicator as separate from
the whitened and CRC-covered PDU; reserved PHY or coding-indicator values remain
unsupported candidates rather than trusted output.

Default output directories are under the workspace's `captures/bluetooth/` and
`artifacts/bluetooth-inspect/`, while standalone burst summaries default under
`artifacts/bluetooth-burst-inspect/` and IQ replay defaults under
`artifacts/bluetooth-replay/`. Burst-family classification defaults under
`artifacts/bluetooth-burst-families/`; alias control defaults under
`artifacts/bluetooth-alias-control/`. Explicit relative output paths also resolve
from the workspace root. New operation directories use mode 0700; accepted
replay files, including decoder-created PCAP and burst artifacts, are checked as
regular files and protected to mode 0600 before inspection. Keep actual session
outputs local; these directories are ignored by Git.

The duration includes decoder initialization. The wrapper interrupts the decoder
at the duration or recording limit, allows five seconds for shutdown, then kills
and reaps a stalled process. `--output-limit-mib` limits the combined candidate,
log and IQ files; its default is 2048 MiB. It does not limit the analysis files
written after capture. Recording limits are soft because polling and native
buffering can overshoot them. Existing output directories are
preserved. Live capture returns an error on decoder failure, unexpected early
exit, forced termination, reported receive or USB errors, candidate PCAP or
burst archive write errors, or decoder logs exceeding the 16 MiB diagnostic
scan limit. It retains partial files for inspection. `capture.json` records
the scan's coverage, expected burst-archive size-limit markers and processing
warnings separately. Warnings such as dropped samples and nonzero overflow or
load-shedding counters indicate possible gaps. Continuity and RF activity
remain uncertain even when the scan finds no errors and the capture is empty.

Offline replay rejects timeout, interruption, decoder/input/PCAP/burst-write
errors, output-limit exhaustion, changed source or decoder files and diagnostic
logs too large for its complete 16 MiB scan. It retains partial artifacts for
diagnosis and hardens any decoder-created PCAP/burst partials to mode 0600 after
stopping the child, but does not create the terminal `replay.json` on those
failures. A burst archive that reaches its configured limit remains valid but
is explicitly marked truncated-by-limit in the terminal manifest.

## What can be observed

Bluetooth LE uses 40 channel centers from 2402 to 2480 MHz. The three primary
advertising channels are logical channels **37 at 2402 MHz**, **38 at 2426 MHz**
and **39 at 2480 MHz**. A receiver observes only signals inside its usable
passband while it is listening. The [Bluetooth LE PHY specification][le-phy]
defines the modulation and channel arrangement; the [Link Layer specification][ll]
defines the packet fields.

| Traffic | Useful information | Boundary |
| --- | --- | --- |
| Legacy advertisements | Advertiser address/type, flags, local names, service UUIDs, manufacturer/service bytes and other AD structures when present | Fields are optional; advertising need not contain a name or identify every service |
| Directed advertisements | Advertiser and target addresses/types | Describes the transmitted addresses, not a permanent device identity |
| Observed scan requests and responses | Scanner/advertiser addresses and extra response AD structures | The monitor receives exchanges initiated by other devices |
| `CONNECT_IND` | Initiator/advertiser addresses, connection access address, CRC initialization, interval/window, latency, timeout, channel map and hop/clock fields | These fields alone do not establish successful connection following |
| Extended advertising | Header/data bytes captured on a supported PHY, including auxiliary pointers or advertising identifiers when decoded | The primary packet can point to additional packets on other channels/PHYs; a complete advertising set is not reconstructed automatically |
| Connected BLE traffic | Access addresses, header fields and opaque payload bytes; independent CRC checks when a previous valid connection request supplies CRC initialization | Receiving a connection request does not guarantee channel following, complete traffic or decryption |
| Classic Bluetooth | Blue Dragon's baseband/header/payload candidates and upstream integrity claims; ICE9 access-code/LAP detections | Classic CRC claims are recorded separately and are not independently validated by this project's Rust parser |

AD structures follow the Bluetooth [data-types specification][ad-types]. Their
raw bytes remain useful even when a vendor-specific payload has no implemented
interpretation. Advertised TX power is a field sent by the remote device; it is
different from received signal amplitude.

CRC-valid advertisements with complete Ruuvi RAWv2 manufacturer data include a
`ruuvi_raw_v2` object in the corresponding AD field of `packets.jsonl`. Its
`sensor_id` is the MAC carried in the payload. The packet's advertiser address
has its own field. The object also reports temperature in millidegrees Celsius,
relative humidity in ppm (100% equals 1,000,000 ppm), pressure in pascals,
acceleration in milligravity units in X, Y, Z order, battery voltage in
millivolts, advertised transmit power in dBm, and motion and measurement
counters. Unavailable values are null. The `ruuvi_raw_v2_ad_structures` count
in `summary.json` includes repeated advertisements. Compare the payload ID
with a known sensor for attribution. [Ruuvi RAWv2 format](https://docs.ruuvi.com/communication/bluetooth-advertisements/data-format-5-rawv2)

CRC-valid 16-bit service data for UUID `0xFCD2` may include a `bthome_v2`
object. It reports the trigger flag and, for unencrypted data, supported
button, contact, motion, telemetry and device-type fields. Its `objects`
array stores `kind` and `value` in wire order, including repeated entries.
`first_unknown_object_id` marks where interpretation stopped. Encrypted data
keeps its opaque bytes and an `encrypted` encoding label.
An embedded address, when present, is separate from the BLE advertiser.
The packet ID tracks repeated data and the device type identifies a class.
The optional `bthome_v2_ad_structures` summary count includes repeated
structures. [BTHome format](https://bthome.io/format/)
[Reference address extension](https://github.com/Bluetooth-Devices/bthome-ble/commit/1059aa1)

CRC-valid advertisements with complete 16-bit service data for Remote ID
(`0xFFFA`, application code `0x0D`) may include `remote_id_basic_ids` in the AD
field of `packets.jsonl`. Each entry reports the advertised identifier, ID
type, aircraft type, protocol version and message counter. `id_hex` preserves
all 20 identifier bytes. `id_text` is present for printable serial or
registration IDs. The packet's BLE advertiser address has its own field. The
optional `remote_id_basic_id_observations` summary count includes repeated
messages.
Extended packets qualify when their header identifies the start of advertising
data and the AD structure is contained in that packet. The values are sender
claims. [OpenDroneID wire layout](https://github.com/opendroneid/opendroneid-core-c/blob/8384b6a4cbe4a3a7ecfff2fbf71f03d38852e366/libopendroneid/opendroneid.h#L423-L437)

## Decoder scope

The capture layer reuses upstream SDR demodulation. The maintained orchestration
and independent packet inspection are Rust.

| Engine | Source capability | Important limits |
| --- | --- | --- |
| `ice9` | LE 1M candidates and Classic access-code/LAP detection | No LE 2M/Coded demodulation; only even-MHz Classic centers are processed; Classic detections are logged rather than written as complete packet records |
| `blue-dragon` (default) | Upstream Rust decoder targets LE 1M, LE 2M, LE Coded and Classic BR; EDR support is experimental | Enable its continuous coded scan with `--coded-scan`; broader capabilities require suitable bandwidth and signal quality and do not imply complete recovery of every connection |

The selected source artifacts are [ICE9][ice9] and [Blue Dragon][blue-dragon].
Their descriptions are version-qualified implementation capabilities, rather
than a statement that every listed path has been measured on every radio.

ICE9 emits candidates without a CRC check. Blue Dragon is invoked with its own
CRC checking enabled. The Rust inspection step checks BLE CRCs independently of
either decoder: ordinary advertising uses the advertising initialization, and
connected packets use the latest CRC-valid request for their access address
already read from the same file. Its capture timestamp must precede or equal
the packet timestamp. Bad CRCs, unsupported records and unknown CRC state have
separate outcomes. CRC validity checks packet consistency.

BLE link encryption uses [AES-128 CCM][le-security]. Encrypted connected traffic
and encrypted advertising data remain opaque without the applicable keys and
protocol state. This project does not derive keys or decode encrypted application
content. Names, service data and manufacturer data are reported only when those
fields are actually present and decodable.

## Bandwidth, timing and signal metadata

Both engines' `--all-channels` mode requests **96 MSps at 2441 MHz**, using bladeRF
oversample mode and SC8 Q7 samples. It selects software channels across the BLE
band; it does not guarantee flat sensitivity or lossless capture across that
band. The native driver's [oversample bandwidth handling][oversample] explicitly
qualifies bandwidth settings. USB and host processing also determine how many
packets reach the decoder. Inspect diagnostics and compare narrower windows when
checking coverage.

PCAP RF channel indexes are physical frequency indexes. For primary logical
channels 37/38/39, the corresponding indexes are 0/12/39. ICE9 timestamps are
host burst-detection times. Blue Dragon anchors a sample-count timeline to host
time; neither bladeRF path uses hardware sample timestamps. The signal/noise
values are relative receiver measurements, not calibrated dBm. The independent
output preserves those distinctions rather than presenting the values as absolute
RF power or precise over-the-air timing.

Use `--save-iq --iq-limit-mib 512` when further signal analysis is needed.
ICE9 records raw SC8 Q7 IQ samples to `iq.sc8q7`; Blue Dragon records channelized
bursts to `bursts.bdb`. Both recordings have the session's time and soft size
limits. A burst archive and a continuous IQ recording have different
replay and timing properties. Captured samples can support later decoder work,
but recording a waveform does not establish that its protocol was decoded.

Each Blue Dragon burst record is a 2 Msps channelized sample window: either a
squelch-triggered record or a periodic continuous-scan window. Records can
overlap, and the archive does not encode a one-to-one relationship to PCAP
packets. Consequently, `bursts.json` keeps squelch and scan-window counts
separate from decoded-packet counts: neither record counts nor summed record
durations establish a protocol, unique on-air duration, capture continuity or
channel occupancy. Its signal and noise values are relative receiver estimates
rather than calibrated dBm.

Burst-family classification uses those same validated 2 Msps records. An ideal
uniform scalar cancels from its envelope ratios and phase differences, but
quantization, clipping and AGC can still change the retained evidence; stored
scale and receiver metadata are record-relative rather than calibrated power. A
50 us same-or-adjacent-bin connectivity tolerance associates records into
candidate events, while multibin width uses the original half-open intervals.
The 2 Msps channelized bins overlap. A group can therefore contribute incidence
to more than one bin, so those incidence counts are neither unique-signal
counts nor channel occupancy; zero-record family/frequency combinations are
omitted from the sparse report.
Detector timestamps, squelch tails, optional EDR pre-roll, AGC, phase aliasing
at 2 Msps and incomplete channelized records all limit interpretation. In
particular, an FSK-like result does not establish Bluetooth, packet validity,
symbol timing or successful decoding; unsupported or ambiguous records remain
separately counted rather than being forced into a protocol label.

## Build the native engines

The Rust project uses the workspace [toolchain](../../../rust-toolchain.toml).
Initialize the upstream checkouts first:

```sh
git submodule update --init --recursive
```

Blue Dragon uses libbladeRF and `protoc`; the bladeRF-only feature selection
avoids its default USRP backend. Apply the patches before building:

```sh
git -C repos/blue-dragon apply ../../patches/blue-dragon-bladerf-rx-setup.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-sc16q11-file-source.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-pfb-wrap.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-pfb-eight-tap.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-frequency.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-multiple.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-known-address.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-context.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-trellis.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-omit-iq.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-termination.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-complete-window.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-soft-preamble.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-s2-metrics.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-omit-squelch-iq.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-narrow-fallback.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-coded-scan-phase-fallback.patch
git -C repos/blue-dragon apply ../../patches/blue-dragon-le1m-advertising-crc.patch
cp config/blue-dragon.Cargo.lock repos/blue-dragon/Cargo.lock
cargo build --release --locked --manifest-path repos/blue-dragon/Cargo.toml -p blue-dragon --no-default-features --features bladerf --target-dir build/blue-dragon --jobs 2
```

The PFB patches streamline coefficient wrapping and the eight-tap AVX2 path.
The LE 1M patch prefers a complete, CRC-valid advertising packet when burst
padding would favor a shifted header under the length heuristic.
The optional [AGC cutoff patch](../../../patches/blue-dragon-agc-gain-cutoff.patch)
moves the squelch threshold comparison into gain units. Use it on runtimes
where the rounded `f32` RSSI is monotone over the AGC gain clamp. Apply it
with `git apply` in the Blue Dragon checkout before building.

The separate lockfile pins dependencies of the upstream workspace. If the native
linker cannot find libbladeRF in a nonstandard library directory, supply its
directory through `RUSTFLAGS`, for example `RUSTFLAGS='-L native=/usr/local/lib64'`
before the build command when that directory contains the installed library.

The executable is `build/blue-dragon/release/blue-dragon`. Select it with
`--engine blue-dragon` when using the capture command, or use the default.
Check the executable and required source patch without opening a radio:

```sh
cargo run --locked -p bluetooth-monitor --release -- doctor --engine blue-dragon
```

The optional ICE9 engine needs libbladeRF, liquid-dsp, FFTW's float library,
C/C++ build tools and `xxd`. Apply the receive-path compatibility patch and build
its CPU backend:

```sh
git -C repos/ice9-bluetooth-sniffer apply ../../patches/ice9-bluetooth-bladerf-rx-bias.patch
cmake -S repos/ice9-bluetooth-sniffer -B build/ice9-bluetooth -DCMAKE_BUILD_TYPE=Release -DUSE_VKFFT=OFF
cmake --build build/ice9-bluetooth --target ice9-bluetooth --parallel 2
```

The executable is `build/ice9-bluetooth/ice9-bluetooth`. The two receive-path
patches explicitly disable the selected receive path's bias tee. The SC16_Q11
patch is file-only. `doctor` checks the decoder path and receive-setup patch.
For Blue Dragon it also checks the SC16_Q11 file-reader changes, which remain
intact across the Coded scan patch series. Build the decoder with the pinned
recipe to establish its provenance.

Both upstream projects retain their own licenses and source conventions. See the
repository
[setup guide](../../../docs/SETUP.md) and [contribution guide](../../../CONTRIBUTING.md)
for native prerequisites and project standards.

[le-phy]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/radio-physical-layer-specification.html
[ll]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/link-layer-specification.html
[ad-types]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/CSS_v14/out/en/core-supplementary-features/data-types-specification.html
[le-security]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/low-energy-link-layer-security.html
[ice9]: https://github.com/mikeryan/ice9-bluetooth-sniffer/tree/788c5bce998a2d15c6b26a7d729ad81a5d28a869
[blue-dragon]: https://github.com/alphafox02/blue-dragon/tree/7bc9ec6b9228852b752f7309e46ecb186b87b2eb
[oversample]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L1210-L1227
