# Contributing

This workspace contains bladeRF tools and a source-backed micro xA9 handbook.
Use the [setup guide](docs/SETUP.md) for native dependencies and the
[handbook build guide](docs/BUILDING.md) for documentation changes.

## Workspace and toolchain

Use the Rust toolchain pinned in [rust-toolchain.toml](rust-toolchain.toml),
edition 2024, and the shared [Cargo.lock](Cargo.lock). The
[build requirements](docs/RUST-TOOLCHAIN.md) describe features and native libraries.
Run Cargo commands from the repository root, selecting a package with `-p` or
the whole workspace with `--workspace`.

- `tools/<name>/` contains repository tools and their source, tests and examples.
- `projects/<name>/` is for independent applications and experiments.
- `crates/<name>/` is for libraries shared by actual consumers.

Register new packages in the root workspace. Keep `tools/bladerf-lab` as the
default member. Preserve upstream checkouts in `repos/` and keep necessary vendor
changes identifiable in `patches/`.

## Rust changes

The [coding standard](docs/coding-standard.md),
[Rust API checklist](docs/rust-api-checklist.md) and
[pinned standards sources](docs/standards-sources.json) apply to all contributors.

Use Rust for maintained implementation code. A different language needs a
specific technical requirement and evidence that it is a better fit. Existing
upstream dependencies and their generated outputs retain their own languages.

Follow the [Rust Style Guide](https://doc.rust-lang.org/stable/style-guide/) and
the applicable [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
Use `rustfmt` with the repository configuration. Keep device access separate
from reusable parsing, calculations and DSP. Express units, sample formats and
ownership clearly; propagate input/device errors with context. Document blocking,
shutdown and partial-output behavior where callers need to understand it.

Run checks relevant to the change. With native dependencies installed, the
workspace checks are:

```sh
cargo fmt --all -- --check
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
cargo doc --locked --workspace --all-features --no-deps
```

Ordinary tests leave the opt-in hardware test ignored. Use meaningful tests for
observable behavior and failure paths, and distinguish host checks from device
or RF validation. Keep configuration, raw captures and hardware-test results in
local ignored files.

## Documentation and reference data

Write for someone using or contributing to the project without access to a
particular lab. Keep setup instructions portable and explain each tool's inputs,
outputs, failure behavior and limits. Repository standards and reusable review
checklists are shared contributor documentation. Personal work plans, unprocessed
research logs, completed internal reviews and installation records stay local.

Support hardware claims with public primary sources and precise revisions or
locations. Distinguish chip specifications, assembled-board properties, software
behavior and measured observations. Follow the
[catalog contract](docs/knowledge/README.md) when editing reusable facts, then run:

```sh
cargo run --locked -p bladerf-docs -- check
cargo run --locked -p bladerf-docs -- render
cargo run --locked -p bladerf-docs -- render --check
```

Review the data and generated diff, then build the handbook with strict warnings
using [BUILDING.md](docs/BUILDING.md). Shared links must resolve to tracked files
or public sources. Keep downloaded vendor documents and generated builds out of
Git, and retain upstream attribution when adapting examples.

## Commits

Use [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/),
under the [repository commit standard](docs/commit-standard.md),
for example `fix(capture): reject incomplete sample buffers`. Group a coherent
change with its relevant documentation and validation. Describe breaking changes
with `!` in the subject and a `BREAKING CHANGE:` footer explaining migration.
Review staged files for unintended data before committing. Git records editorial
history; omit research/review date stamps from reader documentation.
