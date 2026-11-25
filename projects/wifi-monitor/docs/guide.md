# Wi-Fi monitor

Capture Wi-Fi with a bladeRF, preserve the source IQ, and inspect recovered
802.11 frames with an independent Rust FCS check. The default workflow records
a short interval before decoding it, supporting legacy OFDM, DSSS/CCK and
single-stream HT20 without assuming that software keeps up with continuous
20 MSps input. An optional FutureSDR engine receives legacy OFDM live.

Both paths use the normal hosted FPGA and receive only. Select a receive
connector explicitly and use an RF path suitable for the requested band.

## Build and check

Run commands from the workspace root. Initialize the pinned source dependencies:

```sh
git submodule update --init --recursive
git -C repos/libcrafter apply ../../patches/libcrafter-ofdm-observability-recovery.patch
git -C repos/libcrafter apply ../../patches/libcrafter-ofdm-data-failure-accounting.patch
git -C repos/libcrafter apply ../../patches/libcrafter-rustfmt-boundary.patch
git -C repos/libcrafter apply ../../patches/libcrafter-dsss-barker-equalization.patch
git -C repos/libcrafter apply ../../patches/libcrafter-he20-su-signal.patch
```

The endpoint patch applies to the fetchable `f9ef000b` (`v0.3.5`) submodule
base and reproduces the qualified endpoint `f4266c51`. The accounting check
verifies the decoder API after the HE signal patch updates the OFDM receiver.
The other reverse checks cover the full formatting, Barker and HE patches.

```sh
git -C repos/libcrafter apply --reverse --check --include=crafter/src/radio/wifi/decoder.rs \
  ../../patches/libcrafter-ofdm-data-failure-accounting.patch
git -C repos/libcrafter apply --reverse --check \
  ../../patches/libcrafter-rustfmt-boundary.patch
git -C repos/libcrafter apply --reverse --check \
  ../../patches/libcrafter-dsss-barker-equalization.patch
git -C repos/libcrafter apply --reverse --check \
  ../../patches/libcrafter-he20-su-signal.patch
```

The combined endpoint patch preserves the original linear checkpoints
`261c4b44`, `5f2167e4`, `17fe56c9` and `f4266c51`; it deliberately excludes the
later experimental MCS7 shadow branches.
The narrow [DATA-failure accounting patch][crafter-data-failure-patch] applies
after that endpoint and adds the closed legacy, HT-BCC and HT-LDPC terminal
failure partition.
The separate two-line [formatting-boundary patch][crafter-format-patch] keeps
root stable-workspace formatting checks from restyling the nested Rust-2021
upstream repository; it changes no compiled code.

The current host target is Linux; orchestration uses Unix process signals.
The default project uses the workspace's stable
[Rust toolchain](../../../rust-toolchain.toml). Native capture requires libbladeRF,
SoapySDR and the SoapyBladeRF plugin; the upstream `crafter` dependency also
requires the platform's libpcap development/link library. Follow the
[native setup guide](../../../docs/SETUP.md) for the receive stack. On Fedora,
install the additional Wi-Fi decoder link dependency with:

```sh
sudo dnf install libpcap-devel
```

Then build:

```sh
cargo build --locked --release -p wifi-monitor -p bladerf-lab
target/release/wifi-monitor doctor
target/release/wifi-monitor capture --help
```

The default `decode` Cargo feature selects `crafter` from the patched
[source submodule][crafter] through a repository-relative path, with resolution
recorded in the root lockfile. The tracked [endpoint patch][crafter-patch]
exposes closed OFDM rejection and acquisition accounting and retains bounded
lower-priority reference-relaxed hypotheses. A build with
`--no-default-features` omits IQ decoding and its libpcap dependency while
retaining PCAP inspection, inventory and live-engine orchestration. Capture,
survey and watch need the `decode` feature when using the default `crafter`
engine.

`doctor` checks receiver availability and any required source patch. It does not
open the radio, attest how an arbitrary binary was built, or establish reception,
sample continuity or decoding performance.
The optional live engine has an additional build step below.

## Capture and inspect

Start with a short capture on one channel, selecting the connector carrying the
receive feed:

