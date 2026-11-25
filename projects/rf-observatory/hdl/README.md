# FPGA channel bank and framed transport

The VHDL channel bank translates and decimates four receive channels. The
framed transports carry channel samples and configuration to the host.

## Quick start

[Initialize the workspace submodules](../../../repos/README.md) and install the
[documented GHDL version](../docs/guide.md#build-and-check). From the repository root, run:

```sh
cargo run --locked -p rf-observatory -- verify-hdl --timeout 120 \
  --output artifacts/observatory-hdl
```

This runs simulation and generic synthesis with deterministic fixtures.
Use a new output directory for each run.

## Guides

- [Streaming and wire contracts](../docs/gateware.md#numeric-and-streaming-contract)
- [Quartus build procedure](../docs/gateware.md#quartus-baseline-and-custom-image-build)
- [Verification coverage](../docs/gateware.md#verification-and-its-limits)
- [Guarded Profile-2 capture](../docs/guide.md#guarded-profile-2-ram-capture)
