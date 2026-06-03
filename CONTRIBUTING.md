# Contributing to Bris

See `readme.org` for the project overview and `plan.org` for the
development roadmap. Each task in `plan.org` is sized as a meaningful
unit of work; pick one, open a draft PR early, and iterate.

## Local checks

```
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
```

CI runs the same checks plus a cross-build for `aarch64-unknown-linux-gnu`
(Pi Zero 2W class). All four must pass.

## Code of conduct

This project follows the [Contributor Covenant 2.1](CODE_OF_CONDUCT.md).
By participating you agree to abide by its terms.

## License

Contributions are accepted under GPL-3.0-or-later (the project license).
By opening a pull request you agree to license your contribution under
those terms.
