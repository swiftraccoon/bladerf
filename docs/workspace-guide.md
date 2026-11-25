# bladeRF micro xA9 lab

A Rust learning workspace for the **bladeRF 2.0 micro xA9**. Start with the
[handbook](handbook/index.md), [learning guide](LEARNING.md), and
[setup guide](SETUP.md). The [build instructions](BUILDING.md)
produce a searchable local handbook. See [CONTRIBUTING.md](../CONTRIBUTING.md)
for development setup, checks and contribution conventions.

## Workspace layout

The root `Cargo.toml` is a virtual workspace. Its default member,
[`tools/bladerf-lab`](../tools/bladerf-lab), contains the host CLI, tests,
and GNU Radio flowgraphs.
[`tools/bladerf-docs`](../tools/bladerf-docs) validates the source-qualified fact
catalog and reproduces its handbook reference without radio dependencies.

The independent [`bladerf-profile2-ipc`](../crates/bladerf-profile2-ipc),
[`bladerf-profile2-loader-ipc`](../crates/bladerf-profile2-loader-ipc),
[`bladerf-profile2-recovery-ipc`](../crates/bladerf-profile2-recovery-ipc), and
[`bladerf-profile2-session-ipc`](../crates/bladerf-profile2-session-ipc) crates,
together with the
[`bladerf-profile2-adapter`](../tools/bladerf-profile2-adapter) CPU fixtures,
qualify bounded register, image-transition, and complete-session process
protocols for RF-observatory's profile-2 work. The adapter also has an x86-64
Linux-only, sealed-snapshot native-library preflight that resolves the future
ABI and calls only the library version and explicitly requested neutral-open
marker. The model checkers invoke no device API, must be given a separately
trusted pinned build, and are not hardware backends. The
toolchain, `Cargo.lock`, upstream source checkouts, patches, and image hashes stay
at the repository root. Cargo build output remains under root `target/`.
See the [tool's own guide](../tools/bladerf-lab/README.md) for its commands and
component documentation.

Cargo commands below name `-p bladerf-lab` explicitly; omitting the selector
uses that default member.

## Monitoring projects

The [Bluetooth monitor](../projects/bluetooth-monitor) provides passive BLE and
Classic Bluetooth capture, Wireshark-compatible files, decoded JSON records,
and independent BLE CRC validation. Its guide describes decoder setup, receive
connector selection, coverage limits and offline inspection.

The [Wi-Fi monitor](../projects/wifi-monitor) records bounded IQ captures and
decodes legacy OFDM, DSSS/CCK and single-stream HT20. It preserves source IQ,
writes Wireshark-compatible frames and JSON records, and independently checks
the received FCS. An optional live engine covers legacy OFDM. Its guide explains
the two workflows, decoder limits and replaying saved IQ.
Channel surveys, saved-PCAP inventories and repeated captures aggregate
observations while recording gaps and managing local retention.

The [RF observatory](../projects/rf-observatory) adds wideband recording,
four-channel FM reference reception and an FPGA NCO/CIC channel bank. Its guide
separates software replay, bit-exact HDL simulation, generic synthesis and the
Quartus/board qualification steps.

## Use it

From the repository root, after installing the native host stack:

```bash
cargo run --locked -p bladerf-lab --release -- doctor
cargo run --locked -p bladerf-lab --release -- capture --output captures/my-first-rx.sc16q11
cargo run --locked -p bladerf-lab --release -- analyze captures/my-first-rx.sc16q11
```

Capture defaults to **RX1, 100 MHz, 2 MSps, 20 dB manual gain, one second**.
It disables the receive bias tee, saves little-endian interleaved int16 IQ plus a
JSON sidecar, and refuses existing output files. Choose a new filename each run.
Attach an antenna suitable for the band to RX1 to observe local signals.
`--channel 2` selects RX2; `--frequency 100.5e6` changes tuning.

The maintained [Rust application](../tools/bladerf-lab/src/main.rs) handles capture, analysis, image
verification, host setup, and process orchestration. Capture uses safe Rust
SoapySDR bindings through the installed SoapyBladeRF/libbladeRF stack. The
[setup guide](SETUP.md) describes native prerequisites;
[rust-toolchain.toml](../rust-toolchain.toml) pins the Rust compiler.

For finite streaming checks:

```bash
cargo run --locked -p bladerf-lab --release -- soapy-rx
cargo run --locked -p bladerf-lab --release -- gnuradio-rx
```

After a build, the binary can be reused directly:

```bash
cargo build --locked -p bladerf-lab
target/debug/bladerf-lab --help
```

Use `target/release/bladerf-lab` after `cargo build --locked -p bladerf-lab --release` for
optimized runs. Close other bladeRF applications before opening the same device.

For interactive upstream CLI control, run `bladeRF-cli -i`. In a graphical
session with audio, open `gnuradio-companion tools/bladerf-lab/examples/fm_receiver.grc`. That
example has bias tees off, preserves the factory oscillator trim, and starts at
102.9 MHz; choose a local station.

## Sources in this workspace

| Checkout | Purpose | Selected version |
| --- | --- | --- |
| [repos/bladeRF](../repos/bladeRF) | Native library, CLI, firmware, FPGA HDL and upstream examples | Nuand `2025.10` plus separate opt-in [configuration-neutral](../patches/bladerf-rf-observatory-config-neutral-open.patch) and [loader-neutral](../patches/bladerf-rf-observatory-loader-neutral-open.patch) profile-2 patches |
| [repos/bladeRF-wiki](../repos/bladeRF-wiki) | Offline upstream guides and architecture documentation | Pinned Git commit |
| [repos/SoapyBladeRF](../repos/SoapyBladeRF) | bladeRF driver for SoapySDR | `soapy-bladerf-0.4.3` |
| [repos/gr-bladeRF](../repos/gr-bladeRF) | GNU Radio source/sink blocks and examples | Pinned Git commit plus [compatibility patch](../patches/gr-bladeRF-fedora.patch) |
| [repos/blue-dragon](../repos/blue-dragon) | Rust BLE and Classic Bluetooth decoder | Pinned Git commit plus [RX setup patch](../patches/blue-dragon-bladerf-rx-setup.patch) |
| [repos/ice9-bluetooth-sniffer](../repos/ice9-bluetooth-sniffer) | Optional BLE 1M reference decoder | Pinned Git commit plus [RX bias patch](../patches/ice9-bluetooth-bladerf-rx-bias.patch) |
| [repos/libcrafter](../repos/libcrafter) | Rust Wi-Fi PHY library for bounded IQ replay | `crafter` 0.3.5 at [`f9ef000bc402`](https://github.com/pellegre/libcrafter/tree/f9ef000bc402eebc676f6b4708395aa8ac169450) |
| [repos/FutureSDR](../repos/FutureSDR) | Live legacy OFDM receiver and offline IEEE 802.15.4 decoder | [`42832f5473ec`](https://github.com/FutureSDR/FutureSDR/tree/42832f5473ec19ce641c58a4792befdc57945ff5) plus the [Wi-Fi](../projects/wifi-monitor/docs/guide.md#build-the-optional-live-engine) or [IEEE 802.15.4](../projects/ieee802154-monitor/futuresdr-worker/docs/guide.md#build-and-run) patch set |

These are Git submodules. Clone with `--recurse-submodules`, or run
`git submodule update --init --recursive` in an existing checkout. Rust host setup
applies the saved gr-bladeRF patch; that submodule reports intentional local
modifications. The monitoring project guides cover their additional decoder
patches and builds.

## Host setup and validation

The Rust bootstrap command builds without the optional SDR and plotting
features. Its `setup-fedora` implementation installs native prerequisites using
DNF and sudo; inspect the plan before running it on a suitable installation:

```bash
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora
```

The command does not flash firmware or enable transmission. The
[setup guide](SETUP.md) describes its scope, native prerequisites and
installation checks.

```bash
cargo run --locked -p bladerf-lab --release -- fetch-images
```

This downloads/checks official release images without programming the board.
Captures, plots, generated GNU Radio programs, build outputs, and downloaded
images remain ignored by Git. Keep installation and experiment evidence local;
see [setup validation](SETUP.md#validation) for example checks. Quartus is an
optional toolchain for FPGA development.
