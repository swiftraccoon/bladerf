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
```

The current host target is Linux; orchestration uses Unix process signals.
The default project uses the workspace's stable
[Rust toolchain](../../rust-toolchain.toml). Native capture requires libbladeRF,
SoapySDR and the SoapyBladeRF plugin; the upstream `crafter` dependency also
requires the platform's libpcap development/link library. Follow the
[native setup guide](../../docs/SETUP.md) for the receive stack. On Fedora,
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

The default `decode` Cargo feature selects `crafter` at the exact Git revision
matching the [source submodule][crafter], with resolution recorded in the root
lockfile. A build with `--no-default-features` omits IQ decoding and its libpcap
dependency while retaining PCAP inspection, inventory and live-engine
orchestration. Capture, survey and watch need the `decode` feature when using
the default `crafter` engine.

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
target/release/wifi-monitor watch --channel 6 --antenna rx1 --gain 40 --duration 1 --cycles 10 --keep-sessions 3 --retained-limit-mib 512 --output captures/wifi-watch
```

Choose 1 through 1000 cycles. The default channel is 1 for `2g` and 36 for `5g`.
Each cycle uses the same capture, validation and inventory path as a survey.
It is a sequence of snapshots; offline replay and the next acquisition leave
observation gaps.

Watch retains at most three successful session directories by default.
It prunes only completed sessions created by this run after their observations
have entered the inventory. The aggregate inventory survives removal of an old
session's raw IQ, packet files and logs. Failed or interrupted sessions remain
available for diagnosis. Older successful sessions can be pruned before the next
capture to reserve space for its estimated IQ size.

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
| `candidates.decode.json` | Offline sample counts, decoder limits, PHY counters and timing basis |
| `validated.pcap` | Frames accepted by the independent FCS and frame-layout checks |
| `packets.jsonl` | Per-record classification, decoded fields and warnings |
| `summary.json` | Aggregate validation and observable-content counts |
| `decoder.log` | Decoder diagnostics |
| `acquisition.log` | Native IQ acquisition diagnostics for the default engine |

Inspection of a saved PCAP produces the analysis files. IQ replay additionally
produces candidates, its decode report and decoder log. Live FutureSDR capture
produces packet candidates and session records, without a continuous IQ file.
Analysis files are available after successful inspection; an earlier acquisition
or decoder failure can leave only a subset of the session files.

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
| `crafter` (default) | Legacy OFDM at 6/9/12/18/24/36/48/54 Mbps; DSSS/CCK at 1/2/5.5/11 Mbps; HT20 MCS 0–7 | Bounded acquisition followed by replay; one data stream; no HT40, VHT, HE or EHT decoding |
| `futuresdr` | Live legacy OFDM at 6/9/12/18/24/36/48/54 Mbps | Maximum PSDU 1528 bytes; no DSSS/CCK or HT/VHT/HE/EHT decoding; host/USB throughput can limit recovery |

The default decoder supports long-preamble DSSS/CCK at all four rates and short
preambles at 2/5.5/11 Mbps. HT20 includes BCC/LDPC, mixed and greenfield formats,
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
[separate toolchain selection](../../config/wifi-decoder-toolchain.toml),
[dependency lock](../../config/wifi-decoder.Cargo.lock) and
[receive patch](../../patches/futuresdr-wlan-passive-rx.patch) keep that requirement
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
[futuresdr]: https://github.com/FutureSDR/FutureSDR/tree/42832f5473ec19ce641c58a4792befdc57945ff5/examples/wlan
[wiphy]: https://www.nuand.com/bladerf-wiphy/
[basic-rates]: https://documentation.meraki.com/Wireless/Operate_and_Maintain/User_Guides/Radio_Settings/Minimum_Bitrate_Control
[decryption]: https://www.wireshark.org/docs/wsug_html_chunked/Ch80211Keys.html
[radiotap-flags]: https://radiotap.org/fields/Flags.html
[iq-fixtures]: https://github.com/pellegre/libcrafter/tree/f9ef000bc402eebc676f6b4708395aa8ac169450/crafter/tests/fixtures/iq
