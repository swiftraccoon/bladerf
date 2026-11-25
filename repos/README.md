# Upstream repositories

This directory contains the pinned Git submodules for the radio stack,
decoder engines, and upstream reference material. Initialize them from the
repository root.

```console
git submodule update --init --recursive
```

Use the [checkout inventory](../docs/workspace-guide.md#sources-in-this-workspace) to find each
submodule's role and selected revision. Git records the exact pins, and
[`.gitmodules`](../.gitmodules) lists the upstream URLs. The
[patch index](../patches/README.md) points to each consumer's patch and build
instructions.
