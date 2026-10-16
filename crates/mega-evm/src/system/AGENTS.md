# AGENTS.md

## OVERVIEW
The six system contracts: their addresses and bytecode, the interceptor dispatch that answers calls to four of them, and the system-address transaction.

## STRUCTURE
- `oracle.rs`: the Oracle's address, code and ABI, and the `sendHint` side effect.
- `timestamp.rs`: the High-Precision Timestamp wrapper's address and code. No interceptor.
- `keyless/`: the `KeylessDeploy` address, code and ABI (`mod.rs`), the dispatch of `keylessDeploy` (`dispatch.rs`), and the data-only helpers of the pre-EIP-155 transaction (`tx.rs`, `error.rs`).
- `control.rs`: `MegaAccessControl`'s address, code, ABI and revert payloads, and its interceptor.
- `limit_control.rs`: `MegaLimitControl`'s address, code, ABI and its interceptor.
- `sequencer_registry.rs`: the `SequencerRegistry`'s address, code and ABI. No interceptor.
- `intercept.rs`: the dispatch — the address test, the selector peek, the value policy and the shape of an answer.
- `tx.rs`: the system-address transaction, its whitelist and the validation that precedes its promotion to a deposit.

## KEY PATTERNS
- The dispatch order is: the scheme guard (`MegaEvm::intercept`), the address, the selector, then the method's value policy. Each step is cheaper than the next; the address test is one comparison against the shared `0x6342…` prefix and runs on every call a transaction makes.
- A selector is admitted on its four bytes alone. Trailing bytes are accepted; an input shorter than four bytes is not a selector and is not admitted.
- An unknown selector is never intercepted: the call falls through to the deployed bytecode, and what that bytecode answers is the contract's own. The two control contracts revert with `NotIntercepted()` from their fallback; `KeylessDeploy` has no fallback, so a selector it does not declare reverts with empty data while a `keylessDeploy` call reaches the method body's `NotIntercepted()`; the Oracle runs its other methods, and reverts with empty data on a selector it does not declare.
- `CALL` and `STATICCALL` reach the dispatch. `CALLCODE` and `DELEGATECALL` run the callee's code in the caller's context, so they are refused by the scheme guard before any interceptor.
- A method that takes no value answers a value-bearing call with `NonZeroTransfer()`, or with the error its own ABI names (`KeylessDeploy` answers `NoEtherTransfer()`). The policy is per method and is applied after the selector matched, so a value-bearing call to an unknown selector still falls through.
- An answer is a `synthetic_call_result`: the forwarded gas untouched, the caller's reservoir carried, the calling opcode's upfront state-gas flags kept. Never `Gas::new(limit)`, which carries no reservoir and bills the sender for the whole state-gas pool.
- An interceptor that lets the frame run may charge it instead of answering it, by taking gas off the frame's limit (`KeylessDeploy`'s fixed overhead). That is why the dispatch takes the call inputs mutably.
- The Oracle's `sendHint` is a side effect, not an answer: it forwards the hint and returns `None`, so the contract's own bytecode runs. It forwards only for a call that could deliver it — no value, some gas — and the payload is counted on the transaction's data size before it is decoded.
- The system-address transaction is validated before it is promoted to a deposit, because the deposit path validates nothing. The whitelist, the chain id, the nonce and EIP-3607 are checked there, each under the configuration switch a user transaction obeys.
- Accounts read during validation are read without warming them, so the transaction pays what any other transaction would pay for its first touch.

## ANTI-PATTERNS
- Do not answer an unknown selector with a synthetic revert: the on-chain bytecode is the fall-through, and what it answers is the contract's own business.
- Do not materialise calldata before the address and the selector matched. `peek_selector` borrows four bytes; `CallInput::bytes` copies the whole payload.
- Do not build an answer from a bare gas limit. Use `synthetic_call_result`, which carries the reservoir and the upfront-charge flags.
- Do not add a length check to a selector match. Admission is the four bytes.
- Do not accept value on a read-only or control method without saying why in the interceptor and pinning it with a test.
- Do not deploy system bytecode from a literal in this crate; the `mega-system-contracts` crate ships the code and its hash. The timestamp wrapper is the one exception, because the Oracle's address is baked into its code.
- Do not read the system address from a constant in new code once the `SequencerRegistry` is read: the address can be rotated.

## WHERE TO LOOK
- Add a method to an intercepted contract: the contract's own module, next to the selectors it already answers, and a boundary test per `tests/system/`.
- Change what a call to a system contract costs: `intercept.rs` for the dispatch, the contract's module for what its own method charges.
- Change the system transaction's validation: `tx.rs::validate_and_promote`, which `MegaHandler::validate_env` calls.
- Change what a deposit-like transaction pays for the account it creates: `evm/execution.rs`, where the charge is made in the pre-execution phase.
- Deploy the contracts at a fork, read the rotated system address, or run the pre-block calls: not here yet — system contract deployment and the pre-block system calls own those.
