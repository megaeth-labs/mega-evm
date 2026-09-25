# AGENTS.md

## OVERVIEW
The six system contracts: their addresses and bytecode, the interceptor dispatch that answers calls to four of them, the system-address transaction, and the pre-block deploy of those six plus the EIP-7997 factory.

## STRUCTURE
- `oracle.rs`: the Oracle's address, code and ABI, and the `sendHint` side effect. Its storage is read by the Host (`evm/host.rs`), through the oracle environment.
- `timestamp.rs`: the High-Precision Timestamp wrapper's address and code. No interceptor.
- `keyless/`: native keyless deployment. The `KeylessDeploy` address, code and ABI and the semantics of a deployment (`mod.rs`); the dispatch, the overhead, the nine rules, the charges of the creation's start and the rewrite into a creation (`dispatch.rs`); the settlement of the creation into its call and the ABI answer (`settle.rs`); the pre-EIP-155 transaction format and the error ABI (`tx.rs`, `error.rs`).
- `control.rs`: `MegaAccessControl`'s address, code, ABI and revert payloads, `SLOT_NUM_ACCESS_TYPE`, and its interceptor, which steers gas detention's switch.
- `limit_control.rs`: `MegaLimitControl`'s address, code, ABI and its interceptor, which answers the compute the calling frame could still spend.
- `sequencer_registry.rs`: the `SequencerRegistry`'s address, code and ABI, and the [`SequencerRegistryConfig`] that seeds it. No interceptor.
- `deploy.rs`: the declarative spec, `transact_deploy`, the EIP-7997 factory, and the list of seven predeploys.
- `intercept.rs`: the dispatch — the address test, the selector peek, the value policy and the shape of an answer.
- `tx.rs`: the system-address transaction, its whitelist and the validation that precedes its promotion to a deposit.

## KEY PATTERNS
- The dispatch order is: the scheme guard (`MegaEvm::intercept`), the address, the selector, then the method's value policy. Each step is cheaper than the next; the address test is one comparison against the shared `0x6342…` prefix and runs on every call a transaction makes.
- `keylessDeploy` is not intercepted: a call a transaction makes is taken by the keyless rewrite, which runs before interception in `frame_init`, and between the inspector's `frame_start` of the call and that of the creation in `inspect_frame_init`, and becomes a native `CREATE` of its signer. Its test costs one comparison on every frame: the depth, then the address and the selector.
- A selector is admitted on its four bytes alone. Trailing bytes are accepted; an input shorter than four bytes is not a selector and is not admitted.
- An unknown selector is never intercepted: the call falls through to the deployed bytecode, and what that bytecode answers is the contract's own. The two control contracts revert with `NotIntercepted()` from their fallback; `KeylessDeploy` has no fallback, so a selector it does not declare reverts with empty data while a `keylessDeploy` call a contract makes reaches the method body's `NotIntercepted()`; the Oracle runs its other methods, and reverts with empty data on a selector it does not declare.
- `CALL` and `STATICCALL` reach the dispatch. `CALLCODE` and `DELEGATECALL` run the callee's code in the caller's context, so they are refused by the scheme guard before any interceptor.
- A method that takes no value answers a value-bearing call with `NonZeroTransfer()`, or with the error its own ABI names (`KeylessDeploy` answers `NoEtherTransfer()`). The policy is per method and is applied after the selector matched, so a value-bearing call to an unknown selector still falls through.
- An answer is a `synthetic_call_result`: the forwarded gas untouched, the caller's reservoir carried, the calling opcode's upfront state-gas flags kept. Never `Gas::new(limit)`, which carries no reservoir and bills the sender for the whole state-gas pool.
- An interceptor that lets the frame run charges nothing: the frame starts as it was built.
- A keyless deployment's call is a frame no code runs in. It has the lane a depth-0 call pushes, running as the signer, a journal checkpoint, and its own gas (`KeylessCall` on the context), which gas detention holds as a running frame's before the call charges anything; it pays what the `CREATE` opcode charges its frame and starts the creation as its child at depth 1, kept by detention as the frame suspended on it. The creation's result is settled into it in `MegaHandler::last_frame_result`, which every first-frame result reaches, before the call's lane is popped; the inspected path settles it in `inspect_execution` just before, and tells the inspector the call ended.
- What stands after a keyless deployment is what its call keeps: a call that succeeds — the deployment deployed, or failed and reported it — keeps what the creation's start wrote, a signer's nonce spent from 0 to 1, its account and its record; a call that reverts with a stop takes the whole deployment back through its checkpoint. A creation that never started (one an inspector answered) gives the signer's account back. A deployment from nonce 1 keeps no bump, whether it deployed or failed: the settlement sets the nonce back and takes the record and its history back, so no deployment takes a signer past nonce 1, nobody can use up its attempts, and a resubmission of one that deployed is refused `ContractAlreadyExists()`, as in the legacy engine. It takes back the creation's own bump alone, the one just above the nonce the call started from: nonces a delegated signer's code spent in the constructor stay spent.
- The Oracle's `sendHint` is a side effect, not an answer: it forwards the hint and returns `None`, so the contract's own bytecode runs. It forwards only for a call that could deliver it — no value, some gas, a calling frame whose volatile-data access is on — and the payload is counted on the transaction's data size before it is decoded; a hint that is not forwarded is not counted.
- An interceptor that acts for its caller takes the caller as the frame one level above the frame the call would start (`depth - 1`). A transaction that calls a system contract directly has no calling frame: `MegaAccessControl` then switches nothing off, enables without refusal and answers `false`, and `remainingComputeGas()` answers the transaction's own frame's regular gas, or detention's cap when it is detained from its start (its sender is the block beneficiary).
- `remainingComputeGas()` is read when the call reaches the dispatch, after the calling opcode charged its own costs: the lesser of the caller's regular gas with the forward counted back (`MegaEvm::intercept` passes what the caller has left; the answer returns the forward untouched) and gas detention's allowance at the call. That is the caller's spendable regular gas before the forward, which detention caps. This departs from the legacy engine's figure, which came from a separate compute ledger, with per-frame budgets of 98/100 of the caller's remaining compute under the transaction's compute limit, so it could exceed the caller's gas; here compute is regular gas, and the answer is at most the caller's own. The one property carried over is that forwarded gas is not counted.
- `VolatileDataAccessDisabled`'s argument is a `uint8` on the wire. A refused `SLOTNUM` names 12 (`SLOT_NUM_ACCESS_TYPE`), past the contract's enum, so Solidity handlers decode the argument as `uint8`; the contracts are not changed for it, and their code hashes stay.
- The system-address transaction is validated before it is promoted to a deposit, because the deposit path validates nothing. The whitelist, the chain id, the nonce and EIP-3607 are checked there, each under the configuration switch a user transaction obeys.
- Accounts read during validation are read without warming them, so the transaction pays what any other transaction would pay for its first touch.

