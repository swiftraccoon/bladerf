# Source-qualified reference records

[catalog.json](catalog.json) is the canonical, language-neutral dataset for the
[generated fact reference](../handbook/fact-catalog.md).
[catalog.schema.json](catalog.schema.json) defines its structural contract.
These are reference records, not live device capabilities or an operational
configuration validator. The catalog uses schema version 2; this repository
format may evolve independently of device APIs.

Each claim has a stable ID and property, a typed value, scope, conditions, evidence,
and a review record. Numeric values use explicit units and signed 64-bit JSON
integers. Integral floating-point spellings such as `1.0` are rejected by the
Rust consistency layer. Text and boolean facts have explicit value kinds.
Duplicate JSON object keys are rejected before schema validation, including
escaped spellings of the same key; fields cannot silently overwrite earlier ones.
An empty PCB/software scope means **not asserted**, rather than all versions.

Source records identify exact inspected bytes by SHA256. Source-code URLs pin
their declared Git commit. PDF locators identify a physical page counted from
one and its printed label; these numbers can differ. A section or symbol locator
is not a claim that software has automatically verified that source's meaning.
Web hashes identify retrieved snapshots, not immutable content at a permanent URL.

Evidence kind, extraction state, review method and experimental observations
are different concepts. The catalog includes source-checked claims with
agent-assisted review, and no hardware observations. It does not promote
unreviewed extraction candidates into facts or claim independent human review.

Use issue records to retain source disagreements and coverage gaps. A conflicted
claim requires an open source-disagreement issue with two or more declared
sources. A resolved issue cannot justify leaving a claim conflicted indefinitely.
Neither a typical transmit output nor a chip-pin rating should be substituted
for a missing board-connector input rating.

To update the reference:

1. Identify the source revision and bytes; inspect the relevant page or code.
2. Add or correct the source and claim, keeping units, conditions and scope.
3. Record unresolved alternatives or coverage gaps, with their source references.
4. Run the [Rust checker](../../tools/bladerf-docs/README.md), regenerate the page,
   and run its drift check plus the strict handbook build.
5. Review the data and generated diff before submitting the change.

Every source reference here uses a public URL. The generated Markdown is committed
so ordinary handbook rendering needs neither vendor PDFs nor the Rust checker.
Complete validation still runs the checker to detect inconsistent data or a
stale reading edition.
