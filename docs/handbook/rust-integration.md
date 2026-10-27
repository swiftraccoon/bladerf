(rust-integration)=
# Choose a Rust integration for the micro xA9

A Rust application can use the micro xA9 through Nuand's native libbladeRF
library, or through SoapySDR and its SoapyBladeRF plugin. Choose the layer that
exposes the controls, sample formats and metadata the application needs.
An abstraction that supports basic streaming may omit board-specific clock,
trigger or status interfaces.

The versions and commits below identify specific source artifacts. They are
compatibility references, rather than assertions about the newest release.

## Compare the available layers

| Integration | What it provides | Micro xA9 considerations |
| --- | --- | --- |
| [libbladeRF 2.6.0](https://github.com/Nuand/bladeRF/tree/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF) | Native C discovery, streaming, sample metadata and board-specific controls | Direct access to micro APIs; Rust bindings must preserve the native ownership and buffer contracts |
| [soapysdr 0.5.1](https://docs.rs/soapysdr/0.5.1/soapysdr/) | Rust wrappers around SoapySDR discovery, capabilities, controls and typed sample streams | Reaches the micro through SoapyBladeRF/libbladeRF; inspect metadata and extension coverage for the required workflow |
| [Seify 0.26.2](https://github.com/FutureSDR/seify/tree/b2a76fafa1303e88f0028f48f42a4fdfc74014af) | Common radio interfaces, capability snapshots and several backends | Its Soapy backend can reach the micro; its native `bladerf1` feature targets the classic family |
| [Nuand/libbladeRF-rust](https://github.com/Nuand/libbladeRF-rust/tree/ab8b0387f3d55ac985307ade20bb535d3d221433) | Bindgen-based examples calling libbladeRF | Vendor integration examples with direct unsafe calls; a safe device-lifecycle abstraction remains an application responsibility |
| [bladerf 0.1.2](https://docs.rs/crate/bladerf/0.1.2/source/) | Community C-library wrapper and raw bindings | Contains classic-era controls and some newer functions; check micro coverage, selected sample format and lifecycle error paths before use |
| [libbladerf-rs 0.6.1](https://docs.rs/crate/libbladerf-rs/0.6.1/source/) | Pure Rust USB driver with session-based ownership | Supports classic x40/x115; its `bladerf2` implementation is unavailable |

For a portable capture application, the SoapySDR path supplies existing device
discovery and generic controls. Compare its available interfaces with the
application's requirements before writing a new adapter. Direct libbladeRF
access is useful when a required native control or metadata field is missing
from the chosen wrapper. Keep that adapter small and document its contracts.

The published
[`libbladerf-rs` bladeRF2 module](https://docs.rs/crate/libbladerf-rs/0.6.1/source/src/bladerf2.rs)
contains a compile-time error declaring support unimplemented. A feature name
alone therefore does not establish micro support. Likewise,
[`bladerf-bindings` 0.0.13](https://docs.rs/crate/bladerf-bindings/0.0.13/source/build.rs)
fetches and builds a pinned R2-Wireless native fork. Treat its headers, generated
declarations and native library as a coordinated stack; arbitrary installed
Nuand versions are a different compatibility question.

## Check stream data and metadata together

The Rust element type, native stream format and stored file encoding need
separate contracts. For example, a `Complex<i16>` buffer alone does not state
that its values use SC16 Q11 scaling. Transport packing also differs from the
host representation; see {ref}`rates-formats-and-limits`.

In `soapysdr` 0.5.1,
[`RxStream<E>`](https://github.com/kevinmehall/rust-soapysdr/blob/92211fe904f6683f57278cda0bd71344bc43fb9a/src/device.rs#L1308-L1452)
checks channel-buffer counts and limits a read to the shortest destination slice.
Its `time_ns()` accessor exposes the last read's timestamp, while the read flags
are private with no public validity accessor in that implementation. A successful
read count alone therefore does not establish timestamp validity or uninterrupted
reception. Native-driver correctness and thread-safety assumptions also remain
part of the [wrapper's contract](https://github.com/kevinmehall/rust-soapysdr/blob/92211fe904f6683f57278cda0bd71344bc43fb9a/README.md).

Seify's
[stream interface and Soapy adapter](https://github.com/FutureSDR/seify/blob/b2a76fafa1303e88f0028f48f42a4fdfc74014af/src/impls/soapy.rs)
use `Complex32` and return counts without forwarding Soapy's timestamp accessor.
Check this boundary when native integer samples or timestamped continuity are
requirements. Its
[capability records](https://github.com/FutureSDR/seify/blob/b2a76fafa1303e88f0028f48f42a4fdfc74014af/src/capabilities.rs)
are useful for inspecting supported controls; they do not supply omitted stream
metadata.

For each selected wrapper, check configuration and readback, buffer layout,
short reads, timeout/overflow reporting, timestamp validity, cancellation and
stream teardown. Keep unsupported operations, unknown capabilities and failed
queries distinguishable. Record actual native-library, plugin, firmware and
gateware versions when diagnosing an installation.

## Keep device facts separate from driver state

The [reference catalog](fact-catalog.md) describes source-qualified facts and
their applicability. A runtime capability query describes what an opened stack
reports. Requested configuration, actual readback and measured behavior are
additional records. Reusing one schema for all four loses their different
meanings.

Types can prevent accidental substitutions between RF frequency, sample rate,
filter bandwidth, sample count and byte count. RF frequency, sample rate and
bandwidth all have the physical dimension of inverse time, so dimensional
arithmetic alone does not enforce their radio roles. Explicit checked types can
also distinguish gain in dB, power in dBm, sample levels in dBFS and hardware
sample ticks from host durations.

The [uom 0.38.0 unit library](https://github.com/iliekturtles/uom/tree/a465bcc2b3bf67f9f12fc0b133d371762d9d8fe4)
supplies dimensional arithmetic and conversions. Device-specific bounds,
non-finite values, shared LO groups and valid operating combinations still need
explicit validation. Keep serializable reference/capability records separate
from live USB handles and stream ownership.

## Distinguish a driver from a DSP runtime

[FutureSDR 0.10.0](https://github.com/FutureSDR/FutureSDR/tree/42832f5473ec19ce641c58a4792befdc57945ff5)
provides asynchronous DSP flowgraphs, scheduling and buffers. Hardware support
follows its radio adapters. That version's
[manifest](https://github.com/FutureSDR/FutureSDR/blob/42832f5473ec19ce641c58a4792befdc57945ff5/Cargo.toml)
requires nightly Rust and declares Rust 1.98.1. Evaluate that runtime requirement
for a flowgraph application separately from a stable host driver adapter or
small reusable quantity library.