```sh
target/release/wifi-monitor capture --channel 1 --band 2g --antenna rx1 --gain 40 --duration 1 --output captures/wifi-session
```

The default engine is `crafter`. Acquisition records SC16 Q11 IQ through
`bladerf-lab` at **20,000,000 complex samples/s**, stops the receiver, then runs
the Rust decoder in a child process. The decoder converts the samples to its
signed CS8 representation with explicit scaling. Source IQ remains available
for later replay even when decoding fails or reaches its timeout.
Before replay, the wrapper checks the native sidecar's encoding, sample count,
receive settings and corresponding IQ file length against the request.

| Option | Default and meaning |
| --- | --- |
| `--engine` | `crafter`; choose `futuresdr` for the optional live legacy receiver |
| `--band` | `2g`; `5g` selects the 5 GHz channel namespace |
| `--channel` | Channel 1 for `2g`, 36 for `5g`; one channel is observed per capture |
| `--antenna` | `rx1`; select `rx1` or `rx2` to match the physical receive connector |
| `--gain` | 40 dB manual receiver gain; this is not measured signal strength |
| `--duration` | 1 second for `crafter`, 15 seconds for `futuresdr`; maximum 10 seconds of IQ or 300 seconds live |
| `--decode-timeout` | 120 seconds of wall time for the offline decoder |
| `--iq-limit-mib` | 256 MiB acquisition budget |

SC16 Q11 uses four bytes per complex sample: one second at 20 MSps is
80,000,000 bytes, about 76.3 MiB. A longer duration needs a matching IQ budget.
Acquisition startup, shutdown, replay and inspection add to elapsed time.
Increasing the buffer or duration does not establish continuous real-time
decoding. Check `capture --help` for the implemented ranges and engine-specific
options.

Use a fresh output directory for each operation. Inspect an existing capture
without opening the radio:

```sh
target/release/wifi-monitor inspect captures/wifi-session/candidates.pcap --output artifacts/wifi-inspect
```

The inspector accepts classic PCAP with raw IEEE 802.11 or radiotap records.
PCAPNG is unsupported. FCS presence must be described by the capture metadata;
unknown or absent FCS is distinct from an independently validated frame.

## Survey selected channels

`survey` records one bounded snapshot per selected channel, sequentially, and
builds an inventory from the recovered frames:

```sh
target/release/wifi-monitor survey --channels 1,6,11 --band 2g --antenna rx1 --gain 40 --duration 1 --output captures/wifi-survey
```

Capture settings have the same meanings and engine-specific limits as for
`capture`. Channel lists contain at most 64 unique channels and are validated
before opening a radio. The default list is `1,6,11` for `2g` and `36,40,44,48`
for `5g`. Choose channels and an RF path appropriate for the experiment.

A survey does not observe all channels simultaneously. Receiver startup,
offline decoding, inspection and movement between channels leave unobserved
periods. The run records wall time separately from the bounded IQ sample
duration; neither frame counts nor the capture schedule establish channel
utilization or absence of other traffic.

## Inventory saved captures

`inventory` revalidates one or more PCAP files without opening a radio. It does
not trust previously generated packet JSONL as evidence:

```sh
target/release/wifi-monitor inventory captures/wifi-session/validated.pcap --output artifacts/wifi-inventory
```

The JSON and Markdown reports summarize observed addresses and their frame
roles, advertised networks, SSID bytes, security/capability information, frame
counts and first/last validated observations within each source. Network rows
retain their evidence: an infrastructure advertisement, an IBSS advertisement
or an explicit BSSID reference in a data frame. A BSSID reference alone does not
confirm an access point. An observed address is not a permanent physical device
identity. Advertised capabilities describe the transmitted information elements,
not a successful decode of every advertised PHY.
Security advertisements retain privacy capability and malformed-element state;
an absent RSN/WPA element alone does not establish an open network.

Saved PCAP does not necessarily describe its original tuning or clock. The
default `--clock unknown` keeps first/last timestamps scoped to each input
source. Explicit choices are `sample-relative`,
`host-before-rx-enable-plus-sample-offset` and `host-udp-receive`; choose a basis
only when the source's provenance establishes it. Do not combine independent
sample-relative recordings into a historical wall-clock timeline.
Optional `--frequency-mhz` annotates the supplied input context; it does not
retune hardware or establish the original RF settings. Advertised channel
information remains separate from acquisition context.

