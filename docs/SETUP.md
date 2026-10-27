# Setup and validation

This guide explains the repository's setup workflow and how to check a new
installation. Keep installation logs, device identities and measurements in
ignored local evidence.

Run commands from the workspace root unless stated otherwise. The root manifest
is a virtual Cargo workspace; [`tools/bladerf-lab`](../tools/bladerf-lab) is its
default member. Commands name `-p bladerf-lab` explicitly so the intended package
remains clear as the workspace grows.

## Prerequisites and scope

The [toolchain file](../rust-toolchain.toml) pins the Rust compiler, components
and build target. Use that toolchain and a host C linker. `Cargo.lock` records
Rust dependency versions; use `--locked` when reproducing builds. See
[CONTRIBUTING.md](../CONTRIBUTING.md) for workspace development checks.

The `radio` and `analysis` features are enabled by default. Radio capture needs
SoapySDR development files, the SoapyBladeRF plugin and libbladeRF. Analysis uses
Rust FFT/PNG libraries with native font support. `--no-default-features` omits
both, allowing the bootstrap CLI to build before those prerequisites exist.

The implemented native installer is **`setup-fedora`**. It uses DNF package names,
sudo, Linux linker configuration and udev. This is an implementation boundary,
not a claim that every Fedora release or another distribution has been tested.
For a different platform, use Nuand's
[installation documentation](https://github.com/Nuand/bladeRF/wiki/Getting-Started%3A-Linux)
and the relevant SoapySDR/GNU Radio package instructions, then check the native
stack before building the default Rust features.

## Inspect and run the native setup plan

Initialize the upstream sources and inspect the plan first:

```bash
git submodule update --init --recursive
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
```

The dry run prints commands and conditional actions without executing them. The
plan installs build tools and native development packages, builds the pinned
libbladeRF/SoapyBladeRF/gr-bladeRF sources, configures access permissions, and
verifies release-image downloads. Its implementation is in
[`host.rs`](../tools/bladerf-lab/src/host.rs).

On a suitable Fedora installation, run as a regular user with sudo access:

```bash
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora
```

The command uses `/usr/local` as its native installation prefix. It creates the
`plugdev` group if needed, adds the invoking account when necessary, installs
upstream udev rules, and configures `/usr/local/lib` and `/usr/local/lib64` for
the dynamic linker. If group membership changes, log out and back in before
accessing the radio.

Native build concurrency defaults to four jobs; `--jobs` changes it. The
`--disable-repo` option can exclude a named DNF repository for that invocation
when necessary. GNU Radio's upstream Python bindings remain a native integration
dependency; the maintained setup, capture and analysis implementations are Rust.
Setup does not flash firmware or start a transmit stream.

The installer also applies the saved
[gr-bladeRF compatibility patch](../patches/gr-bladeRF-fedora.patch). It checks
whether the patch is already present and rejects an incompatible checkout.
Inspect intentional vendor changes with:

```bash
git -C repos/gr-bladeRF diff
git -C repos/gr-bladeRF apply --reverse --check ../../patches/gr-bladeRF-fedora.patch
```

## Build and inspect the stack

After installing native prerequisites:

```bash
cargo build --locked -p bladerf-lab --release
target/release/bladerf-lab --help
target/release/bladerf-lab doctor
```

`doctor` invokes USB/device discovery, native version queries, SoapySDR
discovery, GNU Radio version inspection and GRC compilation. It does not enable
a receive or transmit stream. Read its output to establish device identity,
firmware/gateware versions and native-library availability, and keep that output
in local evidence.

Close other applications using the same device before opening it. The CLI finds
the workspace from the current directory and its ancestors; from elsewhere,
pass `--workspace /path/to/bladerf`.

## Obtain compatible release images

```bash
cargo run --locked -p bladerf-lab --release -- fetch-images
```

This downloads and checks the official release artifacts selected in
[`config/images.sha256`](../config/images.sha256): FX3 2.6.0 and hosted xA9
FPGA 0.16.0. The command does not program the radio.

Match the FPGA variant and firmware/gateware compatibility before considering
an update. The pinned
[micro compatibility table](../repos/bladeRF/host/libraries/libbladeRF/src/board/bladerf2/compatibility.c)
requires firmware 2.6.0 or later for FPGA 0.16.0. Nuand's
[release](https://github.com/Nuand/bladeRF/releases/tag/2025.10),
[FPGA](https://www.nuand.com/fpga_images/) and
[FX3](https://www.nuand.com/fx3_images/) pages identify upstream artifacts.
The [autoload guide](../repos/bladeRF-wiki/FPGA-Autoloading.mediawiki) explains
host and flash loading arrangements. Download verification and programming are
separate operations.

## Validation

Check software independently of hardware first:

```bash
cargo test --locked -p bladerf-lab
cargo clippy --locked -p bladerf-lab --all-targets -- -D warnings
```

These commands check the host implementation; they do not establish RF behavior.
The [receive lifecycle guide](LEARNING.md#5-follow-the-rust-receive-lifecycle-and-native-boundary)
explains stream shutdown and capture-file behavior.

For a short receive experiment, select a suitable input and use fresh filenames:

```bash
target/release/bladerf-lab capture --output captures/setup-check.sc16q11
target/release/bladerf-lab analyze captures/setup-check.sc16q11 --output artifacts/setup-check-psd.png
target/release/bladerf-lab soapy-rx
target/release/bladerf-lab gnuradio-rx
```

Inspect `capture --help`, choose appropriate settings and
record readbacks alongside the results. It saves SC16Q11 IQ and a JSON sidecar
and refuses existing outputs. A completed single-channel capture contains four
bytes per complex sample; compare the file size with the completed sample count.
Analysis reports relative dBFS/Hz, not calibrated dBm. Untimestamped SC16Q11 does
not by itself certify continuity.

`soapy-rx` and `gnuradio-rx` perform bounded receive checks. The latter compiles
the [finite flowgraph](../tools/bladerf-lab/examples/gnuradio_rx.grc), runs its
generated program and validates sample count, finite values and nonzero energy.
GNU Radio generates its Python program under ignored
`build/bladerf-lab/gnuradio-rx/`; orchestration and result checks are Rust.
Compilation, streaming, calibrated performance and sustained maximum-rate
throughput are separate validation claims. Keep device information, raw IQ,
metadata, plots and installation logs local.

For a graphical FM experiment, use the
[learning guide](LEARNING.md#4-explore-gnu-radio-and-fm-reception). Compiling a
GRC graph does not establish that its display or audio path works.

## Troubleshooting

- **Permission denied or no device:** inspect discovery output, udev rules,
  group membership and device-node permissions. A new group membership needs a
  fresh login session.
- **Native library not found:** inspect the linker configuration and installed
  library paths; refresh the linker cache after correcting an installation.
- **Rust cannot find SoapySDR:** install its native development files and inspect
  `pkg-config --modversion SoapySDR`.
- **Soapy cannot find bladeRF:** inspect `SoapySDRUtil --info` and the plugin
  search paths reported for that installation.
- **Busy device or timeouts:** close other device users, start with a modest
  sample rate, inspect negotiated USB speed and investigate the cable/connection
  path before increasing throughput.
- **No display or audio:** run the FM GUI in a suitable graphical session;
  capture and PNG analysis do not require that GUI.

Upstream references: [basic operation](https://github.com/Nuand/bladeRF/wiki/Getting-Started%3A-Verifying-Basic-Device-Operation)
and [troubleshooting](https://github.com/Nuand/bladeRF/wiki/Troubleshooting).
