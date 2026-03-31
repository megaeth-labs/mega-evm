---
description: Generic call-interception mechanism for MegaETH system contracts — frame-init hook, selector matching, intercepted vs. fall-through semantics, gas treatment, and call-scheme rules.
spec: Rex3
---

# Call Interception

This page specifies the generic mechanism by which MegaETH intercepts calls to system-contract addresses at the EVM level.
It defines when interception fires, how selectors are matched, what happens on match or mismatch, and the gas rules that apply to all intercepted calls.

Each system-contract page defines its own interception policy: which address is intercepted, which function selectors are handled, which call schemes are eligible, and any function-specific preconditions or gas charges.

## Motivation

Some system-contract functions require protocol-level side effects that cannot be expressed by ordinary EVM bytecode alone.
Examples include forwarding data to an external backend, executing logic inside a sandboxed EVM instance, or querying EVM-internal accounting state that is not accessible to contract bytecode.

Rather than introducing new opcodes or precompiles for each such function, MegaETH intercepts ordinary call operations targeting known system-contract addresses and matching known function selectors.
This allows contracts to invoke protocol-level behavior through standard Solidity call syntax while keeping the system-contract ABI stable.

## Specification

### Interception Point

Interception fires during call-frame initialization, after the call opcode has been executed but before a child call frame is created.
At this point the opcode-level gas accounting (including [gas forwarding](../evm/gas-forwarding.md) cap and new-account storage-gas charges) has already been applied.

A system contract MAY intercept `CALL` or `STATICCALL`, but MUST NOT intercept `DELEGATECALL`, `CALLCODE`, `CREATE`, or `CREATE2`.

### Matching

Each interceptor MUST check the following, in order:

1. **Address**: Compare the call's target address against the interceptor's system-contract address.
   If the address does not match, the interceptor MUST fall through.
2. **Selector**: Decode the first four bytes of the call input as a function selector.
   If the selector matches one of the interceptor's handled functions, the interceptor MUST handle the call as specified on the corresponding system-contract page.
   If the selector does not match any handled function, the interceptor MUST fall through.

### Interception Outcomes

An interceptor MUST produce one of two outcomes:

- **Intercepted**: The interceptor returns a synthetic call result directly.
  The system contract's on-chain bytecode does not execute.
- **Fall-through**: The interceptor produces no result.
  Normal child-frame execution proceeds and the system contract's on-chain bytecode executes.

### Fall-Through to Bytecode

If a call targeting a system-contract address is not intercepted, normal frame initialization MUST proceed and the system contract's deployed bytecode MUST execute.
The fall-through behavior is defined by each system contract's on-chain bytecode.

### Gas Semantics

The call opcode's own gas costs (including the [gas forwarding](../evm/gas-forwarding.md) cap adjustment) MUST be charged before interception fires.
These costs are not refunded.

By default, an intercepted call consumes zero gas from the forwarded gas limit.
Each system contract MAY define additional gas consumption for its intercepted functions.

<details>
<summary>Rex4 (unstable): Storage-call stipend</summary>

An intercepted call MUST NOT receive a storage-call stipend.
The stipend is only applicable on fall-through.

</details>

## Spec History

- [Rex2](../upgrades/rex2.md) introduced the call-interception mechanism.
