# bladeRF lab tools

The `bladerf-lab` Rust CLI provides receive capture, IQ/PSD analysis, finite
SoapySDR/GNU Radio checks, device diagnostics, verified image downloads, and
Fedora native-stack setup. It is the default member of the root Cargo workspace.

From the repository root or `tools/bladerf-lab`:

```bash
cargo run --locked -p bladerf-lab -- --help
cargo run --locked -p bladerf-lab --no-default-features -- setup-fedora --dry-run
```

The CLI discovers the repository from the current directory and its ancestors.
Relative capture, build, download, and evidence paths resolve from the repository
root. From elsewhere, pass `--workspace /path/to/bladerf` to the CLI.

Source, integration tests and examples belong to this package. The root retains
the lockfile, build output, toolchain and native dependency checkouts. See the
[learning guide](../../../docs/LEARNING.md), [setup guide](../../../docs/SETUP.md), and
[contribution guide](../../../CONTRIBUTING.md).

The setup guide documents the optional `capture --require-fpga-version
MAJOR.MINOR.PATCH` pre-stream guard, including the SoapyBladeRF constructor's
sample-rate initialization boundary. The guard itself requests no image-load or
flash operation; it runs after ordinary device open.

Use `capture --discard-initial-samples N` to discard up to 20 million complex
samples from the active stream before saving the requested duration. A positive
discard is recorded as `discarded_initial_samples` in the sidecar. The host
timestamp precedes stream setup and carries no timing for the first saved sample.
The ordinary capture checks its acquisition deadline before and after each
read, including the final read. A late read fails before file publication.

## Persistent receive sweeps

The [RF observatory sweep](../../../projects/rf-observatory/docs/guide.md#sweep-a-frequency-range)
uses `capture-sweep` to run up to 500 strictly increasing integer-hertz centers
on one open stock-image receiver at 20 MSps. Each dwell can save up to ten
seconds of IQ. The native command accepts repeated `--center-hz` flags and
requires `--channel`, `--gain`, `--duration`, and an existing mode-0700 `--output`
directory. It creates `tune-0001`, `tune-0002`, and so on for each dwell's IQ
and ordinary metadata sidecar.

`--passes N` repeats that center list on the same open receiver, with up to
16 passes and 2,000 total dwells. For a repeated capture, tune directories
are numbered independently inside `pass-0001`, `pass-0002`, and so on. The
shared timing receipt lists dwell rows in pass order on one host clock.

An ordered sweep supplies `--visit-hz HZ` for every visit in one pass. The
order can repeat centers and must include every declared `--center-hz`.
Duration overrides apply each time the center is visited. Tune directories
and host timing rows follow visit order.

The receive stream is deactivated and closed between dwells. The pinned
driver disables RX at stream close and configures fresh receive buffers at
stream setup. Before reading each dwell, the receiver checks the tuning,
sample rate, bandwidth, gain and planned sample count. There is no extra sample
discard between dwells. Deactivation or close failure stops the sweep before
publishing that IQ pair. Retune gaps and radio settling can affect the captured
samples. [Pinned SoapyBladeRF stream implementation](../../../repos/SoapyBladeRF/bladeRF_Streaming.cpp)

The command stops at the first error or interrupt and preserves completed IQ
and sidecar pairs. It refuses to replace an existing tune directory.

On Linux GNU, use `--check-continuity --channel 1` with FPGA version 0.16.0
to require device timestamp adjacency at 20 MSps. The receiver reads an
adjacent block after the final saved sample before closing and publishing a
dwell. The `sample_timing` sidecar field records the first sample, exclusive
end and witness sample count. The times are in device-clock nanoseconds.

After a successful batch, `acquisition-timing.json` records nanosecond offsets
from one monotonic host clock started before device open. Each row marks tuning,
file preparation, stream setup, the host read loop, stream closure, and file
publication. Use the read-start offsets to measure host cadence.

## Timestamped segment capture

On Linux GNU, `capture-segments` records one stock FPGA 0.16.0 RX1 stream at
20 MSps. By default it tunes to 2437 MHz with 20 MHz bandwidth, discards the
first two million samples, and records four segments. Set `--bandwidth-hz`
to narrow the analog filter. The sidecar records the realized bandwidth.

```console
cargo run --locked --release -p bladerf-lab -- capture-segments \
  --segments 4 --output captures/rx1-segments
```

Each numbered directory contains 19,988,480 samples in `iq.sc16q11`, its
ordinary acquisition sidecar, and `segment.json`. The receipt records device
clock bounds, a checked read at the exclusive end, and both file digests.
Sample offsets start at zero after the discard. The device clock has no UTC
alignment. Enumerate numbered directories to consume atomically published
segments. A timing discontinuity, stream error, or full writer queue stops
acquisition and preserves earlier published segments. Before RX starts, capture
prepares up to three reusable host buffers of about 80 MB each by default.
`--writer-queue-depth N` permits 1–8 pending segments and prepares at most
`N + 2` buffers. It absorbs brief writer stalls within that RAM budget.

`--usb-buffer-samples N` sets the samples per USB transfer. The default is
32,768. Use multiples of 2,048 from 2,048 through 65,536. The receiver checks
the configured stream MTU before reading IQ. Larger buffers increase the
queued payload and memory use for the same 32 transfers.

`--spool-segments` takes a limit of 1 through 120 raw IQ files and allows up to
120 segments per run. Each slot holds about 80 MB. A cooperating consumer can
free slots by removing only surveyed IQ files. Capture stops before staging
another file if every slot is occupied.
The rolling monitor's `--retire-prefix` mode checks the durable `retired.json`
in the capture directory's parent and accepts up to 3,600 segments in finite runs.
With `--until-finished`, an empty `finish` file in that parent requests
a witnessed final segment and writer drain.
