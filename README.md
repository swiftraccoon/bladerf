# bladeRF micro xA9 lab

A Rust learning workspace for the **bladeRF 2.0 micro xA9**. Start with the
[handbook](docs/handbook/index.md), [learning guide](docs/LEARNING.md), and
[setup guide](docs/SETUP.md). The [build instructions](docs/BUILDING.md)
produce a searchable local handbook. See [CONTRIBUTING.md](CONTRIBUTING.md)
for development setup, checks and contribution conventions.

## Workspace layout

The root `Cargo.toml` is a virtual workspace. Its default member,
[`tools/bladerf-lab`](tools/bladerf-lab), contains the host CLI, tests,
and GNU Radio flowgraphs.
[`tools/bladerf-docs`](tools/bladerf-docs) validates the source-qualified fact
catalog and reproduces its handbook reference without radio dependencies. The
toolchain, `Cargo.lock`, native source checkouts, patches, and image hashes stay
at the repository root. Cargo build output remains under root `target/`.
See the [tool's own guide](tools/bladerf-lab/README.md) for its commands and
component documentation.

Cargo commands below name `-p bladerf-lab` explicitly; omitting the selector
uses that default member.

## Monitoring projects

The [Bluetooth monitor](projects/bluetooth-monitor) provides passive BLE and
Classic Bluetooth capture, Wireshark-compatible files, decoded JSON records,
and independent BLE CRC validation. Its guide describes decoder setup, receive
connector selection, coverage limits and offline inspection.

## Use it

From this directory, after installing the native host stack:

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

The maintained [Rust application](tools/bladerf-lab/src/main.rs) handles capture, analysis, image
verification, host setup, and process orchestration. Capture uses safe Rust
SoapySDR bindings through the installed SoapyBladeRF/libbladeRF stack. The
[setup guide](docs/SETUP.md) describes native prerequisites;
[rust-toolchain.toml](rust-toolchain.toml) pins the Rust compiler.

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
| [repos/bladeRF](repos/bladeRF) | Native library, CLI, firmware, FPGA HDL and upstream examples | Nuand `2025.10` |
| [repos/bladeRF-wiki](repos/bladeRF-wiki) | Offline upstream guides and architecture documentation | Pinned Git commit |
| [repos/SoapyBladeRF](repos/SoapyBladeRF) | bladeRF driver for SoapySDR | `soapy-bladerf-0.4.3` |
| [repos/gr-bladeRF](repos/gr-bladeRF) | GNU Radio source/sink blocks and examples | Pinned Git commit plus [compatibility patch](patches/gr-bladeRF-fedora.patch) |
| [repos/blue-dragon](repos/blue-dragon) | Rust BLE and Classic Bluetooth decoder | Pinned Git commit plus [RX setup patch](patches/blue-dragon-bladerf-rx-setup.patch) |
| [repos/ice9-bluetooth-sniffer](repos/ice9-bluetooth-sniffer) | Optional BLE 1M reference decoder | Pinned Git commit plus [RX bias patch](patches/ice9-bluetooth-bladerf-rx-bias.patch) |

These are Git submodules. Clone with `--recurse-submodules`, or run
`git submodule update --init --recursive` in an existing checkout. Rust host setup
applies the saved gr-bladeRF patch; that submodule reports intentional local
modifications. The Bluetooth project guide covers its decoder patches and builds.

## Host setup and validation

The Rust bootstrap command builds without the optional SDR and plotting
features. Its `setup-fedora` implementation installs native prerequisites using
DNF and sudo; inspect the plan before running it on a suitable installation:

```bash
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora
```

The command does not flash firmware or enable transmission. The
[setup guide](docs/SETUP.md) describes its scope, native prerequisites and
installation checks.

```bash
cargo run --locked -p bladerf-lab --release -- fetch-images
```

This downloads/checks official release images without programming the board.
Captures, plots, generated GNU Radio programs, build outputs, and downloaded
images remain ignored by Git. Keep installation and experiment evidence local;
see [setup validation](docs/SETUP.md#validation) for example checks. Quartus is an
optional toolchain for FPGA development.
