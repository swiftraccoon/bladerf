(clocks-and-synchronization)=
# Understand clock references and synchronization

**Purpose:** choose the right clocking concept and interpret its status without
confusing frequency discipline, sample alignment, and RF phase. This chapter
uses Nuand's micro REV1.4 and REV1.6 schematics, the named component documents,
and the `2025.10` implementation. It explains reference paths and status reporting;
a multiboard synchronization procedure requires its own implementation and
measurements.
Page links below refer to physical PDF pages, counted from one; printed page
numbers can differ. For example, the ADF4002 Table 1 is on physical page 4,
printed page 3.

## Three questions that need different answers

1. **How accurate is the timebase?** The onboard nominal **38.4 MHz VCTCXO**
   supplies a reference from which the board derives other clocks. A stored trim
   or an external discipline loop controls its frequency.
2. **Are devices using the same clock and aligned sample positions?** A common
   reference helps, but sample-clock alignment and the start of sample delivery
   need their own mechanisms.
3. **Is the relative RF phase known?** Sharing a reference does not by itself
   establish a repeatable RF phase between different radios.

The nominal frequency comes from the pinned
[micro constants](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/fpga_common/include/bladerf2_common.h#L43-L54).
AD9361's reference feeds its RF and baseband synthesizers; changing the sample
rate is therefore a different operation from replacing the board's 38.4 MHz
reference. See [UG-570 Rev. A, p. 14](https://www.farnell.com/datasheets/2007082.pdf#page=14)
and {ref}`rates-formats-and-limits`.

## Follow the reference and its control loop

```{image} images/clock-reference-paths.svg
:alt: REFIN enters the ADF4002 loop, whose filtered control voltage tunes the onboard VCTCXO instead of the trim DAC. The Si53304 selects that oscillator or CLKIN, distributes the selected clock, and supplies feedback to the ADF4002. CLKOUT is one separately enabled buffer output.
:width: 100%
```

This original conceptual drawing follows **sheet 13** of
[REV1.4](https://www.nuand.com/bladeRF-2.0-micro-REV1.4.pdf#page=13) and
[REV1.6](https://www.nuand.com/bladeRF-2.0-micro-REV1.6.pdf#page=13).
It omits supply circuitry, optional population choices, terminations and detailed
pin connections; it is not a wiring diagram or a claim about an unidentified
physical PCB revision.

The **AD5621 trim DAC, U23**, supplies an adjustable voltage to the oscillator's
tuning input. AD5621 is the 12-bit member of its family; do not mistake the
AD5611's 10-bit resolution for this schematic's part. See the
[ADI family datasheet, Rev. G, p. 1](https://www.farnell.com/datasheets/1780742.pdf#page=1).
A stored setting is a calibration value, not a continuously measured frequency.

The **ADF4002, U26**, provides a second way to control that tuning voltage. It
compares divided versions of an external reference and the distributed board
clock; its charge pump and loop filter steer the VCTCXO. The
[ADF4002 description](https://www.farnell.com/datasheets/2331696.pdf#page=9)
explains the divider/phase-detector loop. In the micro implementation, enabling
this PLL places the trim DAC in its disabled, high-impedance mode; disabling the
PLL restores DAC control. The two are alternative actuators, not independent
corrections to add together. See
[trim-DAC handling](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L2630-L2691)
and [PLL enable](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L3411-L3444).

The **Si53304, U94**, selects `VCTCXO_OUT` or `UFL_CLK_IN` and distributes that
source to the board. Its `CLK_SEL` input chooses CLK0 when low and CLK1 when
high; see the [buffer datasheet, pp. 45–47](https://www.skyworksinc.com/-/media/Skyworks/SL/documents/public/data-sheets/si5330x-datasheet.pdf#page=45).
The schematic's `ADF_RFIN_A` feedback comes from a selected-clock buffer output.
Consequently, when CLKIN replaces the oscillator in the distributed path,
tuning the onboard VCTCXO does not tune that external generator. Treat REFIN
discipline and direct CLKIN selection as different configurations.

## Identify the three clock ports

| Port in REV1.4/REV1.6 sheet 13 | Role | Frequency and electrical qualification |
| --- | --- | --- |
| **REFIN, J95** | Input to the ADF4002 discipline loop | Default expected reference is 10 MHz. The pinned setter's declared range is 5–300 MHz, with divider feasibility and input conditions still required. |
| **CLKIN, J93** | Direct alternative input to the Si53304 board-clock buffer | Nuand specifies **38.4 MHz, 3.3 V CMOS**. It is not the 10 MHz reference input. |
| **CLKOUT, J92** | Enabled/disabled output from that buffer | Nuand specifies a nominal **38.4 MHz, 3.3 V CMOS** output for other devices. |

Nuand's [clocking FAQ](https://www.nuand.com/frequently-asked-questions/)
describes these roles. It calls CLKOUT “U92”; both cited schematic revisions
label the connector **J92**, which is the designator used here. Verify the actual
board revision and connector identification before planning an interconnection;
see [the connector atlas](connectors.md).

REFIN is AC-coupled through C93 on these sheets. Nuand's
[micro reference guidance](https://github.com/Nuand/bladeRF/wiki/Taming-Your-VCTCXO#reference-oscillators)
allows a square wave or sinusoid and specifies **no more than 3.3 V peak-to-peak**
for this input. This is a voltage condition, not a generic dBm rating into an
assumed load. Do not transfer the classic board's 1.8 V reference-interface
description to these micro ports.

The software range is not a complete electrical specification:

- [ADF4002 Rev. D, Table 1, p. 4](https://www.farnell.com/datasheets/2331696.pdf#page=4)
  specifies REFIN input sensitivity of 0.8 Vpp through VDD under its stated
  conditions. Its 20–300 MHz entry requires slew rate **greater than 50 V/µs**
  when operating below 20 MHz. A low-frequency waveform must satisfy that
  condition too; a valid number of hertz alone is insufficient.
- Direct CLKIN also clocks the USB-controller path. The
  [FX3 datasheet, Rev. *Z, Table 4, p. 19](https://community.infineon.com/gfawx74859/attachments/gfawx74859/KnowledgeBaseArticles/651/1/Infineon-CYUSB301X_CYUSB201X_EZ-USB_FX3_SUPERSPEED_USB_CONTROLLER-DataSheet-v21_00-EN.pdf#page=19)
  specifies a maximum input-frequency deviation of **150 ppm**, alongside
  phase-noise, duty-cycle and edge requirements. At 38.4 MHz, 150 ppm is
  **5.76 kHz** by arithmetic, not a measured tolerance margin of this board.

The clock buffer's component-level ability to switch between different input
frequencies does not make arbitrary board clock frequencies supported. Its
“glitchless” switching also includes a short output pause; it is not a guarantee
of uninterrupted acquisition. See [Si5330x, p. 10](https://www.skyworksinc.com/-/media/Skyworks/SL/documents/public/data-sheets/si5330x-datasheet.pdf#page=10).

## Interpret the controls as separate state

| CLI parameter | Native API | What a query establishes |
| --- | --- | --- |
| `clock_sel` | `bladerf_get_clock_select` | Whether the buffer selects the onboard oscillator or external CLKIN |
| `clock_ref` | `bladerf_get_pll_enable`, `bladerf_get_pll_lock_state` | Whether the ADF4002 loop is enabled and its lock-detect state |
| `refin_freq` | `bladerf_get_pll_refclk` | Configured reference frequency inferred from the programmed dividers |
| `clock_out` | `bladerf_get_clock_output` | Whether CLKOUT is enabled |
| `trimdac` | `bladerf_get_vctcxo_trim`, `bladerf_dac_read` | Library-reported stored trim or fallback, plus the cached DAC word; not measured oscillator frequency |

The [micro-specific API](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/bladeRF2.h#L257-L478)
and [CLI query implementation](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/utilities/bladeRF-cli/src/cmd/printset_hardware.c#L228-L467)
define these interfaces. `refin_freq` is not an input-frequency measurement: the
[getter](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L3458-L3504)
calculates from divider values assuming the nominal 38.4 MHz system clock. A
lock indication likewise does not measure absolute frequency accuracy or prove
that two radios have aligned sample indices.

The Nios [DAC and PLL register accessors](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/hdl/fpga/platforms/common/bladerf/software/bladeRF_nios/src/devices.c#L734-L789)
return cached words for the write-only DAC/ADF4002 interfaces. These queries
report software-maintained state, rather than reading the components' physical
registers. The library's [stored-trim loader](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L2704-L2725)
can also use a fallback when the stored calibration value cannot be obtained.

The classic `vctcxo_tamer` interface, including its 1-PPS mode, is a different
mechanism: the pinned micro
[tamer implementation returns unsupported](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L2613-L2627).
A GPS-disciplined frequency output and a GPS 1-PPS timing pulse are different
signals; neither the API names nor the word “GPS” makes them interchangeable.

## Separate frequency lock, sample alignment and RF phase

| Property | What it supplies | What remains separate |
| --- | --- | --- |
| VCTCXO discipline through REFIN | A local oscillator controlled relative to a reference | Matching sample-start positions or a shared timestamp epoch |
| Common clock through CLKIN | A shared source for the selected board timebases | Baseband divider alignment, stream start and RF-path phase |
| AD9361 multichip synchronization | A chip-level procedure aligning baseband clocks | Synchronization of RF local oscillators |
| FPGA sample trigger | A condition gating sample delivery | Frequency discipline, UTC time or calibrated RF phase |
| Sample metadata timestamp | A counter associated with sample positions | An automatic UTC/PPS epoch or measured phase relationship |

[UG-570 Rev. A, pp. 87–89](https://www.farnell.com/datasheets/2007082.pdf#page=87)
requires a common reference plus a specific synchronization procedure for
multiple AD9361 baseband clock chains. It explicitly excludes RF-LO
synchronization from that feature. This chip-level procedure is not evidence
that arbitrary micro hardware/gateware already exposes a complete multiboard
synchronization workflow.

Within one AD9361, the two RX paths share an RX LO, and the two TX paths share a
TX LO; RX and TX have distinct synthesizers. Across boards, establish and measure
the relative RF phase and path delays required by the application. A shared
reference alone does not supply that calibration. See
{ref}`hardware-and-software` for the shared-control distinction.

The native [trigger contract](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/libbladeRF.h#L1442-L1485)
describes sample gating under common-clock/rate assumptions and names micro
**J51 pin 1** as the trigger signal. Its example also contains classic SMB clock
instructions: those do not replace the micro clock controls above. Trigger
wiring, arming and shutdown need their own verified procedure.

The [metadata timestamp](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/libbladeRF.h#L2493-L2506)
is a free-running FPGA counter advancing at the associated sample rate. The
[timestamp getter documentation](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/include/libbladeRF.h#L2660-L2676)
distinguishes a coarse host query from metadata attached to samples. Connecting
a frequency reference does not convert that counter into UTC or align counters
on different devices.

## Inspect an existing idle CLI session

These query commands contain no clock-setting, flash, trigger-arming or streaming
operation. They are examples to run in an **already-open idle session**, not a
claim that opening a device is free of initialization writes. The pinned
[initialization path](https://github.com/Nuand/bladeRF/blob/fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47/host/libraries/libbladeRF/src/board/bladerf2/bladerf2.c#L182-L212)
can restore trim and configure PLL dividers when a device is initialized.

```text
print clock_sel
print clock_ref
print refin_freq
print clock_out
print trimdac
```

Interpret the output in order:

1. Identify the selected source; an enabled discipline loop is not the same
   setting as an external buffer input.
2. Record PLL enable and lock separately. Neither reports the external signal's
   measured amplitude, waveform quality or absolute accuracy.
3. `print refin_freq` emits no frequency when the loop is disabled in this CLI
   implementation. Missing output is not a measurement of zero hertz.
4. Record output enable and trim state as reported. A trim number alone is not
   a calibrated frequency result.

When characterizing synchronization, record the board revisions,
clock-source topology, supplied waveforms and levels, configured/read-back
values, software/gateware versions, sample rates, trigger method, timestamp
interpretation, and the alignment/phase measurements actually performed. Until
those measurements exist, describe a source-selected or lock-detected state
rather than claiming a synchronized measurement system.
