# Rust build requirements

The workspace pins **Rust 1.99.1**, edition **2024**, with `rustfmt` and `clippy`
in [rust-toolchain.toml](../rust-toolchain.toml). The configured build target is
`x86_64-unknown-linux-gnu`. Use the pinned toolchain and `--locked` to reproduce
the dependency resolution in [Cargo.lock](../Cargo.lock).

The root [Cargo.toml](../Cargo.toml) is a virtual workspace with resolver 3,
shared build profiles and a shared `target/` directory. Its default member is
[`bladerf-lab`](../tools/bladerf-lab/README.md). Select an explicit package with
`-p bladerf-lab` or `-p bladerf-docs`; use `--workspace` for both.

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
