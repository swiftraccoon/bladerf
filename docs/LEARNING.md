# Learning the bladeRF 2.0 micro xA9

Run the commands below from the workspace root.
The host application is the `tools/bladerf-lab` Cargo member. Its source, tests
and flowgraphs are colocated there; the virtual workspace root shares
the lockfile, toolchain and build output. Commands use `-p bladerf-lab` so the
intended package remains explicit when more experiments are added.
The source baseline is Nuand's
[2025.10 release](https://github.com/Nuand/bladeRF/releases/tag/2025.10).
Inspect a device's actual firmware, FPGA and host-library versions before
following a version-specific example. See the [setup guide](SETUP.md) for
prerequisites and checks.

## 1. Understand the hardware and software layers

The micro xA9 has an AD9361 RF transceiver, a Cyclone V E FPGA with 301 kLE,
and an FX3 USB controller. It has two receive and two transmit channels, up to
61.44 MS/s sampling and 56 MHz filtered bandwidth. The xA9's larger FPGA provides
space for custom DSP; the ordinary hosted image primarily transports samples
and implements control. Read Nuand's [product brief](https://www.nuand.com/bladerf-2-0-micro/)
and [schematics](https://www.nuand.com/bladeRF-2.0-micro-REV1.4.pdf).

The receive path is:

```text
RX SMA -> AD9361 filters/mixer/ADC -> FPGA -> FX3/USB -> libbladeRF
                                                        |
                                 Rust CLI / SoapySDR / GNU Radio
```

These layers have separate versions. FX3 firmware is USB-controller software;
an `.rbf` is FPGA gateware; libbladeRF is the host library. Quartus builds FPGA
gateware and is unnecessary for Rust receive experiments or GNU Radio.

The maintained host path is Rust `bladerf-lab` -> safe Rust SoapySDR bindings ->
native SoapySDR -> SoapyBladeRF -> libbladeRF. The upstream native components
retain their own languages. The [setup guide](SETUP.md) explains native
prerequisites; [CONTRIBUTING.md](../CONTRIBUTING.md) covers changes to the host
implementation.

## 2. Learn the CLI with a short receive capture

Connect a suitable antenna to **RX1** for an FM-broadcast experiment. Close
other programs using the same bladeRF first. Probe and inspect it:

```bash
bladeRF-cli -p
bladeRF-cli -e info -e version
bladeRF-cli -e 'help rx'
bladeRF-cli -e 'print frequency' -e 'print samplerate' -e 'print gain'
```

Start with 100 MHz and 2 MS/s; choose an actual local station later. This
captures exactly two million complex samples, approximately one second:

```bash
mkdir -p captures
bladeRF-cli \
  -e 'set frequency rx 100M' \
  -e 'set samplerate rx 2M' \
  -e 'set bandwidth rx 1.5M' \
  -e 'set agc rx1 off' \
  -e 'set gain rx1 20' \
  -e 'rx config channel=1 file=captures/first.sc16q11 format=bin n=2000000' \
  -e 'rx start' -e 'rx wait' -e rx
```

Use a fresh filename for each CLI capture: the CLI can overwrite files. A
successful single-channel capture has **8,000,000 bytes**. SC16Q11 stores
little-endian signed 16-bit `I,Q` pairs, with each component in
`[-2048, 2047]`; divide by 2048 to normalize. This format alone does not record
center frequency, sample rate, timing, or gaps. The workspace's
[Rust capture implementation](../tools/bladerf-lab/src/radio.rs) adds a JSON sidecar and refuses
existing output files:

```bash
cargo run --locked -p bladerf-lab --release -- capture --output captures/learning-rx1.sc16q11
cargo run --locked -p bladerf-lab --release -- analyze captures/learning-rx1.sc16q11
```

It checks that the bladeRF's native CS16 full-scale value is 2048 before treating
the data as SC16Q11. It records actual tuning, sample rate and gain together with
the receive API, format, count and host timestamp meaning.

Read [Nuand's operation guide](https://github.com/Nuand/bladeRF/wiki/Getting-Started%3A-Verifying-Basic-Device-Operation)
and the local [CLI help source](../repos/bladeRF/host/utilities/bladeRF-cli/src/cmd/doc/interactive-help.md).
Old examples for classic x40/x115 gain stages (`lna`, `rxvga1`, `rxvga2`) do
not match the AD9361's `full` gain stage.

## 3. Learn IQ and DSP from saved samples

Work through these experiments before increasing sample rate:

1. Plot `I`, `Q`, magnitude `abs(I + 1j*Q)`, and phase. Look for clipping near
   the SC16Q11 limits; compare manual gains of 10, 20 and 30 dB.
2. Apply a Hann window, compute an FFT, and use `fftshift`. For complex IQ,
   the displayed range is approximately `center_frequency +/- sample_rate/2`;
   FFT bin spacing is `sample_rate/N`. Window choice changes leakage and resolution.
3. Retune while keeping the sample rate fixed. Distinguish RF center frequency,
   sample rate, and analog filter bandwidth. They control different things.
4. Frequency-shift a station to baseband, low-pass filter it, then decimate.
   Filtering before decimation prevents aliasing. FM demodulation estimates
   phase change between adjacent complex samples; audio then needs filtering
   and resampling.
5. Repeat a capture with recorded settings. Compare relative power, noise floor
   and center-frequency DC artifacts. An uncalibrated FFT is not a dBm meter.

The [Rust analysis module](../tools/bladerf-lab/src/analysis.rs) provides a starting point. Run
`cargo run --locked -p bladerf-lab --release -- analyze --help` for FFT and input limits.
Keep raw captures and their metadata together. At SC16Q11, storage throughput
is `4 * sample_rate * receive_channels` bytes/second; start at 2 MS/s and measure
before pursuing the maximum rate.

## 4. Explore GNU Radio and FM reception

On a desktop with a graphical display and audio device, open the workspace copy
of Nuand's example (bias tees off, manual gain, stored oscillator trim preserved):

```bash
gnuradio-companion tools/bladerf-lab/examples/fm_receiver.grc
```

Inspect the bladeRF source, low-pass filter, resampler, FM demodulator, spectrum
display and audio sink. The example starts at 102.9 MHz; tune a station available
at your location. Follow each sample-rate change through the graph. Compare it
with the upstream version at `repos/gr-bladeRF/apps/fm_receiver.grc`.

A headless server can run a bounded GNU Radio receive check:

```bash
cargo run --locked -p bladerf-lab --release -- gnuradio-rx
```

The Rust command compiles the [finite GRC flowgraph](../tools/bladerf-lab/examples/gnuradio_rx.grc),
runs GNU Radio's generated program, and validates its exact CF32 sample count,
finite values and nonzero energy. A timeout stops and reaps the child process.
The generated program stays under ignored `build/bladerf-lab/gnuradio-rx/`;
GNU Radio's upstream source
block provides Python generation and native C++ execution. Application
orchestration and result validation are maintained in [Rust](../tools/bladerf-lab/src/host.rs).

The FM Qt/audio example requires a graphical session. Compiling its graph
is not a GUI/audio runtime test. See [gr-bladeRF's README](../repos/gr-bladeRF/README.md)
and [block definitions](../repos/gr-bladeRF/grc/).

## 5. Follow the Rust receive lifecycle and native boundary

Read [tools/bladerf-lab/src/radio.rs](../tools/bladerf-lab/src/radio.rs) for the complete Rust
validate/open/configure/activate/read/deactivate lifecycle. Follow the checked
frequency, sample-rate, duration and count types, bounded typed receive buffers,
actual-setting readbacks, explicit deactivation and partial-file cleanup.
`capture` stores CS16/SC16Q11; `soapy-rx` verifies a finite complex-float stream:

```bash
cargo run --locked -p bladerf-lab --release -- capture --channel 1 --duration 0.25 --output captures/rust-learning.sc16q11
cargo run --locked -p bladerf-lab --release -- soapy-rx --samples 200000
```

These commands require the configured native stack and an attached bladeRF.
Use the [libbladeRF API](https://www.nuand.com/bladeRF-doc/libbladeRF/v2.6.0/index.html),
[C header](../repos/bladeRF/host/libraries/libbladeRF/include/libbladeRF.h), and
[synchronous RX documentation](../repos/bladeRF/host/libraries/libbladeRF/doc/doxygen/sync_no_meta.dox)
to understand the driver's native contract. Safe Rust bindings keep native
ownership and FFI details in an upstream dependency; the binary forbids authored
unsafe code. Native calls are synchronous. The capture path explicitly
deactivates an active stream on success and recoverable errors. The wrapper
closes native handles on drop, but does not expose close errors through a
fallible explicit-close API.

Capture writes private temporary IQ and metadata files, then flushes and
synchronizes them before publishing the final paths. It preserves existing
files, including files created by another writer during the capture. The two
final paths cannot be published in a single rename: if publishing the sidecar
fails after IQ publication, the completed IQ file remains available for recovery
and the command reports an error.

SoapySDR offers a common API across radio brands. Compare its reported settings
with the native library:

```bash
SoapySDRUtil --find='driver=bladerf'
SoapySDRUtil --probe='driver=bladerf'
```

The [SoapyBladeRF documentation](https://github.com/pothosware/SoapyBladeRF/wiki)
explains discovery. Its local [streaming implementation](../repos/SoapyBladeRF/bladeRF_Streaming.cpp)
shows how the generic API maps to libbladeRF. Identify the API's sample format
and scaling before reusing a native SC16Q11 reader. Keep new DSP and host-tool
implementation in Rust; record evidence for any narrow native-language exception.

## 6. Advance to two receivers and hardware timestamps

The two receivers share the AD9361 receive LO: this is not a pair of independently
tunable radios. Begin with both observing the same band, then compare amplitude,
phase and cross-correlation. A shared clock does not remove antenna, cable or
RF-chain phase offsets; measurements still need calibration.

Study `BLADERF_RX_X2` and the channel-interleaved buffer layout in the C header
before recording both channels. Two receiver streams require twice the USB and
storage bandwidth at the same per-channel sample rate. For continuity and timed
acquisition, study `BLADERF_FORMAT_SC16_Q11_META`, metadata status, actual sample
counts and the [timestamped RX example](../repos/bladeRF/host/libraries/libbladeRF/doc/examples/sync_rx_meta.c).
Hardware sample timestamps are sample counters, not UTC; a host file creation
time is not a hardware timestamp. Measure overruns and timestamp discontinuities
as throughput increases.

## 7. Study the FPGA when host DSP is familiar

Quartus is an optional, large vendor toolchain. Cyclone V is supported by
Quartus **Lite** and **Standard**; select
the Cyclone V device package. Quartus Pro targets other device families, as shown
in [Altera's support matrix](https://www.altera.com/developers/software/device-support).

Choose a version to match the source checkout. The pinned `2025.10`
[HDL README](../repos/bladeRF/hdl/README.md) recommends **Lite 20.1.1**;
[Altera's Linux download](https://www.altera.com/downloads/fpga-development-tools/quartus-prime-lite-edition-design-software-version-20-1-1-linux)
includes Cyclone V support. The current upstream
[FPGA development wiki](https://github.com/Nuand/bladeRF/wiki/FPGA-Development)
and master README recommend **23.1.1** instead. Device support and project/IP
compatibility are separate questions: newer versions can change Nios II tools
or IP generation, so follow the instructions for the checked-out revision.
The RF observatory's build workflow selects **23.1.1** and begins with the
unmodified pinned design as its baseline.
The Nios II compiler's presence does not establish availability of the required
`altera_nios2_gen2` processor generator. The
[channel-bank build guide](../projects/rf-observatory/hdl/README.md#quartus-baseline-and-custom-image-build)
explains the 25.1 catalog incompatibility and baseline procedure.

Start with the [micro hosted top level](../repos/bladeRF/hdl/fpga/platforms/bladerf-micro/vhdl/bladerf-hosted.vhd),
[receive path](../repos/bladeRF/hdl/fpga/platforms/bladerf-micro/vhdl/rx.vhd),
and [Quartus build script](../repos/bladeRF/hdl/quartus/build_bladerf.sh).
Learn clock domains, FIFOs, fixed-point arithmetic, simulation, timing constraints
and the Nios control software. Reproduce a hosted build before adding a small
receive-side processing block in a separate revision. Keep the known-good
hosted image available for recovery.

The local [wiki clone](../repos/bladeRF-wiki/) includes architecture,
troubleshooting and FPGA development pages. Nuand's
[FPGA images](https://www.nuand.com/fpga_images/) and
[firmware images](https://www.nuand.com/fx3_images/) provide release checksums.
Use firmware and FPGA as a compatible set: the pinned
[micro compatibility table](../repos/bladeRF/host/libraries/libbladeRF/src/board/bladerf2/compatibility.c)
pairs FX3 2.6.0 with FPGA 0.16.0. Working receive experiments do not require
flashing new firmware or rebuilding gateware.
