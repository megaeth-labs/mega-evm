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
//! | the block environment | `NUMBER`, `TIMESTAMP`, `COINBASE`, `PREVRANDAO`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `SLOTNUM`, `BLOCKHASH` | [`BLOCK_ENV_ACCESS_COMPUTE_GAS`](crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS) |
//! | the block beneficiary's account | `BALANCE`, `SELFBALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`, the four calls (and the EIP-7702 delegate they follow), `SELFDESTRUCT` as either end; a transaction whose sender or recipient is the beneficiary, and an applied EIP-7702 authority that is | [`BLOCK_ENV_ACCESS_COMPUTE_GAS`](crate::constants::BLOCK_ENV_ACCESS_COMPUTE_GAS) |
//! | the Oracle's storage | `SLOAD` in the Oracle's own frame | [`ORACLE_ACCESS_COMPUTE_GAS`](crate::constants::ORACLE_ACCESS_COMPUTE_GAS) |
//!
//! `BLOBHASH` is not on the list: it reads the transaction's own blob hashes, which nothing else
//! decides. A system-originated transaction and a system call are not detained: they are the
//! protocol maintaining its own state, the Oracle's included.
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
//! and history gas that spilled onto regular gas are not compute, and neither is what a halting
//! frame burns ([`Detention`]). A read sets a limit: the transaction's compute at the read plus
//! the read's cap. The limit only goes down, so the most restrictive of several reads is the one
//! that binds, whatever their order.
//!
//! # How the cap is enforced
//!
//! The engine has no counter per opcode, and the interpreter stops a frame on one condition only:
//! the frame runs out of regular gas. So the cap is enforced through the frame's gas. Every frame
//! that runs keeps no more regular gas than the limit leaves the transaction; the rest is
//! *withheld* — moved into the frame's reservoir — until the frame returns. Three points hold
//! this, and between them no frame can compute past the limit:
//!
//! - **the read**: the opcode that read volatile data withholds from its own frame, right after the
//!   Host's load, so the instructions after it run on what the cap leaves;
//! - **a frame's start and every resume**: a frame that starts, and a caller a child returned into,
//!   is held to what the limit leaves then. A child's read caps its callers as they resume, and the
//!   gas a child hands back cannot be computed with past the limit;
//! - **a storage write restored to its original value**: it refills state and history gas that
//!   spilled onto regular gas, possibly before the read, so the frame is held to the limit again.
//!
//! A frame that runs out of regular gas while detention withheld some of it has crossed the cap,
//! not its own gas: detention holds the frame to less than it was given, so the charge that
//! failed was one the cap refused. The frame then stops the transaction the way every
//! transaction-level limit does — it reverts with `MegaLimitExceeded` (kind: compute), the
//! transaction is latched, no caller resumes, and the transaction settles like an EIP-8037 revert
//! (see [`AdditionalLimit`](crate::AdditionalLimit)). What the frame had left of its allowance when
//! the charge failed is spent; the withheld gas is not, and goes back to the sender. A frame
//! detention withheld nothing from runs out of its own gas, and halts: a child forwarded less
//! than the limit leaves the transaction halts on its own, and its caller resumes.
//!
//! State and history charges are not held back. They draw on the reservoir first, where the
//! withheld gas sits, so a detained frame pays for what it writes and appends as it would have.
//!
//! # Withheld gas does not leak
//!
//! Withheld gas is regular gas the frame was given and must neither be lost nor escape the cap.
//! A frame's end hands it back into the frame's regular gas before the result settles, as if it
//! had never been withheld ([`release`](detention::release)), on every path:
//!
//! - **a frame answered without running** — an interceptor's answer, the latch, the depth guard, an
//!   inspector — never runs, so nothing is withheld from it; the answer carries the forwarded gas
//!   back to its caller, and the caller is held to the limit when it resumes;
//! - **the transaction-level stop** returns the stopped frame's withheld gas with the revert, and
//!   every caller's with its own, so the sender gets back what nobody spent;
//! - **the frame's return** releases on success, revert and halt alike: a success or a revert hands
//!   the unspent gas to the caller, which is held to the limit as it resumes, and a halt burns it
//!   as it would have burned it undetained.
//!
//! # Refused reads
//!
//! `MegaAccessControl` can switch volatile-data access off for a frame and every frame below it.
//! The Host then refuses the load and the opcode's wrapper reverts the frame with
//! `VolatileDataAccessDisabled(accessType)`. The refusal comes before the load, so a refused read
//! reads nothing and caps nothing, and the frame keeps the gas it had before the opcode: the
//! opcode's static gas is charged, as for any instruction that ran, and nothing else. A `SLOTNUM`
//! refusal names access type 12, which the contract's `VolatileDataAccessType` does not declare.

mod detention;
mod volatile;

pub use detention::{volatile_data_access_disabled_revert_data, Detention};
pub use volatile::VolatileDataAccess;
