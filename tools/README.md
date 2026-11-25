# Repository tools

Run these Cargo packages from the repository root. Start with the guide for
the tool you need.

| Tool | Use it for |
| --- | --- |
| [`bladerf-lab`](bladerf-lab/README.md) | Capture and analyze IQ, inspect the radio, and set up the native stack. This is the default workspace package. |
| [`bladerf-docs`](bladerf-docs/README.md) | Check the source catalog and render its reference page. |
| [`bladerf-profile2-adapter`](bladerf-profile2-adapter/README.md) | Build and check the process adapter used by the RF observatory's Profile-2 transport. |

```console
cargo run --locked -p bladerf-lab -- --help
```

See the [setup guide](../docs/SETUP.md) for the native dependencies.