`--max-records` defaults to 1,000,000 and `--max-input-mib` to 256. These bounds
apply to each input. Exceeding input or inventory-state limits fails that input
without adding partial observations; completed earlier inputs remain in the
report. Duplicate canonical input paths are rejected. Cancellation is checked
between inputs, so an input already being read can finish before interruption
is handled.

## Repeated fixed-channel monitoring

`watch` repeats a bounded capture on one channel. The default is ten cycles:

```sh
target/release/wifi-monitor watch --channel 6 --antenna rx1 --gain 40 --duration 1 --cycles 10 --max-consecutive-failures 3 --keep-sessions 3 --retained-limit-mib 512 --output captures/wifi-watch
```

Choose 1 through 1000 cycles. A cycle is one attempted visit; a failed visit is
recorded and consumes its cycle rather than being silently replaced. The default
channel is 1 for `2g` and 36 for `5g`. Each cycle uses the same capture,
validation and inventory path as a survey. It is a sequence of snapshots;
offline replay and the next acquisition leave observation gaps.

Watch continues with a fresh attempt after an isolated capture or inventory
failure. A successful attempt resets the failure streak. By default it stops
after three consecutive incomplete attempts; choose 1 through 100 with
`--max-consecutive-failures`, where 1 restores stop-on-first-failure behavior.
Interruption still stops immediately; retained-byte enforcement stops after an
oversized active capture or before another capture starts. Continuation does not
repair the failed RF window or establish continuity.

Watch retains at most three successful session directories by default.
It prunes only completed sessions created by this run after their observations
have entered the inventory. The aggregate inventory survives removal of an old
session's raw IQ, packet files and logs. Failed or interrupted sessions remain
available for diagnosis. Older successful sessions can be pruned before the next
capture to reserve space for its estimated IQ size. Failed sessions are not
pruned, so their accumulated evidence can cause the retained-byte threshold to
stop a run before its failure-streak limit.

Both survey and watch default to a 512 MiB retained-session limit. Storage is
checked between captures; this is not a filesystem quota. A single session can
exceed that threshold before the run stops. `--iq-limit-mib` separately bounds
each bounded-IQ acquisition. Retention cannot remove unrelated directories or
make a failed session appear to have completed.

Run outputs are written below a fresh private directory. `run.json` records
configuration and overall progress, and `attempts.jsonl` records capture
attempts, timing, gaps and completion status. `inventory.json` and
`inventory.md` hold accumulated observations. Individual capture files are
under `sessions/000001-ch006/` and subsequent numbered session directories.
An inventory source path can therefore reference a session already pruned by
retention; the recorded observation remains in the aggregate.
Reports contain captured network details and should stay local, alongside the
source captures.

## Interpret run status and timing

`run.json` separates three completion states:

| Field | Meaning |
| --- | --- |
| `finished` | The supervisor reached its final reporting step |
| `plan_completed` | Every planned attempt ran without interruption or a stop condition |
| `successful` | The run finished without a stop condition and every capture and inventory step succeeded |

A completed survey can contain failed attempts. Valid frames from a degraded
capture can still enter the inventory. Check `run.json`, `attempts.jsonl` and
the preserved session diagnostics for capture health; standalone PCAP
validation establishes frame integrity, not successful or complete acquisition.
An isolated watch failure followed by recovery can leave `plan_completed=true`
and `successful=false`; the command returns nonzero so automation cannot mistake
the degraded run for a clean one. `max_consecutive_failures`,
`consecutive_failures_at_end` and `maximum_consecutive_failures_observed` record
the configured policy and observed streaks. A stop at the bound uses
`stop_reason=consecutive_failure_limit_reached`.

`recorded_sample_seconds` comes from checked native IQ metadata. It is `null`
when any contributing attempt lacks that information. The separate
`known_recorded_sample_seconds` field sums only known contributions. Live
receiver wall time does not establish its active sample duration and remains
unknown for this accounting.

