# Vendored source

The workspace uses the local [`soapysdr` 0.5.1 source](soapysdr-0.5.1/README.md)
through its [Cargo patch](../Cargo.toml). Local changes expose receive timestamps
and terminal flags and report errors when explicitly closing a receive stream.

The [provenance note](soapysdr-0.5.1/UPSTREAM.md) records the upstream revision,
archive hash, and local changes. License texts are
[Apache 2.0](soapysdr-0.5.1/LICENSE) and [Boost 1.0](soapysdr-0.5.1/LICENSE-BSL).
The [setup guide](../docs/SETUP.md) covers the native SDR libraries.
