(handbook-home)=
# Understanding the bladeRF 2.0 micro xA9

The bladeRF 2.0 micro combines an RF transceiver, programmable FPGA, USB
controller, and host software. Learning where each part's responsibilities begin
makes its capabilities—and its limits—much easier to understand.

This handbook starts with the **micro xA9**. It identifies when a statement is
about the AD9361 chip, the assembled board, a PCB revision, a particular software
release, or an observation from an experiment. Those distinctions matter when
you configure the radio or reuse information in software.

## Choose a starting point

- **Identify the hardware and software layers:** read
  {ref}`hardware-and-software`.
- **Understand tuning, sample rates, bandwidth, and IQ representation:** read
  {ref}`rates-formats-and-limits`.
- **Identify a connector or expansion contact:** read {ref}`connectors`.
- **Distinguish external reference, direct clock input, and synchronization:** read
  {ref}`clocks-and-synchronization`.
- **Inspect reusable, source-qualified records:** read {ref}`fact-catalog`.
- **Choose bindings and preserve stream contracts in Rust:** read
  {ref}`rust-integration`.

```{toctree}
:maxdepth: 1
:hidden:
:caption: Handbook

hardware-and-software
rates-formats-and-limits
connectors
clocks-and-synchronization
fact-catalog
rust-integration
```

## How to read the evidence

Technical claims link to the manufacturer's documentation or a named upstream
source revision. Source code references use Nuand's `2025.10` release commit,
`fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47`, unless a page says otherwise.
A source inspection establishes what that document or implementation says;
it does not establish measured performance of every radio or host.

The published documents sometimes disagree or omit important conditions. This
handbook keeps those disagreements visible. A general marketing range, a
software-accepted range, and a guaranteed electrical characteristic answer
different questions.

The hardware chapters explain documented architecture, controls and interfaces.
Application-specific RF calibration and multiboard synchronization need
measurements and procedures appropriate to the system being built.