Per-channel `wall_seconds_without_recorded_samples_estimate` is a bookkeeping
difference between elapsed wall time and known recorded sample time. It is not
a measurement of actual RF blind time. `between_visit_wall_gaps_seconds` covers
the wall time between attempts on that channel; it excludes unobserved periods
inside an attempt, including startup and offline decoding. Neither value proves
sample continuity or lossless channel coverage.

First/last packet timestamps remain scoped to their input and stated clock
basis. Reports do not merge independent source clocks into a global timeline.
Each inventory/run snapshot is replaced atomically on its own; the set of
JSON, Markdown and journal files is not a multi-file transaction.

## Outputs and failure handling

| File | Purpose |
| --- | --- |
| `request.json` | Requested acquisition and decoder settings |
| `capture.json` | Acquisition or live-receiver completion and timestamp information |
| `iq.sc16q11` | Source IQ retained by the default bounded capture workflow |
| `iq.sc16q11.json` | Native capture metadata and actual receive settings |
| `candidates.pcap` | Frames recovered by the selected PHY decoder, retaining their received FCS |
| `candidates.decode.json` | Offline sample counts, decoder limits, PHY counters, exact OFDM rejection accounting and timing basis |
| `validated.pcap` | Frames accepted by the independent FCS and frame-layout checks |
| `packets.jsonl` | Per-record classification, decoded fields and warnings |
| `summary.json` | Aggregate validation and observable-content counts |
| `decoder.log` | Decoder diagnostics |
| `acquisition.log` | Native IQ acquisition diagnostics for the default engine |

Inspection of a saved PCAP produces the analysis files. IQ replay additionally
produces candidates, its decode report and decoder log. Live FutureSDR capture
produces packet candidates and session records, without a continuous IQ file.

### Replay event sidecars

Use `decode-iq --frame-spans` to also save `candidates.frame-spans.json`.
Its schema 1 lists decoder-reported, half-open input sample spans in candidate
PCAP record order. Members of one aggregate share the PHY interval.
Use `decode-iq --ht-signal-events` to save `candidates.ht-signal-events.json`.
It lists sample-relative preamble positions and fields of integrity-checked
HT headers in diagnostic order. The decoder and the 4,096-event report cap can
omit events. The sidecar reports saturation. Check frame FCS independently.
Use `decode-iq --he-su-signal-events` to save `candidates.he-su-signal-events.json`.
It records validated HE20 single-user headers and their sample positions under
the same bounds. Payload decoding coverage is listed below.
Use `decode-iq --ampdu-error-events` to save `candidates.ampdu-error-events.json`.
It records the preamble position and the decoder's delimiter, FCS, truncation
and size-error counts for each aggregate diagnostic. It uses the same bounds.

### Analysis reports

Analysis files are available after successful inspection; an earlier acquisition
or decoder failure can leave only a subset of the session files.
`candidates.decode.json` is schema version 4 with operation
`offline_wifi_iq_decode`; `summary.json` remains schema version 4 with operation
`inspect_wifi_packets`. Consumers can reject incompatible producer reports
before interpreting their counts. In decode schema 4, `ofdm.rejections` is a
closed partition of `ofdm.rejected_frames` with the fields
`signal_or_probe_rejected`, `greenfield_header_unsupported`,
`greenfield_probe_unrecognized`, `mixed_header_unsupported`,
`legacy_fallback_unreservable`, `data_decode_failed`,
`pending_capacity_rejected` and `strong_long_training_not_found`. The last
counter is a strong-path timeout and may coexist with successful
weak-short-period or reference-relaxed recovery of the same waveform. Their
checked sum exactly equals `ofdm.rejected_frames`; a mismatch or integer
overflow fails publication.
They count terminal decoder-hypothesis branches rather than unique physical
frames. Invalid FCS, truncation and output drops retain their separate OFDM
counters, while the DSSS counter shape is unchanged. The bounded diagnostic
counts remain overlapping lower bounds and are not used to reconstruct this
partition.

`ofdm.data_failures` partitions `data_decode_failed` into `legacy`, `ht_bcc`
and `ht_ldpc`. Its checked sum must equal the parent counter; mismatch or
overflow fails before the staged PCAP is installed. These fields identify the
decoder-selected data path after a supported header, not a proven air waveform,
unique missed frame or occupancy event.

