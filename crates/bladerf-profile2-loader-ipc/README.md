# Profile-2 loader IPC

This library defines the guarded image-transition protocol used by the Profile-2
adapter. The primary role can load and verify a custom image, then restore and
verify stock. The recovery role is limited to stock restoration and verification.

[Initialize the workspace submodules](../../repos/README.md), then run the host
tests from the repository root.

```sh
cargo test --locked -p bladerf-profile2-loader-ipc
```

See the [crate reference](src/lib.rs) for the wire format and a fixture request
example. The [adapter guide](../../tools/bladerf-profile2-adapter/README.md)
covers the loader process and image staging.
