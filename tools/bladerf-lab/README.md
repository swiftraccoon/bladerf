# bladeRF lab tools

`bladerf-lab` is the default workspace CLI for bladeRF receive capture,
IQ analysis, diagnostics, and native setup.

## Quick start

[Initialize the workspace submodules](../../repos/README.md), then list commands
without building the native receive stack. Run from the repository root or
this package directory.

```sh
cargo run --locked -p bladerf-lab --no-default-features -- --help
```

For the default build, follow the [native setup guide](../../docs/SETUP.md).
The [capture guide](docs/guide.md) covers ordinary and timestamped recording,
output receipts, buffering and retention. See the
[learning guide](../../docs/LEARNING.md) for hands-on workflows.