Schema 4 retains the closed `ofdm.strong_long_training` subpartition with
`strong_short_periodicity_timeout`, `strong_long_repeat_not_found`,
`strong_long_reference_not_found` and
`strong_long_coordinates_unavailable`. Its checked sum must equal
`ofdm.rejections.strong_long_training_not_found`; a mismatch or overflow also
fails publication before the staged PCAP is installed. Each strong candidate is
classified by the furthest eligible acquisition gate reached before expiry.
The first field means that no eligible long-training evaluation occurred, not
that short periodicity was proved throughout the interval. The coordinate field
is limited to source-position formation after reference acceptance and is not a
generic missing-history bucket. A reference-relaxed lower-priority hypothesis
does not pass the unchanged strong known-reference gate, so its recovery or a
later downstream rejection can coexist with the original strong candidate's
`strong_long_reference_not_found` timeout. These are still decoder-hypothesis
events rather than missed frames, unique transmissions or occupancy evidence.

Existing outputs are preserved. Decoder errors and timeouts return an error and
retain available evidence; a successful empty capture means that no frames were
recovered by that configuration. It does not establish absence of Wi-Fi traffic.
At a deadline or interrupt, the supervisor signals its child, allows up to five
seconds for shutdown, then kills and reaps a stalled process.
Recognized receive/USB errors, including FutureSDR Seify errors and overruns,
make capture fail after preserving available decoded packets. The diagnostic
scan reads at most 16 MiB of each checked log; a longer log also makes capture
fail because diagnostics were not fully examined. Absence of a recognized
marker does not prove sample continuity.
Ctrl-C during live capture records `interrupted: true`, preserves and inspects
available packets, and returns failure because the requested window was shortened.
Offline candidates are published only after decoding completes; terminating a
worker can leave a private `.wifi-decode-*.partial` staging file.
IQ recording alone does not establish lossless sample continuity. Keep actual
captures, metadata and logs local; the workspace's `captures/` and `artifacts/`
directories are ignored by Git.

## Replay saved IQ

Replay does not open a radio. Supply the sample format and tuning explicitly;
raw IQ contains neither a header nor an inherent center frequency:

```sh
target/release/wifi-monitor decode-iq captures/wifi-session/iq.sc16q11 --format sc16q11 --frequency-mhz 2412 --max-samples 20000000 --decode-timeout 120 --output artifacts/wifi-decoded
```

`--format cs8` accepts headerless, signed, interleaved 8-bit I/Q, in
`I0,Q0,I1,Q1,...` order. `sc16q11` accepts the bladeRF capture representation.
Both are decoded as a 20 MSps stream. Do not relabel a recording taken at another
sample rate: resampling and its metadata must be handled explicitly first.
`--max-samples` bounds the replay work; `--decode-timeout` separately bounds CPU
wall time. Retaining IQ allows another decoder or later version to revisit the
same received waveform.

Use `--double-q11` to double SC16 Q11 components before translation and CS8
conversion. The decode report counts components saturated by the fixed gain.

When the recording was tuned off channel, use `--capture-frequency-hz` for
the saved tuning and `--frequency-mhz` for the desired Wi-Fi channel center.
For saved-IQ replay, the centers may differ by up to 5 MHz. Shifts beyond
1 MHz filter the spectrum that would wrap across Nyquist. As the offset grows,
less of the target channel lies inside the capture. The report records the
shift, saturation count, and applied filter. Frame sample offsets stay on the
original capture grid.

To check the software path without a radio, replay an independent synthetic
waveform from the pinned source dependency. These examples cover legacy OFDM
and HT20 with BCC coding:

```sh
target/release/wifi-monitor decode-iq repos/libcrafter/crafter/tests/fixtures/iq/ofdm-6-clean.cs8 --format cs8 --frequency-mhz 2412 --max-samples 10000 --output artifacts/wifi-ofdm-fixture
target/release/wifi-monitor decode-iq repos/libcrafter/crafter/tests/fixtures/iq/ht-bcc-0-gi800-len100-clean.cs8 --format cs8 --frequency-mhz 2412 --max-samples 10000 --output artifacts/wifi-ht20-fixture
```

