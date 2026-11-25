# Profile-2 adapter

`bladerf-profile2-adapter` provides the child processes that the RF observatory
uses for Profile-2 register control, guarded capture and stock-image recovery.
It also provides CPU-only fixtures and preflight checks.

## Quick start

[Initialize the workspace submodules](../../repos/README.md). From the repository
root, build the adapter and run the device-free check.

```sh
cargo build --locked -p bladerf-profile2-adapter
cargo run --locked -p rf-observatory -- verify-profile2-adapter
```

This exercises the register protocol and models stock restoration and recovery
with no radio attached. The adapter's `fixture` subcommand reads binary frames
from standard input. Use the observatory command above to drive it.

For capture on hardware, follow the RF observatory's
[guarded Profile-2 capture procedure](../../projects/rf-observatory/docs/guide.md#guarded-profile-2-ram-capture).

## Guides

- [Native libbladeRF builds and preflight](docs/native-library.md)
- [FPGA image staging](docs/image-staging.md)
- [Device-free loader qualification](docs/loader-qualification.md)
- [Device-free session qualification](docs/session-qualification.md)
- [Native session and recovery children](docs/native-session.md)
- [Register protocol and server](docs/register-server.md)
