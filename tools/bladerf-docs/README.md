# bladeRF documentation catalog tools

`bladerf-docs` checks the source-qualified fact catalog and reproduces its
generated handbook reference. It runs without the native receive stack.

## Quick start

[Initialize the workspace submodules](../../repos/README.md), then run from the
repository root.

```sh
cargo run --locked -p bladerf-docs -- check
cargo run --locked -p bladerf-docs -- render --check
```

See the [catalog tool guide](docs/guide.md) for regeneration, validation rules
and limits, or the [handbook build guide](../../docs/BUILDING.md) for HTML output.
