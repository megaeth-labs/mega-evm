# AGENTS.md

## Overview

This crate exports versioned system-contract bytecode and typed ABI bindings.
Cargo builds use the checked-in Rust constants.
When the JSON artifacts are present, the build script checks their self-hashes, verifies that each latest alias matches a versioned artifact on version and hash, and compares the artifacts with the generated Rust sources.
These checks are read-only.
Published crates ship the constants and ABI interfaces.

## Structure

- `build.rs` runs the read-only consistency check during Cargo builds.
- `artifact_checks.rs` shares artifact validation and Rust source rendering between the build script and the developer tool in `tools/system-contracts/`.
- `src/lib.rs` includes the generated constants and uses `alloy_sol_types::sol!` for the ABI bindings.
- `contracts/` contains the Solidity implementations and `contracts/interfaces/` contains the ABI sources used by the Rust bindings.
- `artifacts/` contains versioned bytecode JSON artifacts and `*-latest.json` aliases.
- `scripts/` contains Foundry deployment scripts used to attest the latest artifacts.

## Updating system contracts

Preserve historical artifacts for spec traceability.
For an intentional new version, add `artifacts/<Name>-X.Y.Z.json` and update `artifacts/<Name>-latest.json` with the corresponding spec change.
Run `mise run system-contracts:attest` with Foundry v1.7.1 and initialized submodules to check that the Solidity deployments reproduce the latest checked-in artifacts.
Run `mise run system-contracts:generate` to regenerate `src/generated/` from the artifacts.
The generator emits the artifacts in semantic version order as `Vx_y_z_*` constants with `LATEST_*` aliases.
The required `lint` CI job runs attestation with the pinned Foundry version and checks that the Rust constants match the artifacts.

For a new system contract, register it in `artifact_checks.rs`, add a `scripts/<Name>Bytecode.s.sol:Save<Name>Bytecode` deployment script, and wire the generated include and ABI binding in `src/lib.rs`.
Keep interface paths stable because the Rust bindings reference them directly.
