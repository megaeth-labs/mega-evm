//! Gas detention: volatile-data access tracking, and the compute cap a read of volatile data sets.
//!
//! `MegaETH`'s parallel executor runs transactions side by side and has to redo the ones whose
//! reads another transaction's writes invalidate. Some data changes with every block or every
//! transaction — the block environment, the block beneficiary's account, the Oracle's storage —
//! so a transaction that reads it is likely to be redone, and a long computation after the read
//! is expensive to redo. Detention caps how much a transaction may still compute once it has read
//! such data.
//!
//! # What is volatile
//!
//! | Kind | Read by | Cap |
//! |---|---|---|
//! | the block environment | `NUMBER`, `TIMESTAMP`, `COINBASE`, `PREVRANDAO`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `SLOTNUM`, `BLOCKHASH` | [`block_env_access_compute_gas_limit`](crate::EvmTxRuntimeLimits::block_env_access_compute_gas_limit) |
//! | the block beneficiary's account | `BALANCE`, `SELFBALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, the four calls (and the EIP-7702 delegate they follow), `SELFDESTRUCT` as either end; a transaction whose sender or recipient is the beneficiary or whose recipient delegates to it, an applied EIP-7702 authority that is, and a keyless deployment's signer that is | [`block_env_access_compute_gas_limit`](crate::EvmTxRuntimeLimits::block_env_access_compute_gas_limit) |
//! | the Oracle's storage | `SLOAD` in the Oracle's own frame | [`oracle_access_compute_gas_limit`](crate::EvmTxRuntimeLimits::oracle_access_compute_gas_limit) |
//!
//! The caps are runtime limits ([`EvmTxRuntimeLimits`](crate::EvmTxRuntimeLimits)): the spec's,
//! [`BLOCK_ENV_ACCESS_COMPUTE_GAS`](crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS) and
//! [`ORACLE_ACCESS_COMPUTE_GAS`](crate::constants::ORACLE_ACCESS_COMPUTE_GAS), by default, and a
//! caller's limits may set either. A cap of `u64::MAX` caps nothing, and a transaction whose caps
//! are both unlimited — under [`no_limits`](crate::EvmTxRuntimeLimits::no_limits), as the
//! execution-spec gate runs — is not detained. `no_limits` turns detention off together with
//! every other per-transaction limit: it is for the gate's equivalence mode and for tests, not for
//! executing the chain.
//!
//! `BLOBHASH` is not on the list: it reads the transaction's own blob hashes, which nothing else
//! decides. A system-originated transaction and a system call are not detained, whatever they
//! read, the block environment included: they are the protocol maintaining its own state, the
//! Oracle's included, and the same rule exempts them from every per-transaction limit.
//!
//! # Where a read is marked
//!
//! Where the Host actually loads the value: its block-environment accessors, its account load
//! (which every account opcode and every call goes through, the EIP-7702 delegate's included), its
//! storage load and its `SELFDESTRUCT`. A load that fails — a cold account the frame cannot pay
//! for, a database error — marks nothing, and neither does an opcode that fails after the load:
//! the Host only observes, and the opcode's wrapper commits the read once the opcode completed,
//! as it does with a write record.
//!
//! # The cap
//!
//! Compute is the regular gas the transaction spends on what it runs, read off revm's `Gas`: state
//! and history gas that spilled onto regular gas are not compute, gas withheld from regular
//! charges is never spent, and what a halting frame burns is not compute either ([`Detention`]).
//! The one burn counted as compute is what a frame has left when an opcode's static gas fails,
//! which no wrapper sees: per halting frame, under the failed charge's price (at most 4,999, on
//! `SELFDESTRUCT`). Burned gas is counted as compute, so the stop comes earlier, never later.
//! A value call's stipend is not compute either: compute is regular gas drawn from the
//! transaction's own pools, and the stipend is gas nobody paid. A callee may run up to 2,300 gas
//! on each value call's stipend, and each value call costs its caller at least 9,100 of compute,
//! so the gas run after a read is at most about 25% more than the cap.
//!
//! A read sets a limit: the transaction's compute at the read plus the read's cap. The limit only
//! goes down, so the most restrictive of several reads is the one that binds, whatever their
//! order.
//!
//! # How the cap is enforced
//!
//! The engine has no counter per opcode, and the interpreter stops a frame on one condition only:
//! the frame runs out of regular gas. So the cap is enforced through the frame's gas, with the
//! revm fork's two parts of a frame's regular gas: a spendable part, the only one a regular charge
//! draws, and a withheld part. Every other reader of the frame's gas sees the two together — `GAS`,
//! the 63/64 forward and the clamp on an explicit call gas, the `SSTORE` sentry, the skip-cold
//! checks, the gas a child returns, the reimbursement — and a forward or a state or history spill
//! draws the withheld part first.
//!
//! Every frame that runs has its spendable part held at what the limit leaves the transaction
//! (`Gas::limit_spendable`); the rest is withheld. Three points hold this, and between them no
//! frame can compute past the limit (a frame answered without running is held by the rule under
//! the stop below):
//!
//! - **the read**: the opcode that read volatile data holds its own frame once it committed the
//!   read, so the instructions after it run on what the cap leaves;
//! - **a frame's start and every resume**: a frame that starts, and a caller a child returned into,
//!   is held to what the limit leaves then. A child's read holds its callers as they resume, and
//!   the gas a child hands back cannot be computed with past the limit;
//! - **a storage write restored to its original value**: it refills state and history gas that
//!   spilled onto regular gas, possibly before the read, onto the spendable part.
//!
//! So a transaction that read volatile data runs exactly as it would without the read until a
//! regular charge needs the withheld part. That charge fails as it would with nothing withheld,
//! and the fork records the crossing, with the regular gas the frame had before the charge.
//!
//! # The stop
//!
//! A frame whose result carries a crossing record crossed the cap, not its own gas: undetained,
//! the withheld part would have paid the charge. The frame stops the transaction the way every
//! transaction-level limit does — it reverts with `MegaLimitExceeded` (kind: compute), the
//! transaction is latched, no caller resumes, and the transaction settles like an EIP-8037 revert
//! (see [`AdditionalLimit`](crate::AdditionalLimit)). The stopped frame's gas is put back to what
//! it had before the charge that crossed, from the record: the charge is not made, the spendable
//! part the frame had and the withheld part go back to the sender, and the transaction is billed
//! its compute at the crossing, less than one charge short of the limit. The stop reports that
//! compute as what was used, the same figure at every site a crossing is made: a frame that ran,
//! an answer, a precompile; the size of the charge that crossed is not kept.
//!
//! Every other out-of-gas halts and burns as it would without the read: an operand above `usize`,
//! a failed state or history charge, and a regular charge the frame's whole gas could not pay.
//!
//! A frame answered without running is held the same way. revm runs a precompile inside the
//! frame's start, against the frame's gas limit, before its answer can be classified, so after a
//! read a precompile forwarded more than the allowance its frame would start with is decided
//! before it runs, from its price where the engine knows it (the `precompiles` module of `evm`):
//!
//! - priced within the allowance, it runs on its whole forward and answers as it would without the
//!   read;
//! - priced past the allowance and within the forward, it needs gas the limit withholds: it is
//!   answered out of gas without running, the answer is marked as a crossing, and the same rule
//!   stops the transaction;
//! - priced past its whole forward, it runs on the forward and runs out of gas, as without the
//!   read: a failed call that burns its forward and that its caller survives.
//!
//! One residual is a choice: the price does not tell whether an input passes the checks a
//! precompile makes after its gas check, so an input priced between the allowance and the forward
//! that would fail such a check is the stop, where without the read the check fails the call,
//! which burns its forward and which its caller survives. Running it to find out would compute
//! past the limit.
//!
//! Every entry of the Satin set is priced, op-revm's size-limited wrappers of the BN254 pairing
//! and the BLS12-381 MSMs and pairing included. A precompile the engine cannot price — a node's
//! own, a Satin address a node replaced — is run on the allowance, and its answer gets the rest of
//! the forward back. Within the allowance, it answers as it would without the read. Past it, it
//! answers out of gas without computing, the answer is marked as a crossing, and it is the stop,
//! whether its price is within its forward or not: one priced past its whole forward is the stop
//! too, where without the read it would be a failed call its caller survives. It also sees the
//! allowance as its gas limit.
//!
//! An interceptor builds its answer on all the gas the caller forwarded, the caller's withheld
//! part included: an answer that spent more regular gas than the allowance the frame would have
//! run on is answered out of gas and marked as a crossing, and the same rule stops the
//! transaction.
//!
//! A keyless deployment's call is the transaction's own frame, and runs no code: it charges its
//! own work — the overhead of decoding and recovering the signer, then the `CREATE` opcode's
//! regular gas — on its frame's gas, held to the limit as any frame's is, so a charge past the
//! limit is a crossing, whether a rule would then refuse the call or its creation would run. A
//! call that starts its creation suspends on it, and its charges are compute as a caller's are; a
//! call carrying value is refused before its frame is built, an answer held by the rule above.
//! The call reads its
//! signer's account through the journal, where the Host marks nothing, so a signer that is the
//! beneficiary is marked there, at the call's compute then: the creation runs for that account,
//! as a `CREATE` runs in a frame of it, which a read of the account started.
//!
//! # Nothing withheld leaks
//!
//! Withheld gas never leaves the frame's tracker, so there is nothing to release and nothing that
//! can escape the cap: a child's withheld part goes back to its caller with the rest of its gas,
//! and the caller is held again as it resumes; a frame answered without running starts with
//! nothing withheld and hands its forwarded gas back; the stop hands the frame's gas back with its
//! revert, both parts as they were before the charge that crossed; and a halt burns the frame's
//! gas, withheld part included, as it would undetained.
//! `return_create`'s deposit and hash charges, the creating frame's own compute, draw the spendable
//! part like any other regular charge.
//!
//! # Refused reads
//!
//! `MegaAccessControl` can switch volatile-data access off for a frame and every frame below it.
//! The Host then refuses the load and the opcode's wrapper reverts the frame with
//! `VolatileDataAccessDisabled(accessType)`. The refusal comes before the load, so a refused read
//! reads nothing and caps nothing, and the frame keeps the gas it had before the opcode: the
//! opcode's static gas is charged, as for any instruction that ran, and nothing else. A refusal
//! names the kind the opcode reads — `BLOCKHASH`'s names the hash, though the opcode loads the
//! block number first — and a `SLOTNUM` refusal names access type 12
//! ([`SLOT_NUM_ACCESS_TYPE`](crate::system::SLOT_NUM_ACCESS_TYPE)), which the contract's
//! `VolatileDataAccessType` does not declare. The error's argument is encoded as a `uint8`, and
//! Solidity handlers must decode it as `uint8`: Solidity's ABI decoder reverts on an enum value
//! out of range. [`decode_volatile_data_access_disabled`] is the Rust side's decoder.

mod detention;
mod volatile;

pub(crate) use detention::ComputeStop;
pub use detention::{
    decode_volatile_data_access_disabled, volatile_data_access_disabled_revert_data, Detention,
};
pub use volatile::VolatileDataAccess;
