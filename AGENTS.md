# Repository instructions

Read [CONTRIBUTING.md](CONTRIBUTING.md) and follow the shared
[coding standard](docs/coding-standard.md),
[commit standard](docs/commit-standard.md), and
[Rust API checklist](docs/rust-api-checklist.md).

Use Rust for maintained implementation code. Another language needs a specific
technical requirement, alternatives and evidence that it is a better fit.
Upstream dependencies retain their languages; keep vendor changes narrow and
identifiable. Assembly requires measured benefits on the target. FPGA synthesis
may require HDL.

Use the pinned toolchain, shared lockfile and build profiles. The root is a
virtual workspace: put tools in `tools/<name>/`, independent applications in
`projects/<name>/`, and genuinely shared libraries in `crates/<name>/`. Register
actual packages and preserve `tools/bladerf-lab` as the default member.

Assess all 54 checklist entries for a new component; update affected decisions
for later changes. Keep completed working reviews local, and put durable API,
ownership, failure and shutdown contracts in source or public guides. Run checks
appropriate to the change, distinguishing host, simulated and hardware results.

Keep committed documentation useful to external users and contributors.
Repository standards and reusable checklists are shared requirements. Internal
plans, research logs and completed development reviews stay local and ignored.
Git records editorial history; omit research/review date stamps from documents.

Support hardware claims with public primary sources and precise revisions or
locations. Follow the [catalog contract](docs/knowledge/README.md), edit canonical
data before regenerating its reference, and run the catalog/drift checks and
strict handbook build when affected. Shared links must resolve to tracked files
or public sources.

Use portable paths in shared docs. Keep personal identifiers, host configuration,
connected-device state, antenna/wiring arrangements, captures and lab results in
ignored local files. `AGENTS.override.md` supplies checkout-specific context and
must remain ignored; this shared `AGENTS.md` must remain tracked.

Preserve intentional upstream modifications. Review staged content and relevant
verification before making Conventional Commits at meaningful checkpoints.
Releases, tags and pushes require their own task authorization.