Each fixture describes one expected frame. Its original PSDU bytes and sample
SHA-256 are recorded in the dependency's [fixture inventories][iq-fixtures],
independently of the receive decoder. Software replay does not establish
over-the-air reception, sensitivity or sustained hardware throughput.

### HE20 single-user payload replay

Build the `radio_receive` example in a separate [pinned libcrafter checkout][he-receiver].
Use that executable with `decode-he-iq` on saved 20 MSps IQ.

```sh
cargo +1.99.1 build --locked --release --manifest-path /path/to/libcrafter/Cargo.toml -p crafter --features radio --example radio_receive
target/release/wifi-monitor decode-he-iq captures/wifi-session/iq.sc16q11 --decoder /path/to/libcrafter/target/release/examples/radio_receive --frequency-mhz 2437 --output artifacts/wifi-he-decoded
```

The command publishes HE20 single-user frames with independently checked FCS,
half-open sample spans, and a `he-replay.json` receipt after inspection succeeds.
It uses the same IQ format, gain, recentering, sample and time limits as `decode-iq`.
Receiver records and diagnostics are capped at 64 MiB and 2 MiB, respectively.

For VHT20 single-user replay, use `decode-vht-iq` with the same decoder and
arguments. It accepts the pinned receiver's single-data-stream BCC and LDPC
frames, including STBC. Each published MPDU must match its VHT-SIG-A record
and pass an independent FCS check. The receipt is `vht-replay.json`.

## What can be observed

The useful fields depend on which frames were transmitted, successfully
demodulated and independently validated:

| Traffic or field | Available information | Interpretation boundary |
| --- | --- | --- |
| MAC headers | Frame type/subtype, transmitted addresses, address roles, sequence/fragment values and retry/protected flags | Addresses do not reliably identify a person or a permanent device |
| Beacons and probe exchanges | SSID bytes when present, BSSID, beacon interval, capabilities, rates and channel information elements | Empty/hidden SSIDs and optional fields remain explicit |
| RSN/WPA information elements | Advertised group/pairwise ciphers, authentication suites and applicable management-frame-protection flags | Advertisements describe capabilities; they do not supply session keys |
| HT/VHT/HE/EHT capability elements | Preserved raw information-element data and identification | Advertising a newer PHY does not prove that its data waveform was decoded |
| Unprotected data | Preserved payload and LLC/SNAP fields where recognized; EAPOL fields when present | This is not a complete application-protocol or handshake-state decoder |
| Protected data | Visible MAC fields and preserved ciphertext bytes | Payload decryption is not implemented |

SSID bytes need not be UTF-8. The JSON retains raw information-element bytes and
reports text only when valid. Unknown/vendor-specific elements remain available
without inventing an interpretation.

The schema-4 packet summary counts only EAPOL carried by independently FCS-valid
frames whose unprotected, unfragmented MSDU is eligible for LLC/SNAP parsing. It
reports header/body completeness, packet-type totals, and overlapping Key
Information flag counts for complete EAPOL-Key prefixes. Those aggregates contain
no addresses, identifiers, EAPOL bodies, key material or other payload bytes.
The private `packets.jsonl` output still preserves the decoded EAPOL fields and
body bytes needed to investigate a particular capture. These structural counts
do not establish a complete handshake, identify a participant, authenticate a
sender, prove replay freshness or show that decryption keys are available.

The same summary has a closed `block_ack` aggregate for independently FCS-valid
Block Ack control frames. It partitions every such frame among a truncated BA
Control field, a complete classic 64-bit compressed single-TID body, a truncated or
length-mismatched body of that recognized form, and another BA variant. The
recognized form requires compressed BA type 2, clear reserved BA Control bits,
a zero low nibble in Starting Sequence Control, and exactly 12 bytes following
the Frame Control, Duration, RA and TA fields. The received FCS is excluded from
that length. A nonzero low nibble can signal a newer compressed-bitmap length,
so it remains an other variant instead of being guessed as the classic 8-byte
bitmap.

