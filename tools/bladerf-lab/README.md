# bladeRF lab tools

The `bladerf-lab` Rust CLI provides receive capture, IQ/PSD analysis, finite
SoapySDR/GNU Radio checks, device diagnostics, verified image downloads, and
Fedora native-stack setup. It is the default member of the root Cargo workspace.

From the repository root or this package directory:

```bash
cargo run --locked -p bladerf-lab -- --help
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
```

The CLI discovers the repository from the current directory and its ancestors.
Relative capture, build, download, and evidence paths resolve from the repository
root. From elsewhere, pass `--workspace /path/to/bladerf` to the CLI.

Source, integration tests and examples belong to this package. The root retains
the lockfile, build output, toolchain and native dependency checkouts. See the
[learning guide](../../docs/LEARNING.md), [setup guide](../../docs/SETUP.md), and
[contribution guide](../../CONTRIBUTING.md).
