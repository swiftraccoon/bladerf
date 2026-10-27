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

The output separates evidence from interpretation:

| File | Purpose |
| --- | --- |
| `candidates.pcap` | Original decoder packet candidates, including unverified or corrupt records |
| `validated.pcap` | BLE packets accepted by the independent CRC validation path |
| `packets.jsonl` | Packet-by-packet classifications and decoded fields |
| `summary.json` | Counts grouped by classification and observable packet content |
| `request.json` | Requested capture settings and decoder arguments |
| `capture.json` | Stop reason, decoder exit status and elapsed session time |
| `decoder.log` | Decoder diagnostics, including processing or sample-drop indications |

`request.json`, `capture.json` and `decoder.log` describe live capture sessions.
Packet inspection produces the analysis files from an existing candidate PCAP.
Default output directories are under the workspace's `captures/bluetooth/` and
`artifacts/bluetooth-inspect/`; explicit relative output paths also resolve from
the workspace root. Keep actual session outputs local; these directories are
ignored by Git.

The duration includes decoder initialization. The wrapper interrupts the decoder
at the duration or recording limit, allows five seconds for shutdown, then kills
and reaps a stalled process. `--output-limit-mib` limits the combined candidate,
log and IQ files; its default is 2048 MiB. It does not limit the analysis files
written after capture. Recording limits are soft because polling and native
buffering can overshoot them. Existing output directories are
preserved. Decoder failure, unexpected early exit, reported receive/USB errors
or forced termination returns an error while retaining
partial files for inspection; an empty capture does not establish that no
Bluetooth devices were transmitting. `capture.json` includes bounded log-prefix
diagnostics. These checks identify known error messages and nonzero overflow or
load-shedding counters; absence of a recognized message does not prove continuity.

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
connected packets use initialization learned from an earlier CRC-valid connection
request in the same input stream. Bad CRCs, unsupported records and data requiring
unknown CRC state remain
distinct from validated packets. A valid CRC checks packet consistency; it does
not authenticate a sender or decrypt a payload.

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

## Build the native engines

The Rust project uses the workspace [toolchain](../../rust-toolchain.toml).
Initialize the upstream checkouts first:

```sh
git submodule update --init --recursive
```

Blue Dragon uses libbladeRF and `protoc`; the bladeRF-only feature selection
avoids its default USRP backend. Build only after applying its compatibility
patch:

```sh
git -C repos/blue-dragon apply ../../patches/blue-dragon-bladerf-rx-setup.patch
cp config/blue-dragon.Cargo.lock repos/blue-dragon/Cargo.lock
cargo build --release --locked --manifest-path repos/blue-dragon/Cargo.toml -p blue-dragon --no-default-features --features bladerf --target-dir build/blue-dragon --jobs 2
```

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

The executable is `build/ice9-bluetooth/ice9-bluetooth`. Both patches explicitly
disable the selected receive path's bias tee. To check whether a patch is already
applied, use `git apply --reverse --check` with the same paths before applying it
again. `doctor` checks files and source patch state; it does not attest how an
arbitrary decoder executable was built.

Both upstream projects retain their own licenses and source conventions. See the
repository
[setup guide](../../docs/SETUP.md) and [contribution guide](../../CONTRIBUTING.md)
for native prerequisites and project standards.

[le-phy]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/radio-physical-layer-specification.html
[ll]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/link-layer-specification.html
[ad-types]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/CSS_v14/out/en/core-supplementary-features/data-types-specification.html
[le-security]: https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/low-energy-link-layer-security.html
[ice9]: https://github.com/mikeryan/ice9-bluetooth-sniffer/tree/788c5bce998a2d15c6b26a7d729ad81a5d28a869
[blue-dragon]: https://github.com/alphafox02/blue-dragon/tree/7bc9ec6b9228852b752f7309e46ecb186b87b2eb
[oversample]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L1210-L1227
