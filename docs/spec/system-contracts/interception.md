---
description: Generic call-interception mechanism for MegaETH system contracts — frame-init hook, selector matching, intercepted vs. fall-through semantics, gas treatment, and call-scheme rules.
spec: Rex4
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

An intercepted call MUST NOT receive a [storage gas stipend](../glossary.md#storage-gas-stipend).
The stipend is only applicable on fall-through.

## Rationale

**Why intercept at frame initialization rather than call dispatch?**
Interception fires after the call opcode has executed and gas forwarding has been applied, but before a child frame is created.
This ensures that opcode-level gas accounting (including the gas forwarding cap and new-account storage-gas charges) is already settled.
Intercepting earlier (at opcode decode) would require reimplementing gas accounting inside each interceptor; intercepting later (inside the child frame) would require creating and then discarding a frame, wasting resources.

**Why exclude DELEGATECALL and CALLCODE?**
DELEGATECALL and CALLCODE execute the target's code in the caller's context — `msg.sender`, `msg.value`, and storage all belong to the caller, not the target.
Intercepting these call schemes would mean the interceptor runs with the caller's identity and state, which is inconsistent with the system contract's intended semantics.
Excluding them keeps interception limited to schemes where the system contract's address is both the target and the execution context.

## Security Considerations

**Unknown selectors MUST fall through to on-chain bytecode.**
If an interceptor silently consumed calls with unrecognized selectors, it could mask contract bugs or produce unexpected silent success.
The fall-through requirement ensures that unrecognized calls execute the system contract's deployed bytecode, which reverts with a stable custom error (e.g., `NotIntercepted()`).

**Intercepted calls do not receive a storage gas stipend.**
The stipend is designed for value-transferring calls that create a real child frame.
Intercepted calls produce a synthetic result without a child frame, so the stipend mechanism does not apply to them.
Any leftover stipend gas is burned on frame return rather than returned to the parent; this is a deliberate design choice, not a security constraint.

## Spec History

- [Rex2](../upgrades/rex2.md) introduced the call-interception mechanism.
