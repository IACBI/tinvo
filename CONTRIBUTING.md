# Contributing to TINVO

Thanks for helping. These rules keep TINVO small, fast and correct.

Author: 𝓐.𝓒.𝓑

## Ground rules

- **No third-party crates.** Use only `std` and direct FFI to operating-system APIs. A pull request that adds a dependency will not be merged. Write the piece you need, with tests.
- **The audio thread is sacred.** Code reachable from `Engine::process` must not allocate, lock, block, do I/O or panic on valid input. Size everything when the engine is built.
- **Every document change is an `Edit`.** New features that change the project go through `Project::apply`, need an inverse in `Project::inverse`, and must round-trip through the `.tvo` format.
- **Do not break old files.** When the file format changes, bump `FORMAT_VERSION` and keep reading older versions. Add new instrument parameters at the end of the list.
- **Prove it.** New behaviour comes with tests that would fail without the change. DSP claims (pitch, timing, frequency response) are measured in tests, not assumed.

## Before you open a pull request

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release
```

If you changed the project format, the WAV reader or the edit model, run `tinvo fuzz project`, `tinvo fuzz wav` or `tinvo fuzz edits` for a few minutes (each runs 60 s by default; `--seconds N` for longer). A failing input is saved, and `tinvo fuzz TARGET --check FILE` replays it.

If you changed an audio backend, run `tinvo probe` on that platform and say in the pull request what you tested on. Clippy for other platforms can be run with `--target x86_64-unknown-linux-gnu` or `--target aarch64-apple-darwin` once the target is installed with `rustup target add`.

## Style

- Comments explain *why*, not *what*. Public items get doc comments.
- Keep functions small enough to test on their own. The `App` state machine is unit-tested without a terminal; keep it that way.
- Update `CHANGELOG.md` under `[Unreleased]`, and the docs in `docs/` if the design changes.
