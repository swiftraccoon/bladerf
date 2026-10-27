# Rust build requirements

The workspace pins **Rust 1.99.1**, edition **2024**, with `rustfmt` and `clippy`
in [rust-toolchain.toml](../rust-toolchain.toml). The configured build target is
`x86_64-unknown-linux-gnu`. Use the pinned toolchain and `--locked` to reproduce
the dependency resolution in [Cargo.lock](../Cargo.lock).

The root [Cargo.toml](../Cargo.toml) is a virtual workspace with resolver 3,
shared build profiles and a shared `target/` directory. Its default member is
[`bladerf-lab`](../tools/bladerf-lab/README.md). Select an explicit package with
`-p bladerf-lab`, `-p bladerf-docs`, `-p bluetooth-monitor` or `-p wifi-monitor`;
use `--workspace` for all members.

## Lab tool features and native libraries

The lab tool's [manifest](../tools/bladerf-lab/Cargo.toml) defines two additive
features, both enabled by default:

| Feature | Function | Native prerequisites |
| --- | --- | --- |
| `radio` | Receive capture and finite stream checks through `soapysdr` | SoapySDR development files, SoapyBladeRF plugin, libbladeRF |
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
than a libpcap device. The dependency specifies an exact Git revision, matching
the [libcrafter submodule](../repos/libcrafter); the submodule retains the source
and independent waveform fixtures for review and upstream tests.

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
[project instructions](../projects/wifi-monitor/README.md#build-the-optional-live-engine).
Runtime reception requires SoapySDR, SoapyBladeRF and libbladeRF.
