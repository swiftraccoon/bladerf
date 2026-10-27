# Coding standard

This standard defines the Rust baseline for the bladeRF 2.0 micro xA9 workspace.
It governs code we author: host tools, DSP, capture and analysis workloads,
device adapters, shared libraries, build helpers, and any custom firmware.
Upstream bladeRF, GNU Radio, SoapySDR, and vendor code retain their own conventions;
keep our additions identifiable and avoid unrelated restyling.

## Adopted sources and authority

| Reference | Authority |
| --- | --- |
| [Rust Style Guide](https://doc.rust-lang.org/stable/style-guide/) | Source and manifest conventions. |
| [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) | Every applicable checklist entry, identified by its `C-*` identifier. |
| [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html) | Supplemental implementation and design guidance. |

The [source lock](standards-sources.json) records exact upstream revisions,
immutable source links, and file hashes. Source texts are not vendored. The
published links above provide convenient navigation; the locked revisions define
the review baseline. The [toolchain baseline](RUST-TOOLCHAIN.md) defines build
requirements; [CONTRIBUTING.md](../CONTRIBUTING.md) describes the contribution workflow.
Updating a toolchain does not update the adopted rules; review changed rules
and reconcile applicability records when adopting new revisions.

Precedence is: repository requirements, the two explicit authority assignments
above in their respective domains, then compatible Google guidance. The Google
guide's own precedence rule is for Google's codebases; it does not override the
repository's assignments here. For an actual conflict, cite both sections, retain the
explicit authority, and record the resolution. A hardware limitation needs an
explicit, narrow exception with evidence, not an unrecorded relaxation.

## Language and implementation boundaries

Use Rust for maintained code we author, including host-side automation,
capture, analysis, DSP, and shared libraries. Use existing tools for routine
research and inspection; this preference is not a reason to recreate shell
commands or upstream tooling in Rust.

Use another implementation language only when it is objectively better for the
specific requirement. Record the requirement, alternatives, evidence, and smallest
affected scope. Examples include HDL required for FPGA synthesis, an integration
that requires a native language, or assembly with a demonstrated efficiency gain
over optimized Rust on the actual target. Do not assume assembly is faster.
Evaluate existing Rust bindings and a small FFI adapter before concluding that
Rust integration is unsuitable. Scope an exception to the part that needs it
and explain the choice in the change description.

An upstream library implemented in C or C++ is a dependency, not a claim that our
application is entirely Rust. Authored Python and shell setup/learning tools must
follow the same rule; their creation date is not an exception. Running existing
external tools or unmodified upstream examples does not require translating them.

Keep reusable DSP, parsing, and calculation logic separate from USB/device
adapters, application entry points, transport, and board configuration. Preserve
behavior across adapters: sample formats and scaling, payloads, queue capacities,
validation, timeouts, and completion conditions.
Prefer a small direct adapter to a general framework without a concrete need.

## Formatting and tooling

Follow the official chapters for
[source layout](https://doc.rust-lang.org/stable/style-guide/),
[items and imports](https://doc.rust-lang.org/stable/style-guide/items.html),
[statements](https://doc.rust-lang.org/stable/style-guide/statements.html),
[expressions](https://doc.rust-lang.org/stable/style-guide/expressions.html),
[types](https://doc.rust-lang.org/stable/style-guide/types.html),
and [Cargo manifests](https://doc.rust-lang.org/stable/style-guide/cargo.html).
Use `rustfmt` as the mechanical formatter and review the parts it does not cover,
including manifests and documentation. Do not add nightly-only formatting options
solely to match a supplemental preference.

Select and record a compiler version, Rust edition, formatting edition, target
triple, components, and SDK/toolchain requirements before implementation. The
initial selections are in [RUST-TOOLCHAIN.md](RUST-TOOLCHAIN.md).
Pin tool versions for reproducible runs. The
[style-edition chapter](https://doc.rust-lang.org/stable/style-guide/editions.html)
must be read at the recorded revision; historical release-status wording is not
a current compiler-support matrix. The language edition, style edition, and
minimum supported Rust version (MSRV) are separate decisions. Do not invent an MSRV
before checking the chosen device bindings, OS, and other dependencies. The
manifest's supported compiler baseline is the selected and tested toolchain;
it does not claim that older compilers have been tested.

Prefer a supported stable compiler. If a dependency requires nightly or a vendor
toolchain, record the requirement, exact version, and affected targets. Scope
that requirement to the component that needs it. A shared host library should
not inherit firmware-only restrictions unnecessarily.

For each maintained application, commit its dependency lockfile and document
supported SDK, OS and tool requirements plus source/blob versions or hashes.
Keep individual installation inventories and lab configurations local. Set actual package
metadata; do not invent authors, licenses, or websites to satisfy a checklist.
Choose project licensing deliberately before distribution, and flag dependency
license constraints without changing upstream license terms.

## Commit messages and checkpoints

Use Conventional Commits 1.0.0 under the
[commit standard](commit-standard.md). Commit
meaningful completed work as it progresses, including the relevant documentation,
lockfiles and relevant validation summaries. That standard distinguishes upstream
syntax from repository choices and keeps release/version/tag/push actions scoped
to their separate authorization.

## API review and applicability

The upstream checklist contains **54 active entries**. The complete inventory and
review procedure are in [rust-api-checklist.md](rust-api-checklist.md). Every code
change needs an applicability review; every applicable entry needs evidence.
Reuse a crate's established applicability record and update affected decisions
instead of duplicating a long checklist in every small change. The shared
checklist defines the review requirements; completed working assessments stay
local. Document durable API contracts in source or shared guides and include
relevant verification summaries in the commit or change description.

Use four outcomes: **applies**, **not applicable** with a concrete reason,
**exception** with scope and evidence, or **pending**. Pending is not compliance.
Private application code does not need the public surface of a general-purpose
library, but privacy does not waive naming, safety, correctness, or any guideline
whose stated scope still applies. Apply the actual referenced section, including
its qualifications, rather than treating its short checklist title as the entire
rule.

Document public behavior and useful examples; link one realistic example from
related items where the guideline permits it. Hardware-only examples must state
their target, wiring, and execution requirements. Do not label an example tested
when it was only compiled. Keep implementation details private unless callers
need a representation commitment. Use types to distinguish cycles, durations,
bytes, sample counts, frequencies, radio modes, and result statuses where confusion
could affect correctness. Distinguish RF tuning, sample rate, and filter bandwidth.

## Embedded and host applicability

| Situation | Repository application |
| --- | --- |
| Bare-metal or embedded `no_std` component | State whether `alloc` is used and how memory is provided. Prefer bounded storage for measured paths. A `no_std` label alone does not prove absence of allocation. |
| Rust with an OS/SDK providing `std` | Record the exact target and `std` integration. Review thread, timer, I/O, heap, and failure behavior through that integration. |
| Host tooling | Use normal `std` facilities where appropriate; preserve target-independent parsing and calculation logic for host tests. |
| Reusable records and configuration | Evaluate `C-SERDE`; an optional `serde` feature can preserve a small firmware dependency graph. Hardware handles and live peripheral ownership are not interchange records. |
| Public error type | Preserve the intent of `C-GOOD-ERR`: meaningful diagnostics and error composition. Use `core::error::Error` when supported by the selected compiler; use a documented `std`-gated implementation or narrow MSRV exception when necessary. Record this adaptation. |
| Reader/writer abstraction | Apply `C-RW-VALUE` to the actual trait contract. Do not import `std::io` into a `no_std` component merely to mirror a source example. Verify whether the embedded I/O trait supports the same borrowing pattern. |
| Peripheral/resource handle | Consider common traits individually. Do not derive `Copy`, `Clone`, `Default`, or sharing traits when doing so would duplicate ownership or make an invalid device state constructible. |
| Fixed-capacity collection | Evaluate `C-COLLECT` against explicit overflow semantics. Document a narrow exception if an infallible collection trait would hide overflow; expose checked operations. |

These are mappings and review questions, not blanket exemptions. The final
choice belongs in the component's applicability record with its target and
compiler evidence.

## Safety and concurrency

Use maintained safe device and OS interfaces where they satisfy the requirement.
Keep raw register access, unsafe initialization, FFI, and interrupt glue isolated
from workload algorithms. For every unsafe operation or unsafe trait
implementation, explain the invariant and why this call site preserves it.
Document the caller's obligations on public unsafe interfaces. Performance alone
is not evidence for unsafe code; measure the safe alternative if performance is
the justification.

For FFI, document ABI, layouts, ownership, pointer validity, alignment, lifetimes,
callback context, teardown ordering, and error conversion. A safe wrapper must
prevent callers from violating the foreign contract. Validate foreign integer
statuses before constructing Rust enum values. State the panic strategy and
ensure a panic cannot unwind across an incompatible boundary. Prefer generated
bindings with recorded input headers and generation versions over handwritten
copies; keep generated files identifiable.

Treat USB callbacks, streaming workers, host threads, interrupts, and FPGA/FX3
execution as distinct contexts. Do not infer synchronization safety from a
single application thread. Check the selected target's atomic support and runtime
critical-section implementation; document ordering choices and shared-memory
assumptions. Volatile accesses do not provide a locking or synchronization
protocol. Review reentrancy, interrupt masking, nested contexts, DMA ownership,
and `Send`/`Sync` behavior for the actual adapter.

Keep interrupt handlers bounded. State which operations may allocate, block,
sleep, take locks, or run only from a task. Review priority inversion and lock
ordering. Do not hold a blocking lock or interrupt-disabled critical section
across `.await`. Async APIs must state cancellation behavior and what happens to
an in-progress transfer, borrowed buffer, or partially recorded sample when a
future is dropped. Do not add async without a concrete concurrency or I/O need.

Make resource shutdown explicit when it can fail or block, so destructors remain
predictable (`C-DTOR-FAIL`, `C-DTOR-BLOCK`). Test the relevant ownership and
failure paths when they are introduced.

## Diagnostics and measurement behavior

Represent unsupported, timed out, skipped, failed, and passed as distinct
outcomes. A missing feature produces no numeric score; it is never silently
recorded as zero. Preserve raw error codes and useful context at adapter
boundaries. Use `Result` or `Option` according to the contract; avoid panic as
ordinary device, connection, capacity, or input-error handling. Explain the rare
`expect` used for a proved invariant. Tests may assert expected behavior.

Choose and record the application's panic behavior and, for custom firmware,
its reset behavior. A host process recovering from a device reboot does not mean
the capture or experiment survived it. Keep recovery outside the measured region
and preserve the evidence of failure.

Record logging transport, level, buffering, dropped events, and whether logging
was disabled during timing. Avoid formatting, serial output, heap growth, and
host round trips inside measured regions unless those operations are the
workload. Bound queues and define what happens when they fill. Do not expose
credentials or other secrets in logs, diagnostic formatting, or committed
configuration.

Use a consistent workload definition with specified inputs, seed when relevant,
warmup, repetitions, synchronization points, and completion validation. Keep
timer source, units, wrap handling, resolution, conversion, and observer overhead
explicit. Variable radio conditions remain variable even when a random seed is
fixed. Label deterministic workload inputs separately from environmental
repeatability.

For an experiment's local evidence, record build profile, optimization, LTO, clock/power settings, memory placement,
enabled OS services, task/executor configuration, and workload hash. Record the
bladeRF model, firmware, FPGA image, USB link speed, RF settings, sample format,
antenna or cabled setup, and any overruns or discontinuities. Disclose toolchain
and implementation differences when comparing drivers or DSP paths. Preserve raw
samples and outliers. Each experiment records its own aggregation and comparison
methods; successful untimestamped streaming does not prove sample continuity.

## Verification and review evidence

Run the relevant formatter, compiler checks, lints, documentation checks, and
meaningful tests for supported target/feature combinations. At implementation
time, record the exact commands; a generic `cargo test` instruction does not
establish an embedded test strategy. Use host tests for pure workload logic,
parsers, conversions, result validation, and analysis. Use on-device checks for
interrupt timing, scheduling, peripheral behavior, power transitions, and radio
interactions. Compilation and emulation are useful evidence with narrower scope.

Review compiler and Clippy warnings. Scope a needed lint suppression to its
actual cause and document the reason. Do not impose a global warning denial
without checking the pinned toolchain and generated/vendor code. Do not add
tests that simply repeat implementation details; choose checks capable of
finding a meaningful failure.

Maintain one current local API/safety record per component. It identifies execution
contexts, applicable `C-*` decisions, unsafe/FFI and blocking contracts, meaningful
regression checks, and exceptions. Update that record when the contract changes;
record routine commands/results and unrun checks in the commit or CI record.
Carry lasting findings from one-off investigations into the public contract or
test, then retire redundant process notes. An exception records
the rule/source, reason, smallest affected scope, alternatives considered,
consequence, and a revisit trigger. Propose changes to the repository's language
policy or assigned authorities explicitly in the change description.

## Google guidance compatibility review

Apply the supplemental guide using this mapping. It is not a claim that a
component review has already been completed.

| Topic | Resolution here |
| --- | --- |
| Google precedence | Repository-assigned authorities prevail. |
| Chromium layout, imports, async, threading | Product-specific; inapplicable here. |
| Fuchsia mutex/testing | Product-specific; inapplicable here. |
| Extra formatting options | Stable tools suffice; official conventions govern. |
| Trait implementation scope | Retain `C-COMMON-TRAITS`. |
| Lifetime/generic naming | Retain official/API conventions where recommendations differ. |
| Constructors and examples | Retain `C-CTOR`/`C-EXAMPLE`, including their qualifications. |
| Pre-`main` restriction | Allow required, explicit firmware startup. |

Relevant sections:
[API context](https://google.github.io/styleguide/rustguide.html#api-design-context),
[unsafe](https://google.github.io/styleguide/rustguide.html#unsafe),
[initialization](https://google.github.io/styleguide/rustguide.html#global-initialization).
