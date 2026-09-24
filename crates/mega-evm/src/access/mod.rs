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
//! | the block beneficiary's account | `BALANCE`, `SELFBALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, the four calls (and the EIP-7702 delegate they follow), `SELFDESTRUCT` as either end; a transaction whose sender or recipient is the beneficiary, and an applied EIP-7702 authority that is | [`block_env_access_compute_gas_limit`](crate::EvmTxRuntimeLimits::block_env_access_compute_gas_limit) |
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
//! and the fork records the crossing, with the withheld part it could not draw.
//!
//! # The stop
//!
//! A frame whose result carries a crossing record crossed the cap, not its own gas: undetained,
//! the withheld part would have paid the charge. The frame stops the transaction the way every
//! transaction-level limit does — it reverts with `MegaLimitExceeded` (kind: compute), the
//! transaction is latched, no caller resumes, and the transaction settles like an EIP-8037 revert
//! (see [`AdditionalLimit`](crate::AdditionalLimit)). The stopped frame's gas is the withheld part
//! at the crossing: the spendable part it had counts as spent, which brings the transaction's
//! compute to the limit exactly, and the withheld part goes back to the sender. The stop reports
//! the limit as what was used; the size of the charge that crossed is not kept.
//!
//! Every other out-of-gas halts and burns as it would without the read: an operand above `usize`,
//! a failed state or history charge, and a regular charge the frame's whole gas could not pay.
//!
//! A frame answered without running is held the same way. revm runs a precompile inside the
//! frame's start, against the frame's gas limit, before its answer can be classified, so after a
//! read a precompile forwarded more than the allowance its frame would start with is run on that
//! allowance, and its answer gets the rest of the forward back. Priced within the allowance, it
//! answers as it would without the read. Priced past it, it answers out of gas without computing;
//! the answer is marked as a crossing, and the same rule stops the transaction. The price is not
//! known without running the precompile, so one priced past its whole forward runs out of the
//! allowance too, and is the stop, where without the read it would be a failed call that burns its
//! forward and that its caller survives. So is one priced between the allowance and its forward
//! whose input fails a check made after its gas check: it runs out of the allowance before that
//! check, where without the read the check fails the call, which burns its forward and which its
//! caller survives. A precompile run on the allowance also sees the allowance as its gas limit.
//!
//! An interceptor builds its answer on all the gas the caller forwarded, the caller's withheld
//! part included: an answer that spent more than the allowance the frame would have run on is
//! answered out of gas and marked as a crossing, and the same rule stops the transaction. An
//! interceptor may instead charge the frame, by taking gas off its limit, and let it run. The
//! charge is the interceptor's own work, and compute whether the call is then answered or runs:
//! one the allowance cannot pay stops the transaction before the frame runs, and one it can pay
//! leaves the frame the rest.
//!
//! # Nothing withheld leaks
//!
//! Withheld gas never leaves the frame's tracker, so there is nothing to release and nothing that
//! can escape the cap: a child's withheld part goes back to its caller with the rest of its gas,
//! and the caller is held again as it resumes; a frame answered without running starts with
//! nothing withheld and hands its forwarded gas back; the stop hands the withheld part back with
//! its revert; and a halt burns the frame's gas, withheld part included, as it would undetained.
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
//! block number first — and a `SLOTNUM` refusal names access type 12, which the contract's
//! `VolatileDataAccessType` does not declare.

mod detention;
mod volatile;

pub use detention::{volatile_data_access_disabled_revert_data, Detention};
pub use volatile::VolatileDataAccess;
