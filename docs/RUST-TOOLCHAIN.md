# Rust build requirements

The workspace pins **Rust 1.99.1**, edition **2024**, with `rustfmt` and `clippy`
in [rust-toolchain.toml](../rust-toolchain.toml). The configured build target is
`x86_64-unknown-linux-gnu`. Use the pinned toolchain and `--locked` to reproduce
the dependency resolution in [Cargo.lock](../Cargo.lock).

The root [Cargo.toml](../Cargo.toml) is a virtual workspace with resolver 3,
shared build profiles and a shared `target/` directory. Its default member is
[`bladerf-lab`](../tools/bladerf-lab/README.md). Select an explicit package with
`-p bladerf-lab`, `-p bladerf-docs`, `-p bluetooth-monitor`, `-p wifi-monitor`
or `-p rf-observatory`;
use `--workspace` for all members.

## Lab tool features and native libraries

The lab tool's [manifest](../tools/bladerf-lab/Cargo.toml) defines two additive
features, both enabled by default:

| Feature | Function | Native prerequisites |
| --- | --- | --- |
| `radio` | Receive capture and finite stream checks through `soapysdr` | SoapySDR development files, SoapyBladeRF plugin, libbladeRF, C toolchain for `ring` |
| `analysis` | IQ statistics and FFT/PNG plots | Native font support used by Plotters |
| Neither | Diagnostics, verified image downloads and setup commands | A host C linker and the external tools invoked by the selected command |

The native integration baseline is **SoapySDR 0.8.1**, **SoapyBladeRF 0.4.3** and
**libbladeRF 2.6.0**. The Rust wrapper is **soapysdr 0.5.1**; exact resolved Rust
dependencies are recorded in the lockfile. Native source revisions are pinned
by the [submodules](../.gitmodules). Select firmware and FPGA images using
Nuand's compatibility requirements; downloadable artifact hashes are in
[config/images.sha256](../config/images.sha256).

The Fedora installer builds without the radio and analysis prerequisites:

```sh
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
```

The [setup guide](SETUP.md) explains its installation scope and native-stack
checks. To select a feature combination explicitly:

```sh
cargo check --locked -p bladerf-lab --no-default-features
cargo check --locked -p bladerf-lab --no-default-features --features radio
cargo check --locked -p bladerf-lab --no-default-features --features analysis
```

GNU Radio examples require GNU Radio Companion and its bladeRF blocks. The
upstream generator emits Python programs from `.grc` inputs; the Rust tool
orchestrates finite runs and validates their output.

## Documentation tool

[`bladerf-docs`](../tools/bladerf-docs/README.md) uses the same Rust toolchain
and lockfile, with no native SDR dependency. Its checks and rendering run without
a connected radio or runtime network access:

```sh
cargo check --locked -p bladerf-docs --all-targets
cargo run --locked -p bladerf-docs -- check
cargo run --locked -p bladerf-docs -- render --check
```

Rendering handbook HTML or text uses Sphinx separately; see
[BUILDING.md](BUILDING.md). Contribution checks and coding conventions are in
[CONTRIBUTING.md](../CONTRIBUTING.md).

## Bluetooth monitor

[`bluetooth-monitor`](../projects/bluetooth-monitor/README.md) builds with the
same toolchain and lockfile. PCAP inspection has no native SDR prerequisite;
live reception launches a separately built decoder. The project guide specifies
the pinned decoder sources, compatibility patches and upstream lockfile needed
for that build.

## Wi-Fi monitor

[`wifi-monitor`](../projects/wifi-monitor/README.md) uses the same stable
toolchain and root lockfile. Its default `decode` feature enables the pinned
`crafter` dependency's `radio` feature for offline legacy OFDM, DSSS/CCK and
single-stream HT20 decoding. The dependency requires the platform's **libpcap
development/link library**, even though this project feeds it saved IQ rather
than a libpcap device. The dependency uses the
[libcrafter submodule](../repos/libcrafter) at fetchable `v0.3.5` revision
`f9ef000b` plus the tracked
[OFDM observability/recovery patch](../patches/libcrafter-ofdm-observability-recovery.patch),
then resolves that patched package through a repository-relative path. The
combined patch reproduces the qualified linear endpoint `f4266c51` through
commits `261c4b44`, `5f2167e4`, `17fe56c9` and `f4266c51`; later experimental
MCS7 shadow changes are excluded. The submodule also retains independent
waveform fixtures for review and upstream tests. Before a default-feature build,
apply the complete [ordered patch set](../projects/wifi-monitor/docs/guide.md#build-and-check).
It includes a
[terminal DATA-failure accounting patch](../patches/libcrafter-ofdm-data-failure-accounting.patch)
that adds closed legacy, HT-BCC and HT-LDPC failure counters. The
[Barker equalization patch](../patches/libcrafter-dsss-barker-equalization.patch)
adds a trained fallback for 1 and 2 Mbps DSSS. The
[HE signal patch](../patches/libcrafter-he20-su-signal.patch) provides the HE20
header diagnostic API used by the decoder. A separate
[Rust-2021 formatting-boundary patch](../patches/libcrafter-rustfmt-boundary.patch)
prevents root stable-workspace formatting checks from restyling the nested
upstream repository; it changes no compiled code.

```sh
cargo build --locked -p wifi-monitor --release
```

Without `decode`, PCAP inspection and the optional live engine's orchestration
do not compile or link libcrafter. Bounded IQ acquisition/replay requires
`decode`. Native IQ capture launches the separately built `bladerf-lab` binary
with its `radio` feature and the receive stack described above.

```sh
cargo build --locked -p wifi-monitor --no-default-features --release
```

The optional FutureSDR WLAN receiver is a separate upstream workspace requiring
**nightly-2026-10-27**. That requirement is scoped to its native receiver build;
the maintained Wi-Fi application and other workspace packages remain on stable.
Its reproducibility inputs are
[the separate toolchain selection](../config/wifi-decoder-toolchain.toml),
[upstream dependency lock](../config/wifi-decoder.Cargo.lock),
[pinned submodule](../repos/FutureSDR) and
[passive receive/FCS patch](../patches/futuresdr-wlan-passive-rx.patch).
Build it with explicit `--bin rx` and the `soapy` feature, using the
[project instructions](../projects/wifi-monitor/docs/guide.md#build-the-optional-live-engine).
Runtime reception requires SoapySDR, SoapyBladeRF and libbladeRF.

## RF observatory

[`rf-observatory`](../projects/rf-observatory/README.md) uses the stable workspace
toolchain and lockfile. Its Rust references, saved-IQ processing and controlled
waveform generation need no native SDR library. Live ordinary-IQ recording
launches the separately built `bladerf-lab` radio adapter.
Building the observatory or [digital monitor](../projects/digital-monitor/README.md)
requires a C toolchain for `ring`'s bundled SHA-256 backend.

The FPGA arithmetic and stream wrapper are VHDL-2008, integrated into Nuand's
existing VHDL design. This target-specific hardware description is separate
from the Rust host code. HDL verification uses **GHDL 6.0.0** for simulation and
generic synthesis. That does not produce a device bitstream or establish timing.
Cyclone V builds use **Quartus Prime Lite 23.1.1**, Cyclone V device support and
matching Nios II tools. See the
[HDL build guide](../projects/rf-observatory/hdl/README.md) for toolchain setup
and the checks required to qualify an image.
