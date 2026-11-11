# Summary

<!-- What changed and why? -->

## Verification

There is no hosted CI; run these locally (see `CONTRIBUTING.md`).

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo clippy --locked --all-targets --features screenshot -- -D warnings`
- [ ] `cargo clippy --locked --all-targets --features os-notifications -- -D warnings`
- [ ] `cargo clippy --locked --lib --features fuzzing -- -D warnings`
- [ ] `cargo clippy --locked --all-targets --features ui-snapshots -- -D warnings` (macOS only)
- [ ] `cargo test --locked --no-fail-fast`
- [ ] `cargo build --locked --release`
- [ ] Visual change: ran `cargo test --features ui-snapshots snapshot_tests` on macOS and updated the goldens in `tests/snapshots/sections/`; each golden stays under 400 KB
- [ ] Touched `fuzz/`, `src/fuzz.rs`, `src/integrations/` or a feature's `persistence.rs`: the fuzz targets still build and run

## Screenshots

<!-- Include only when a visual behavior changed. Do not add generated history. -->
