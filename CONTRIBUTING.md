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

`cargo test` includes `tests/realtime.rs`, which counts every allocation made while the engine renders, while it takes edits, live notes and samples, and while input and MIDI are read. It must stay at zero; on a failure it prints where the allocation happened.

If you changed anything the engine runs, run `tinvo bench --check` in release mode: it prints each scenario's cost per callback against its time budget. CI also counts the instructions each scenario takes with Valgrind's cachegrind, on x86-64 Linux, against `bench/instructions.txt`, and fails beyond 10 % over. If a change makes rendering costlier on purpose, run `tinvo bench --instructions bench/instructions.txt` there (the CI log shows the same output), copy the measured lines into the file, and say why in the pull request. The counts depend on the compiler, so a new stable Rust can move them too.

If you changed the project format, the WAV reader or the edit model, run `tinvo fuzz project`, `tinvo fuzz wav` or `tinvo fuzz edits` for a few minutes (each runs 60 s by default; `--seconds N` for longer). A failing input is saved, and `tinvo fuzz TARGET --check FILE` replays it.

A new editor key is an `Action` in `src/keymap.rs`, with its context and default keys, and a new colour is a `Role` in `src/theme.rs`, with a style for both built-in themes. The handlers and drawing use these, never a key code or a colour directly, so the settings file and the key list pick them up. The keymap's tests fail if a default key clashes with another in the same view.

If you changed an audio backend, run `tinvo probe` on that platform and say in the pull request what you tested on. Clippy for other platforms can be run with `--target x86_64-unknown-linux-gnu` or `--target aarch64-apple-darwin` once the target is installed with `rustup target add`.

## Style

- Comments explain *why*, not *what*. Public items get doc comments.
- Keep functions small enough to test on their own. The `App` state machine is unit-tested without a terminal; keep it that way.
- Update `CHANGELOG.md` under `[Unreleased]`, and the docs in `docs/` if the design changes.
