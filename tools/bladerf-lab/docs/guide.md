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
