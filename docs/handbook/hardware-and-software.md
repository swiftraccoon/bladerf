(hardware-and-software)=
# Identify the hardware and software layers

**Purpose:** understand what “bladeRF 2.0 micro xA9” identifies, and which layer
to investigate when behavior changes. This page explains the architecture;
it is not a connector pinout or a firmware-upgrade procedure.

## Follow the signal through the board

```{image} images/radio-architecture.svg
:alt: RX and TX connectors connect to the AD9361 RF transceiver, which exchanges digital IQ and control with the Cyclone V FPGA; the FPGA connects through the FX3 USB controller to host software.
:width: 100%
```

On receive, the AD9361 handles analog RF conversion and filtering, and produces
digital I/Q samples. The FPGA implements the board's digital interfaces and
gateware functions. The FX3 provides the USB-controller side of the connection
to the host. Transmit follows the corresponding path in the opposite direction.
The ordinary hosted FPGA image is part of that data/control path; the FPGA can
also implement application-specific processing when its design is changed.
The drawing is conceptual and omits matching networks, switches, power,
clocking, and other board circuitry. See Nuand's
[micro product description](https://www.nuand.com/bladerf-2-0-micro/) and
[pinned HDL guide](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/README.md).

This division gives a useful troubleshooting order. A USB enumeration failure
belongs earlier in the connection than a host FFT. A host process can enumerate
a device before its FPGA is usable. A working sample stream can still contain
an RF tuning or gain mistake. Verify each layer's state rather than treating
“the radio works” as a single binary property.

## Separate model, FPGA size, and PCB revision

| Identifier | What it establishes | What still needs separate evidence |
| --- | --- | --- |
| bladeRF **2.0 micro** | The board family, based on AD9361 and Cyclone V | PCB assembly/revision and installed firmware/gateware |
| **xA9** | The larger FPGA variant in this family | Which logic the loaded FPGA image implements |
| **PCB revision** | Which schematic and board details apply | Whether optional parts are populated on an individual assembly |
| **FPGA image version** | The loaded gateware's version | Host-library compatibility and application feature support |

Nuand publishes separate micro schematic sets for
[REV 1.3](https://www.nuand.com/bladeRF-2.0-micro-REV1.3.pdf),
[REV 1.4](https://www.nuand.com/bladeRF-2.0-micro-REV1.4.pdf), and
[REV 1.6](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf).
Use the revision appropriate to the board being examined. The xA9 designation
does not select one of those schematic revisions.

The pinned micro build configuration selects `5CEBA9F23C8` for xA9. Nuand's
product page describes this variant as a 301 kLE Cyclone V FPGA. That is device
capacity; application capacity depends on the existing gateware and resource
utilization. See the
[platform configuration](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/fpga/platforms/bladerf-micro/build/platform.conf)
and [xA9 product page](https://www.nuand.com/product/bladerf-xa9/).

## Keep the software versions distinct

| Component | Responsibility |
| --- | --- |
| FX3 firmware | USB-controller execution and device communication |
| FPGA gateware | Programmable logic and its associated embedded functions |
| libbladeRF | Native host-side device control and sample streaming |
| bladeRF-cli | Interactive and scripted access to the native library |
| SoapyBladeRF | Maps SoapySDR operations to libbladeRF |
| An application | Capture, signal processing, presentation, and experiment behavior |

A native-library upgrade does not itself establish that a different image is
loaded into the FPGA or written into device flash. Likewise, changing an
application's Rust dependencies does not update the native C library it uses.
Record these layers independently when reporting an issue.

Nuand's [2025.10 release](https://github.com/Nuand/bladeRF/releases/tag/2025.10)
groups libbladeRF 2.6.0, bladeRF-cli 1.10.0, FX3 firmware 2.6.0, and FPGA
0.16.0. Treat this as a named release set, not an assertion that those versions
will always be the latest or that arbitrary combinations are equivalent.
The matching source includes
[micro compatibility tables](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/compatibility.c).

## Two receive channels are not two independent receivers

RX1 and RX2 provide two receive signal paths, but they share the receive local
oscillator. TX1 and TX2 similarly share the transmit local oscillator. RX and TX
have distinct RF synthesizers. Two RX channels therefore do not provide two
arbitrarily separated analog tuning frequencies at the same time.

This relationship is visible in the AD9361 architecture and in Nuand's
[RFIC implementation](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/rfic_host.c#L440-L500),
which handles the channels sharing an LO together. The
[AD9361 reference manual, UG-570 Rev. A](https://www.analog.com/media/en/technical-documentation/user-guides/AD9361_Reference_Manual_UG-570.pdf#page=16)
describes the receive and transmit RF synthesizers separately.

Software may frequency-shift and filter several signals within the captured
bandwidth after digitization. That is a different operation from independently
tuning the two analog receive paths. A configuration model should represent
shared controls explicitly rather than promising per-channel independence
merely because an API accepts a channel argument.

## Choose an authority for the question

- For a connector or circuit connection, use the matching board schematic and
  assembly evidence.
- For a component's electrical limits, use that component's datasheet, including
  its conditions. A chip-pin rating is not automatically a board-SMA rating.
- For settings accepted by a particular library release, inspect that release's
  API, implementation, and capability queries.
- For actual performance, retain a measurement with its host, settings, versions,
  setup, and uncertainty.

These sources complement each other. A runtime capability query does not prove
RF performance, and a datasheet cannot describe every behavior of later host
software. Continue with {ref}`rates-formats-and-limits` for examples of the
distinction.