For complete recognized frames, the aggregate reports fixed TID 0 through 15
counts, the total number of set acknowledgement-bitmap bits, and zero, nonzero
and full-bitmap frame counts. Zero plus nonzero equals the complete recognized
frame count; full-bitmap frames are a subset of nonzero frames, not a third
partition. The aggregate deliberately omits addresses, starting sequence
values, raw bitmaps and payload bytes. The private `packets.jsonl` record exposes
the recognized TID, acknowledgement policy, starting sequence and bitmap
population alongside its existing raw frame payload. These are acknowledgement
structures observed in captured frames; they do not identify a participant,
authenticate the sender, establish traffic completeness or recover acknowledged
payloads.

The same summary retains a closed advertisement aggregate for independently
FCS-valid, unprotected and unfragmented beacons and probe responses whose fixed
fields are complete. Counts describe frames, not distinct networks: duplicate
information elements within one frame count once. It distinguishes complete and
truncated information-element layouts, records per-frame presence of HT, VHT,
HE and EHT capability/operation elements and unknown extension IDs, and reports
RSN, WPA and management-frame-protection structure. Parsed and malformed
security elements can both be true for a frame containing inconsistent duplicate
elements. The aggregate contains no SSID, address, OUI, cipher/authentication
suite, channel or raw element bytes.

These counters are advertisement evidence, not airtime, occupancy or unique
network counts. A capability element does not show that the corresponding PHY
waveform was decoded. RSN, WPA and protection-capability advertisements neither
authenticate their sender nor decrypt traffic or establish that a protected
exchange occurred.

An FCS checks consistency of the captured MAC bytes. It neither authenticates
the sender nor decrypts content. Decryption needs the appropriate keys and
session state; a WPA3 password plus a captured handshake does not by itself
provide the per-connection key. See the
[Wireshark WLAN decryption guide][decryption] for those distinctions.

## Decoder coverage

The application reuses pinned upstream Rust DSP and independently inspects the
resulting packets. These are implementation capabilities, not a claim of complete
recovery or measured sensitivity for every listed waveform.

Both upstream PHYs filter failed FCS checks before publishing frames. The
candidate PCAP therefore contains recovered packet output, rather than every
RF detection or corrupted transmission. Independent inspection verifies the
retained bytes again and classifies unsupported frame layouts separately.

| Engine | Receive implementation | Limits |
| --- | --- | --- |
| `crafter` (default) | Legacy OFDM at 6/9/12/18/24/36/48/54 Mbps; DSSS/CCK at 1/2/5.5/11 Mbps; HT20 MCS 0–7 | Bounded acquisition followed by replay; one data stream; no HT40, VHT, HE or EHT payload decoding |
| `futuresdr` | Live legacy OFDM at 6/9/12/18/24/36/48/54 Mbps | Maximum PSDU 1528 bytes; no DSSS/CCK or HT/VHT/HE/EHT decoding; host/USB throughput can limit recovery |

The default decoder supports long-preamble DSSS/CCK at all four rates and short
preambles at 2/5.5/11 Mbps. At 1 and 2 Mbps, it can use a Barker payload candidate
trained from the PLCP header if the direct decode fails FCS.
HT20 includes BCC/LDPC, mixed and greenfield formats,
supported guard intervals, A-MPDU members, single-data-stream STBC and extension
training. Unsupported combinations remain outside its coverage.

The OFDM-only engine can miss 2.4 GHz beacons sent at 1 Mbps. AP basic-rate
configuration determines the management-frame rate; the
[Meraki basic-rate documentation][basic-rates] gives one common example. A live
OFDM capture containing data but no SSID is therefore possible. The default
DSSS/CCK path addresses that PHY gap, while reception quality and observation
time still affect what is recovered.

The source artifacts are [libcrafter][crafter] and [FutureSDR WLAN][futuresdr].
Nuand's separate [bladeRF WiPHY][wiphy] modem uses different FPGA gateware and
MAC integration; this project does not require it.

## Timing, bandwidth and packet metadata

