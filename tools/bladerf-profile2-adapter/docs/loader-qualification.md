# Device-free loader qualification

These checks use the [loader-neutral library](native-library.md#loader-neutral-image-transition-build)
and the [staged image roles](image-staging.md).

## Device-free loader worker qualification

The combined command below retains the typed loader function table from the
sealed B2L1 library, stages the exact image roles, and exercises the loader state
machine with deterministic fake calls:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-profile2-loader-worker \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

The retained table comes from the same hash-qualified sealed snapshot as its
typed `bladerf_load_fpga` pointer. The command never invokes that pointer. The
fake primary worker owns both image descriptors across its synchronous calls and
checks this exact state sequence:

```text
Ready -> CustomUnverified -> CustomVerified -> StockUnverified
      -> StockVerified -> Closed
```

The worker enters a fail-closed terminal state before every load or identity
callback, then advances to unverified or verified only after exact success. Any
failure or unwind permits no later handle call, including close. Custom identity
requires configured state `1`, host source `2`, FPGA version `0.16.0`, profile
ID `0x52460201` and capabilities `0x00001f04`.
Stock identity requires configured state `1`, host source `2` and FPGA version
`0.16.0`. A second release trace makes the primary custom dispatch ambiguous,
abandons that fake handle without closing it, and restores stock through a
distinct recovery worker whose stock descriptor was staged independently.

This command loads the sealed shared object, so its initializer and transitive
dependencies run. The adapter itself invokes only `bladerf_version` and the
device-independent B2L1 marker; all open, image-load and identity operations in
the two traces are fake. The adapter invokes no device-facing libbladeRF
function or USB, FPGA, RF, capture, bias-tee, flash or TX operation. A supplied
digest is not provenance, so use the separately trusted build and controlled
runtime in the [library guide](native-library.md). The loader fixtures below
add a killable protocol process. For an attached device, use the RF-observatory
[guarded capture procedure](../../../projects/rf-observatory/docs/guide.md#guarded-profile-2-ram-capture)
to run the supervised session binding.

## Killable loader fixture processes

The parent-side checker starts a primary child followed by a distinct recovery
child, validates each exact value-only bundle attestation and completes both
typed lifecycles:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-profile2-loader-process \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

The checker validates syntax and roles before spawn without opening or hashing
the supplied paths. Each child then repeats validation and performs its own
staging and library qualification inside the finite Hello deadline. The parent
removes `LD_PRELOAD`, `LD_AUDIT` and `LD_LIBRARY_PATH`, permits one outstanding
request, and uses private-process-group kill/reap fallback. It never transfers a
path over the wire; paths are fixed child arguments and the Hello attests only
the resulting version, marker, hashes and byte counts.

The role-fixed child entry points below expose that separate loader protocol.
They accept no serial or device selector and invoke no device-facing function
from the retained library table:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  loader-fixture-primary \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --custom-image /absolute/path/to/rf-observatory-profile2-xA9.rbf \
  --custom-sha256 "$PROFILE2_IMAGE_SHA256" \
  --custom-bytes "$PROFILE2_IMAGE_BYTES" \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf

cargo run --locked -p bladerf-profile2-adapter -- \
  loader-fixture-recovery \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --stock-image /absolute/path/to/hostedxA9-v0.16.0.rbf
```

These are framed-stdin/stdout child commands, not interactive commands. They
require an exact loader-family, protocol-version, request-ID and role hello
before accessing any supplied path, staging an image, mapping the shared object
or constructing the fake worker. The successful hello reports value-only
library and image facts; it never reports source paths or a device identifier.
The primary process stages both roles, while every recovery process stages its
own stock snapshot independently.

The primary lifecycle is `Ready -> CustomUnverified -> CustomVerified ->
StockUnverified -> StockVerified -> Closed`; recovery starts at `Ready` and can
only traverse the stock states. Explicit shutdown is also valid directly from
untouched `Ready`. Any malformed frame, wrong mode, invalid transition, backend
failure, EOF or lost response is terminal. Error handling publishes at most one
bounded correlated error when possible and never calls orderly worker shutdown;
process exit and the guarded worker's fail-closed Drop policy handle ambiguous
states. The parent clients use finite transaction deadlines and private-process-
group kill/reap cleanup.

Both child commands retain the exact hash-qualified B2L1 function table, but their
workers receive deterministic fake calls only. Loading the shared object still
runs its initializer and transitive loader dependencies. The adapter invokes
only the device-independent version and B2L1 marker functions during startup;
it invokes no open, USB, FPGA-load, RF, capture, bias-tee, flash or TX function.
The [native session](native-session.md) binds `LoaderDynamicCalls` to a worker.
