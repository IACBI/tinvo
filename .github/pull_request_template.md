**What this changes and why**

**How it was tested**
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test` and `cargo test --release`
- [ ] `tinvo bench --check` in release mode if the engine changed
- [ ] `tinvo fuzz <target>` for a few minutes if a reader or the edit model changed
- [ ] Ran on real hardware (say which OS) if audio or terminal code changed

**Checklist**
- [ ] No new dependencies
- [ ] Nothing new allocates, locks or blocks on the audio thread
- [ ] Old `.tvo` files still load (if the format changed)
- [ ] `CHANGELOG.md` updated under `[Unreleased]`