The channel selects an RF center; 20 MSps selects the decoder's complex sample
clock. Neither is a promise of full-band monitoring or complete reception.
Channels outside the receiver's instantaneous window and traffic sent while it
is stopped are unobserved. Antenna, filtering, distribution, USB and host
processing can further limit useful coverage.
Native IQ acquisition reserves about 8 MiB of receive buffering to absorb brief
host stalls; persistent overruns still require addressing USB or processing load.

Packet captures use radiotap and preserve the actual received FCS. The radiotap
[Flags definition][radiotap-flags] distinguishes an included FCS from a failed
check. Missing rate, channel or signal-power metadata is left unknown rather
than filled with plausible defaults. Session settings and decoded information
elements retain their separate meanings.

Host packet-delivery timestamps are not hardware RF timestamps. The bounded
capture uses the native sidecar's host start time, recorded before RX enable,
plus sample offsets. The activation delay and untimestamped receive stream
prevent precise RF timing. Standalone IQ replay starts at timestamp zero and
encodes sample-relative offsets, not historical wall time. Receiver gain,
correlation and frequency-offset estimates are not calibrated received power
in dBm.

## Build the optional live engine

FutureSDR's pinned WLAN example requires nightly. Its
[separate toolchain selection](../../../config/wifi-decoder-toolchain.toml),
[dependency lock](../../../config/wifi-decoder.Cargo.lock) and
[receive patch](../../../patches/futuresdr-wlan-passive-rx.patch) keep that requirement
outside the stable workspace.

Install the selected toolchain, apply the patch once, and build only the receiver:

```sh
rustup toolchain install nightly-2026-10-27 --profile minimal
git -C repos/FutureSDR apply ../../patches/futuresdr-wlan-passive-rx.patch
cp config/wifi-decoder.Cargo.lock repos/FutureSDR/examples/wlan/Cargo.lock
cargo +nightly-2026-10-27 build --locked --manifest-path repos/FutureSDR/examples/wlan/Cargo.toml --release --bin rx --no-default-features --features soapy --target-dir build/futuresdr --jobs 2
target/release/wifi-monitor doctor --engine futuresdr
```

Check whether the patch is already applied before applying it again:

```sh
git -C repos/FutureSDR apply --reverse --check ../../patches/futuresdr-wlan-passive-rx.patch
```

The executable is `build/futuresdr/release/rx`. It uses the installed SoapySDR
library and bladeRF plugin. The patch retains the received FCS, fixes the
20 MSps receive profile, selects the requested receive connector and disables
loopback and its bias tee on the same device handle. The wrapper collects its
localhost UDP frames into PCAP and runs independent inspection.

Select this engine explicitly:

```sh
target/release/wifi-monitor capture --engine futuresdr --channel 1 --band 2g --antenna rx1 --gain 40 --duration 15 --output captures/wifi-live
```

Both upstream dependencies retain their own licenses and source conventions.
Builds and source checks establish software readiness; the selected receive
path still needs RF validation for the intended experiment.

[crafter]: https://github.com/pellegre/libcrafter/tree/f9ef000bc402eebc676f6b4708395aa8ac169450
[crafter-patch]: ../../../patches/libcrafter-ofdm-observability-recovery.patch
[crafter-data-failure-patch]: ../../../patches/libcrafter-ofdm-data-failure-accounting.patch
[crafter-format-patch]: ../../../patches/libcrafter-rustfmt-boundary.patch
[he-receiver]: https://github.com/pellegre/libcrafter/tree/00f1fed7ed8984cfc89360fd7ffbe19ba6c36a5b
[futuresdr]: https://github.com/FutureSDR/FutureSDR/tree/42832f5473ec19ce641c58a4792befdc57945ff5/examples/wlan
[wiphy]: https://www.nuand.com/bladerf-wiphy/
[basic-rates]: https://documentation.meraki.com/Wireless/Operate_and_Maintain/User_Guides/Radio_Settings/Minimum_Bitrate_Control
[decryption]: https://www.wireshark.org/docs/wsug_html_chunked/Ch80211Keys.html
[radiotap-flags]: https://radiotap.org/fields/Flags.html
[iq-fixtures]: https://github.com/pellegre/libcrafter/tree/f9ef000bc402eebc676f6b4708395aa8ac169450/crafter/tests/fixtures/iq