## PRE-BLOCK STATE CHANGE CONTRACT

Pre-block helpers in this module participate in `apply_pre_execution_changes` on `MegaBlockExecutor`.
The contract for a new or modified helper:

- Never call `db.commit(...)` inside the helper.
  Return the prepared state; let the executor commit.
- On an idempotent no-change path (the contract is already deployed with the correct code hash), return `EvmState` that carries the observed account as a read-only entry: neither `touched` nor `created`.
  Returning nothing here is a bug — the account disappears from the stateless witness read set.
- On a real-change path, include every account and storage slot the helper touched.
- Present with different code is an error, not an overwrite.
  A system address holding foreign bytecode is a broken chain.

The returned `EvmState` is both the commit and the witness record of that step.
The executor hands it to the pre-block observer, then commits: the sequence the observer sees is the witness a stateless client needs.

## ANTI-PATTERNS
- Do not answer an unknown selector with a synthetic revert: the on-chain bytecode is the fall-through, and what it answers is the contract's own business.
- Do not materialise calldata before the address and the selector matched. `peek_selector` borrows four bytes; `CallInput::bytes` copies the whole payload.
- Do not build an answer from a bare gas limit. Use `synthetic_call_result`, which carries the reservoir and the upfront-charge flags.
- Do not charge a keyless deployment's start out of the creation's gas, and do not translate its result anywhere but the settlement: the call pays the start as a `CREATE`'s frame does, and only `last_frame_result` (and `inspect_execution` before it) sees every first-frame result, the creation answered at its start included.
- Do not turn a failed read or SALT lookup of a keyless deployment into an ABI error: it fails the transaction with its cause, as it does at every other site.
- Do not add a length check to a selector match. Admission is the four bytes.
- Do not accept value on a read-only or control method without saying why in the interceptor and pinning it with a test.
- Do not deploy system bytecode from a literal in this crate; the `mega-system-contracts` crate ships the code and its hash. The timestamp wrapper is the one exception, because the Oracle's address is baked into its code.
- Do not read the system address from a constant in new code once the `SequencerRegistry` is read: the address can be rotated.
- Do not call `db.commit(...)` inside a helper that participates in `apply_pre_execution_changes`.
  It hides the step from a witness generator.
- Do not return nothing from a helper on a "no change needed" path.
  Return the account entries observed, even when nothing is written.
- Do not overwrite foreign code at a system address.

## WHERE TO LOOK
- Add a method to an intercepted contract: the contract's own module, next to the selectors it already answers, and a boundary test per `tests/system/`.
- Change what a call to a system contract costs: `intercept.rs` for the dispatch, the contract's module for what its own method charges.
- Change a keyless deployment: `keyless/dispatch.rs` for what is dispatched, the rules and the charges of the creation's start; `keyless/settle.rs` for the settlement and the answer; `evm/execution.rs` for where the two are called; `tests/system/keyless/` for the tests.
- Change the system transaction's validation: `tx.rs::validate_and_promote`, which `MegaHandler::validate_env` calls.
- Change what a deposit-like transaction pays for the account it creates: `evm/execution.rs`, where the charge is made in the pre-execution phase.
- Add a predeploy: a spec in `deploy.rs::system_contract_specs`, seeded from chain params if it has storage.
- Change how a block deploys the contracts: `block/executor.rs::apply_pre_execution_changes` iterates the spec list, delivers each witness to the pre-block observer, and commits.
- Read the rotated system address, or run `applyPendingChanges`: the pre-block system calls own those.
