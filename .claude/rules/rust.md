# Rust Coding Conventions

## `no_std` in `mega-evm` crate

Do not use `std::` directly.
Follow the existing pattern: `#[cfg(not(feature = "std"))] use alloc as std;` then `use std::{vec::Vec, ...};`.
Use `core::` for items like `fmt`, `cell`, `convert`.

## Dependency management

- `cargo sort` is enforced in CI.
  Dependencies in `Cargo.toml` must follow the grouped-by-family convention with comment headers (`# alloy`, `# revm`, `# megaeth`, `# misc`) and be sorted alphabetically within each group.
- Use `default-features = false` for new workspace dependencies.
  Features are opted-in explicitly.

## Compiler checks

- Use `cargo check` (not `cargo clippy`) for quick compiler error checking.
- Use `cargo clippy` only when specifically checking lint warnings.

## Markdown

- One sentence, one line.
  When writing markdown or similar format files, put each sentence in a separate line.
