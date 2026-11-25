# Native libbladeRF builds and preflight

## Library preflight

On x86-64 Linux, the library check copies the selected shared-library bytes into
a private sealed in-memory file while computing their SHA-256, loads that
verified snapshot, resolves the required native symbols and calls
`bladerf_version`. With either specialized requirement below, it also calls the
selected patch's device-independent build-marker function. Loader-neutral
qualification additionally resolves, but never calls, the typed image-load,
Wishbone-write, receive-configuration/readback and synchronous-RX symbols.
Loading a shared object runs its initializers; the pinned libbladeRF 2.6.0
[initializer configures logging][library-initializer] and does not enumerate or
open a device. The checker itself performs no radio open, USB access, FPGA load,
capture, RF setting or transmit operation.

The tracked
[`bladerf-rf-observatory-config-neutral-open.patch`](../../../patches/bladerf-rf-observatory-config-neutral-open.patch)
provides that separately identifiable build path for pinned Nuand commit
`fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47`. Use the
[guarded capture procedure](../../../projects/rf-observatory/docs/guide.md#guarded-profile-2-ram-capture)
for USB, image-transition, RF and Wishbone qualification on hardware.

The CPU-only x86-64 Linux library preflight requires an explicit file and its
independently obtained lowercase SHA-256:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-native-library \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

The placeholder hash above is intentionally not a known library. Use a digest
from a separately trusted build manifest; supplying a digest alongside an
arbitrary file does not establish its provenance. A successful check establishes
only the loaded-byte digest, required symbol availability and reported semantic
version `2.6.0`. The function declarations come from the audited pinned header
whose API macro is `0x02060000`; that macro is not reported at runtime. This is
not device, FPGA, USB or RF evidence. The command does not authenticate the
dynamic loader or transitive shared-library dependencies, so run it only in the
controlled pinned build environment being qualified.

## Specialized bladeRF 2 open build

Ordinary upstream `bladerf_open` can [automatically load a default FPGA
image][automatic-fpga], initialize the board and RFIC, cancel scheduled retunes,
and [process `bladeRF.conf`][configuration-load]. Configuration entries can
change [FPGA, RF and bias-tee state][configuration-actions]. The opt-in patch
leaves that behavior unchanged by default. Its
`ENABLE_LIBBLADERF_BLADERF2_CONFIG_NEUTRAL_OPEN=ON` mode instead:

- rejects bladeRF 1 devices and a bladeRF 2 without an already loaded FPGA;
- reads the FPGA version and selects the compatible host packet protocol needed
  for Wishbone access, but skips board/RFIC initialization;
- skips default-image search/loading, `bladeRF.conf`, and open/close retune
  cancellation; and
- refuses to configure unless `ENABLE_USB_DEV_RESET_ON_OPEN=OFF`.

This is a scoped open contract, not a read-only library. The USB backend still
claims the device interface and selects its NULL, SPI-flash and RF-link
alternate settings, so opening is exclusive transport activity and can
interrupt another stream.
Open also performs its existing readiness, firmware-version, bus-speed, flash
metadata and FPGA-status/version reads before returning the handle.
Explicit API calls—including Wishbone writes and FPGA-loading APIs—remain
capable of changing the device. A native adapter must therefore require the
versioned marker and expose only its reviewed operation set.

Apply and build the patch in a fresh ignored clone, leaving the pinned submodule
unchanged:

```sh
git clone --no-hardlinks repos/bladeRF \
  build/rf-observatory/libbladerf-config-neutral-source
git -C build/rf-observatory/libbladerf-config-neutral-source \
  checkout --detach fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47
git -C build/rf-observatory/libbladerf-config-neutral-source \
  submodule update --init --recursive
git -C build/rf-observatory/libbladerf-config-neutral-source \
  apply --check "$PWD/patches/bladerf-rf-observatory-config-neutral-open.patch"
git -C build/rf-observatory/libbladerf-config-neutral-source \
  apply "$PWD/patches/bladerf-rf-observatory-config-neutral-open.patch"

cmake -S build/rf-observatory/libbladerf-config-neutral-source \
  -B build/rf-observatory/libbladerf-config-neutral-build \
  -DCMAKE_BUILD_TYPE=Release \
  -DENABLE_LIBBLADERF_BLADERF2_CONFIG_NEUTRAL_OPEN=ON \
  -DENABLE_USB_DEV_RESET_ON_OPEN=OFF
cmake --build build/rf-observatory/libbladerf-config-neutral-build \
  --target libbladerf_shared --parallel 4
```

Record the produced library's lowercase SHA-256 in the separately trusted local
build manifest, then require marker V1 (`0x42324e01`) during preflight:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-native-library \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --require-bladerf2-config-neutral-open
```

The requirement fails closed when the marker symbol is missing, disabled, or
unknown. It still invokes no device API. CPU qualification must compare the
default and enabled builds, confirm the default-image and configuration-file
references are absent only from enabled objects, compare their exported symbols
and SONAME, and exercise the positive and negative marker checks. Do not use
`bladerf_open`, discovery, `doctor`, or a hardware test for this build-only
checkpoint.

## Loader-neutral image-transition build

The separate
[`bladerf-rf-observatory-loader-neutral-open.patch`](../../../patches/bladerf-rf-observatory-loader-neutral-open.patch)
is the build prerequisite for the guarded RAM image-transition and recovery
worker boundary. It targets the same pinned Nuand commit but has a different
contract and marker from the configuration-neutral build. Configure it with
`ENABLE_LIBBLADERF_BLADERF2_LOADER_NEUTRAL_OPEN=ON` and
`ENABLE_USB_DEV_RESET_ON_OPEN=OFF`; the build rejects USB reset-on-open and a
simultaneously enabled configuration-neutral mode.

Marker V1 (`0x42324c01`, `B2L` revision 1) establishes that:

- only bladeRF 2 is accepted, whether its FPGA is configured or unconfigured;
- open performs the firmware readiness/version, bus-speed, flash-metadata and
  firmware-reported FPGA-status queries, but never reads the current FPGA
  version, selects its protocol, loads an automatic image or initializes the
  board/RFIC;
- open returns with the internal board state at firmware-loaded and skips
  `bladeRF.conf` plus open/close scheduled-retune cancellation;
- USB reset-on-open and environment overrides for FPGA identity/status, image
  size and tuning mode cannot weaken the specialized contract; and
- an explicit `bladerf_load_fpga` loads the supplied RAM image, requires a
  positive configured-status result and then performs ordinary board/RFIC
  initialization in host tuning mode.

Opening still claims the USB interface and may select transport alternate
settings. The broader library still contains explicit flash, FPGA, RF and TX
APIs; a real worker process must retain only its reviewed allowlist and must
verify stock-image restoration after every attempted custom-image session. An
ambiguous load or initialization failure requires a fresh worker and handle for
the restoration attempt, so recovery does not depend on the possibly damaged
current FPGA protocol.

Apply and build this alternative patch in its own fresh ignored clone:

```sh
git clone --no-hardlinks repos/bladeRF \
  build/rf-observatory/libbladerf-loader-neutral-source
git -C build/rf-observatory/libbladerf-loader-neutral-source \
  checkout --detach fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47
git -C build/rf-observatory/libbladerf-loader-neutral-source \
  submodule update --init --recursive
git -C build/rf-observatory/libbladerf-loader-neutral-source \
  apply --check "$PWD/patches/bladerf-rf-observatory-loader-neutral-open.patch"
git -C build/rf-observatory/libbladerf-loader-neutral-source \
  apply "$PWD/patches/bladerf-rf-observatory-loader-neutral-open.patch"

cmake -S build/rf-observatory/libbladerf-loader-neutral-source \
  -B build/rf-observatory/libbladerf-loader-neutral-build \
  -DCMAKE_BUILD_TYPE=Release \
  -DENABLE_LIBBLADERF_BLADERF2_LOADER_NEUTRAL_OPEN=ON \
  -DENABLE_USB_DEV_RESET_ON_OPEN=OFF
cmake --build build/rf-observatory/libbladerf-loader-neutral-build \
  --target libbladerf_shared --parallel 4
```

Qualify an exact resulting library without device access by supplying its
separately trusted digest:

```sh
cargo run --locked -p bladerf-profile2-adapter -- \
  check-native-library \
  --library /absolute/path/to/libbladeRF.so.2 \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --require-bladerf2-loader-neutral-open
```

The placeholder hash is intentionally not a known library. A successful check
establishes only the sealed bytes, semantic version, loader symbol and exact
marker; it does not open a device or load an image. The current `native` register
server does **not** consume the loader-neutral marker or perform image
transitions. It continues to require the configuration-neutral build and a
separately loaded, identity-checked profile-2 image. Do not substitute this
library in that server.

[library-initializer]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/init_fini.c#L79-L91
[automatic-fpga]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L430-L489
[configuration-load]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/bladerf.c#L167-L173
[configuration-actions]: https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/helpers/configfile.c#L43-L128
