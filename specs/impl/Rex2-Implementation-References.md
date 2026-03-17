# Rex2 Implementation References

This document is informative.
Normative semantics are defined in [Rex2 Specification](../Rex2.md).
If this mapping conflicts with the normative spec text, the normative spec wins.

## Scope

This document maps each Rex2 spec change and invariant to implementation.
It is intended for code navigation and auditing.

## Change Mapping

### 1. SELFDESTRUCT re-enabled (EIP-6780)

Spec clauses:
- SELFDESTRUCT is a valid opcode.
- Same-transaction contracts: full destruction (code, storage, balance transfer).
- Non-same-transaction contracts: balance transfer only.

Implementation:
- [crates/mega-evm/src/evm/instructions.rs](../../crates/mega-evm/src/evm/instructions.rs) (`rex2::instruction_table` — maps SELFDESTRUCT to `compute_gas_ext::selfdestruct`, lines 225–246)
- [crates/mega-evm/src/evm/state.rs](../../crates/mega-evm/src/evm/state.rs) (`merge_evm_state_optional_status` — handles EIP-6780 same-transaction check during state merge)

### 2. KeylessDeploy system contract

Spec clauses:
- System contract deployed at `0x6342000000000000000000000000000000000003`.
- Provides `keylessDeploy(bytes, uint256)` returning `(uint64, address, bytes)`.
- Interception only at depth 0.
- Non-depth-0 calls fall through to on-chain bytecode with `NotIntercepted()`.
- Value-bearing calls revert.
- Fixed overhead of 100,000 gas.

Implementation:
- [crates/mega-evm/src/system/keyless_deploy.rs](../../crates/mega-evm/src/system/keyless_deploy.rs) (contract address, code, deployment function)
- [crates/mega-evm/src/system/intercept.rs](../../crates/mega-evm/src/system/intercept.rs) (`KeylessDeployInterceptor` — depth==0 check, ABI decoding, activation gated by `MegaSpecId::REX2`)
- [crates/mega-evm/src/evm/execution.rs](../../crates/mega-evm/src/evm/execution.rs) (`frame_init` — system contract dispatch, passes `depth` to interceptor)
- [crates/mega-evm/src/sandbox/execution.rs](../../crates/mega-evm/src/sandbox/execution.rs) (`execute_keyless_deploy_call` — sandbox execution, gas overhead, state merge)
- [crates/mega-evm/src/constants.rs](../../crates/mega-evm/src/constants.rs) (`rex2::KEYLESS_DEPLOY_OVERHEAD_GAS` = 100,000)

## Invariant Mapping

- `I-1`: Stable Rex1 semantics unchanged.
  Rex2 reuses Rex1 constants and only adds SELFDESTRUCT to the instruction table and the KeylessDeploy interceptor.
- `I-2`: SELFDESTRUCT of non-same-tx contract does not delete code or storage.
  Implementation: EIP-6780 semantics in revm, wired via [crates/mega-evm/src/evm/state.rs](../../crates/mega-evm/src/evm/state.rs).
- `I-3`: KeylessDeploy interception only at depth 0.
  Implementation: [crates/mega-evm/src/system/intercept.rs](../../crates/mega-evm/src/system/intercept.rs) (`depth != 0` early return in `KeylessDeployInterceptor::intercept`).

## Maintenance Notes

Update this mapping when Rex2 semantics change.
Update this mapping when implementation locations move.
