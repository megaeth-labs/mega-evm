//! The Host of the Satin engine.
//!
//! # Writes are counted once their opcode completed
//!
//! revm hands the Host the facts of every state-writing opcode twice: as it makes the write (an
//! `SSTORE`'s slot, a log, a `SELFDESTRUCT`'s beneficiary), and once the opcode completed, in the
//! completed-write hook ([`write_completed`](revm::context_interface::Host::write_completed)),
//! after every charge the opcode makes. The Host counts nothing in the first: the write methods
//! only write, whoever calls them. The hook commits the write's record to the running frame, holds
//! it to the limits and charges the frame the history the record costs. Counting at the write
//! would count one the opcode's own failure then takes back: `SSTORE` charges its dynamic gas after
//! the write, and an out-of-gas there halts the frame with the record already counted — and would
//! have charged the frame for a byte the chain never carries. A tool's write through the Host, an
//! inspector's log among them, reaches no hook, and is not counted.
//!
//! `block_hash` serves the read and records it, so a stateless witness learns of a `BLOCKHASH`
//! that bypassed the journal.
//!
//! # Volatile reads
//!
//! The Host is where gas detention marks a read of volatile data: the block-environment
//! accessors, the account load when the account is the block beneficiary, the storage load when
//! the storage is the Oracle's, and `SELFDESTRUCT` when either end is the beneficiary. A read is
//! marked once the load succeeded, and the opcode's wrapper commits it once the opcode completed
//! (see the `access` module). Every account opcode reaches the account load: `balance`,
//! `load_account_delegated`, `load_account_code` and `load_account_code_hash` are the trait's own,
//! built on it.
//!
//! While volatile-data access is switched off for the running frame, the Host refuses those loads
//! instead: an account or storage load fails as a skipped cold load, and a block-environment
//! accessor answers zero. Nothing is loaded, and the opcode's wrapper turns the refusal into the
//! frame's revert.
//!
//! # The Oracle's storage
//!
//! A storage load whose owner is the Oracle reads the slot from the node's oracle service
//! ([`OracleEnv`]) and falls back to the database for a slot the service has no value for. Every
//! such read is priced as a cold access, because a node replaying a block cannot tell which of the
//! two sources the node that built it read.
//!
//! # One hook prices every state charge
//!
//! [`state_gas_price`](revm::context_interface::Host::state_gas_price) is the one place an
//! EIP-8037 state gas charge is priced. Every charge site of the engine asks it, and so does
//! every site that names a charge to give back — the same `(id, site)` pair, so the refill
//! cancels the charge it undoes exactly, whatever the price was. That is what lets SALT scale the
//! charge by the capacity of the bucket it lands in without a second table of what to give back.
//!
//! A charge comes back two ways, and neither reads a second table. `SSTORE`'s own restore, a
//! caller's upfront charge for a call or creation that did not add the leaf, the transaction-level
//! refund and the settlement of a synthetic frame result re-price the recorded `(id, site)`
//! through this hook. A frame that fails instead rolls its own charges back by the amounts it
//! recorded while running, without asking for a price at all. Both cancel exactly, for different
//! reasons: the re-priced sites because the multiplier of a bucket is read once per transaction,
//! the rolled-back amounts because nothing but this hook priced them in the first place.
//!
//! Every other Host method delegates to op-revm's context.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::format;

use delegate::delegate;
use revm::{
    context::ContextTr,
    context_interface::{
        cfg::{gas::GasTracker, GasId, GasParams, StateGasSite},
        context::{ContextError, SStoreResult, SelfDestructResult, StateLoad},
        host::{CompletedWrite, FrameStart, LoadError, WriteVerdict},
        journaled_state::AccountInfoLoad,
    },
    primitives::{Address, Log, StorageKey, StorageValue, B256, U256},
    Database,
};

use crate::{
    charge_write_records_history, history_gas,
    limit::{shadow, HistoryBytes},
    system::ORACLE_CONTRACT_ADDRESS,
    ExternalEnvTypes, MegaContext, OracleEnv, VolatileDataAccess,
};

impl<DB: Database, ExtEnvs: ExternalEnvTypes> revm::context_interface::Host
    for MegaContext<DB, ExtEnvs>
{
    delegate! {
        to self.inner {
            fn chain_id(&self) -> U256;
            // `DIFFICULTY` reads the randomness on every spec since the merge; see `prevrandao`.
            fn difficulty(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn caller(&self) -> Address;
            fn blob_hash(&self, number: usize) -> Option<U256>;
            fn max_initcode_size(&self) -> usize;
            fn gas_params(&self) -> &GasParams;
            fn is_amsterdam_eip8037_enabled(&self) -> bool;
            fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);
            fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;
            // A write is counted once its opcode completed, in `write_completed`: what the Host's
            // write methods are called with, by an opcode or by a tool, counts nothing.
            fn sstore(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
            ) -> Option<StateLoad<SStoreResult>>;
            fn sstore_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
                skip_cold_load: bool,
            ) -> Result<StateLoad<SStoreResult>, LoadError>;
            fn log(&mut self, log: Log);
        }
    }

    #[inline]
    fn basefee(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::BASE_FEE) {
            return Default::default();
        }
        self.inner.basefee()
    }

    #[inline]
    fn blob_gasprice(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::BLOB_BASE_FEE) {
            return Default::default();
        }
        self.inner.blob_gasprice()
    }

    #[inline]
    fn gas_limit(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::GAS_LIMIT) {
            return Default::default();
        }
        self.inner.gas_limit()
    }

    #[inline]
    fn prevrandao(&self) -> Option<U256> {
        if !self.read_block_env(VolatileDataAccess::PREV_RANDAO) {
            return Some(U256::ZERO);
        }
        self.inner.prevrandao()
    }

    #[inline]
    fn block_number(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::BLOCK_NUMBER) {
            return Default::default();
        }
        self.inner.block_number()
    }

    #[inline]
    fn timestamp(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::TIMESTAMP) {
            return Default::default();
        }
        self.inner.timestamp()
    }

    #[inline]
    fn beneficiary(&self) -> Address {
        if !self.read_block_env(VolatileDataAccess::COINBASE) {
            return Default::default();
        }
        self.inner.beneficiary()
    }

    #[inline]
    fn slot_num(&self) -> U256 {
        if !self.read_block_env(VolatileDataAccess::SLOT_NUM) {
            return Default::default();
        }
        self.inner.slot_num()
    }

    /// Loads the slot; a slot of the Oracle's storage is read through the oracle environment
    /// ([`oracle_sload`](Self::oracle_sload)).
    #[inline]
    fn sload_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        if address == ORACLE_CONTRACT_ADDRESS {
            return self.oracle_sload(key, skip_cold_load);
        }
        self.inner.sload_skip_cold_load(address, key, skip_cold_load)
    }

    /// Loads the account, marking a read of the block beneficiary's.
    #[inline]
    fn load_account_info_skip_cold_load(
        &mut self,
        address: Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountInfoLoad<'_>, LoadError> {
        let beneficiary = address == self.inner.block.beneficiary;
        if beneficiary && self.detention.refuses(VolatileDataAccess::BENEFICIARY_BALANCE) {
            return Err(LoadError::ColdLoadSkipped);
        }
        let load =
            self.inner.load_account_info_skip_cold_load(address, load_code, skip_cold_load)?;
        if beneficiary {
            self.detention.observe(VolatileDataAccess::BENEFICIARY_BALANCE);
        }
        Ok(load)
    }

    /// Prices one EIP-8037 state gas unit, scaling the schedule's entry by the SALT bucket
    /// capacity of the account — or, for a slot-scoped entry, of the storage slot — the charge
    /// lands on.
    ///
    /// `state gas = schedule entry x m`, where `m` is the bucket's capacity in minimum buckets.
    /// A charge in a minimum-capacity bucket pays the schedule's own number, and a charge in a
    /// bucket eight times as large pays eight times as much: state that crowds a region of the
    /// trie costs what it makes everyone else pay to carry it. Only the state dimension scales;
    /// regular gas is never touched by `m`.
    ///
    /// Two charges are priced without reading SALT at all: an entry the schedule prices at zero,
    /// which `m` cannot move off zero, and every charge of a system-originated transaction,
    /// which prices at the minimum bucket by rule (see
    /// [`is_system_originated`](crate::MegaContext::is_system_originated)).
    ///
    /// Reporting nothing is how a failed capacity lookup travels: the cause is recorded in the
    /// context error the way [`Host::sload`](revm::context_interface::Host::sload) records a
    /// database failure, and the charge site turns it into a bail-out that the transaction
    /// surfaces as an error. A price the engine could not look up is never replaced by one
    /// nobody chose.
    #[inline]
    fn state_gas_price(&mut self, id: GasId, site: StateGasSite) -> Option<u64> {
        let base = self.inner.cfg.gas_params.get(id);
        if base == 0 || self.is_system_originated() {
            return Some(base);
        }
        let multiplier = match site.slot {
            Some(slot) => self.slot_bucket_multiplier(site.address, slot),
            None => self.account_bucket_multiplier(site.address),
        };
        match multiplier {
            Ok(multiplier) => Some(base.saturating_mul(multiplier)),
            Err(error) => {
                *self.error() =
                    Err(ContextError::Custom(format!("SALT bucket lookup failed: {error}")));
                None
            }
        }
    }

    /// Serves the hash and records that this block read it.
    ///
    /// `BLOCKHASH` reads the chain's history through the database, so a witness built from the
    /// journal alone would miss it; the record is where a node learns of the read.
    #[inline]
    fn block_hash(&mut self, number: u64) -> Option<B256> {
        if !self.read_block_env(VolatileDataAccess::BLOCK_HASH) {
            return Some(B256::ZERO);
        }
        let hash = self.inner.block_hash(number)?;
        self.block_hash_record.record(number, hash);
        Some(hash)
    }

    /// Destructs the account, marking a read of the block beneficiary's account when it is either
    /// end: the source's balance is read and cleared, the target's read and credited.
    #[inline]
    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        let beneficiary = self.inner.block.beneficiary;
        let volatile = target == beneficiary || address == beneficiary;
        if volatile && self.detention.refuses(VolatileDataAccess::BENEFICIARY_BALANCE) {
            return Err(LoadError::ColdLoadSkipped);
        }
        let load = self.inner.selfdestruct(address, target, skip_cold_load)?;
        if volatile {
            self.detention.observe(VolatileDataAccess::BENEFICIARY_BALANCE);
        }
        Ok(load)
    }

    /// Commits the record of a write an opcode completed — to the running frame's lane, and to the
    /// running frame's level of the journal's ledger — holds it to the limits, and charges the
    /// frame the history it costs. revm calls it at the end of `SSTORE`, `LOG0`..`LOG4` and
    /// `SELFDESTRUCT`, after every charge the opcode makes, so a write the opcode's own failure
    /// takes back is never counted.
    ///
    /// The record is held to the limits first, and only then is its history charged: a record the
    /// limit rejects is not kept, so its history is no charge, and the stop is what the frame
    /// reports — a revert whose output is the stop's
    /// [`MegaLimitExceeded`](crate::MegaLimitExceeded), on the frame's gas, so its caller resumes
    /// as after any revert. A record the limit accepts is charged, and a charge the frame cannot
    /// pay, or a byte count with no price, is an ordinary out-of-gas.
    ///
    /// A log's history is drawn from the frame's history allowance first, and the frame's gas pays
    /// the rest ([`storage_call_stipend`](crate::storage_call_stipend)): a write record is the
    /// frame's own to pay for, and a record taken back — a slot written back to its original value
    /// — gives back what the frame's gas paid, never what an allowance did. Both sides are priced
    /// the same way, so a record taken back cancels its own charge exactly, whichever frame made
    /// it: a refill below zero is reconciled when the frame merges into its caller.
    ///
    /// A completed `SSTORE` holds the frame to the compute limit again: a slot restored to its
    /// original value refills the state gas (revm, before this hook) and the history gas (here)
    /// that spilled onto regular gas, and the spill may predate the limit.
    #[inline]
    fn write_completed(&mut self, write: CompletedWrite<'_>, gas: &mut GasTracker) -> WriteVerdict {
        let (from_allowance, holds_again) = match write {
            CompletedWrite::Sstore { .. } => (false, true),
            CompletedWrite::Log { .. } => (true, false),
            CompletedWrite::SelfDestruct { .. } => (false, false),
        };
        // One count, committed to the lanes, which decide, and to the journal's ledger.
        let (count, history) = self.additional_limit.write_count(&write);
        let check = self.additional_limit.commit_count(&count);
        let journaled = self.ledger_mut().count_write(&count);
        shadow::completed_write(&self.additional_limit, self.ledger(), check, journaled);
        if check.exceeded_limit() {
            return WriteVerdict::Revert(self.additional_limit.note_frame_stop(&check));
        }
        if self.prices_history() {
            match history {
                HistoryBytes::None => {}
                HistoryBytes::Appended(bytes) => {
                    let Some(cost) = history_gas(bytes) else { return WriteVerdict::OutOfGas };
                    let drawn = if from_allowance {
                        self.additional_limit.try_consume_stipend(cost)
                    } else {
                        0
                    };
                    if !gas.record_history_cost(cost - drawn) {
                        return WriteVerdict::OutOfGas;
                    }
                }
                HistoryBytes::Taken(bytes) => gas.refill_history(history_gas(bytes).unwrap_or(0)),
            }
        }
        if holds_again {
            self.detention.hold(gas);
        }
        WriteVerdict::Continue
    }

    /// Charges the running frame the history of the write records the frame it is starting makes
    /// — a value transfer's sender and recipient, a creation's creator nonce and created account.
    /// revm calls it at `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE` and `CREATE2`,
    /// once the opcode made every charge and took the gas it forwards, and before it builds the
    /// frame's input.
    ///
    /// The charge comes out of what the caller kept rather than out of what the callee gets, so
    /// the frame's allowance is free for what the recipient does. The frame's input copies the
    /// caller's reservoir after the charge, so the frame, or the answer revm gives in its place,
    /// inherits the reservoir the charge left. An opcode that starts no frame — a creation the
    /// balance, the nonce or the depth refuses, an out-of-gas before the forward — never calls
    /// it, and makes no records. A start revm then refuses — a value the caller cannot fund, a
    /// frame past the call-stack limit — makes no records, and the charge comes back when it
    /// returns, as does what a frame that ran did not keep.
    ///
    /// A charge the caller cannot pay, or a price that does not fit in a `u64`, fails the opcode
    /// with an out-of-gas and starts no frame, even where revm would have refused the start. The
    /// gas the opcode forwarded goes back to the caller first, so the halt burns it with the rest:
    /// the frame never ran, and gas detention must not count the forward as compute. The halt's
    /// record of what the frame had, which detention reads where the frame ends, is written again
    /// once the forward is back: a failed charge recorded the gas left after the forward was
    /// taken, and a price that could not be computed failed before any charge.
    #[inline]
    fn frame_starting(&mut self, start: &FrameStart, gas: &mut GasTracker) -> bool {
        let records = self.additional_limit.opcode_start_records(start);
        if !self.prices_history() {
            return true;
        }
        let charged = charge_write_records_history(gas, records.on_lane, u64::from(records.caller));
        let Some((on_lane, caller)) = charged else {
            gas.erase_cost(start.forward);
            gas.set_failed_charge_remaining(Some(gas.remaining()));
            return false;
        };
        self.additional_limit.stage_frame_charge(records, on_lane, caller);
        true
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Reads the block-environment field `access` stands for: `false` when the read is refused,
    /// and the accessor answers zero instead of the field.
    #[inline]
    fn read_block_env(&self, access: VolatileDataAccess) -> bool {
        if self.detention.refuses(access) {
            return false;
        }
        self.detention.observe(access);
        true
    }

    /// Reads the slot `key` of the Oracle's storage, in the Oracle's own frame: a storage load
    /// whose owner is the Oracle, so an `SLOAD` of any other contract, and a `DELEGATECALL` into
    /// the Oracle's code, which runs on its caller's storage, are ordinary loads.
    ///
    /// The slot is loaded through the journal as any read of it is, then the oracle environment
    /// is asked ([`OracleEnv::get_oracle_storage`]): the frame gets the environment's value when
    /// it has one, over whatever the chain or the frame itself stored in the slot, and the loaded
    /// value otherwise. Hints reach the environment synchronously as the `sendHint` call is made,
    /// so a read that follows a hint in execution finds the environment told, as the trait
    /// promises.
    ///
    /// A node replaying a block cannot tell whether the node that built it answered a read from
    /// its oracle service or from the chain, so nothing the transaction pays and nothing its
    /// state records may depend on the source. The read is always priced cold, whichever source
    /// answered and however often the transaction read the slot before; and the slot is loaded on
    /// both paths, so afterwards it is warm for the frame's `SSTORE` whichever source answered,
    /// and it is in the transaction's state, which a stateless witness is built from, even when
    /// the environment's value was the one used.
    ///
    /// A system call does not ask the environment, whatever it holds: its read answers the chain's
    /// slot, so the call is a function of the state a stateless validator is given (see
    /// [`MegaContext::is_system_call`]).
    ///
    /// A frame whose regular gas, the part detention withholds included, cannot pay the cold
    /// access reads nothing and asks nothing, as revm's own skipped cold load does. A detained
    /// frame that holds the gas but may not spend it does read and ask, and the charge then stops
    /// the transaction at the compute limit.
    ///
    /// It is a read of volatile data: it is refused while the frame's volatile-data access is off,
    /// and marked for gas detention, under the Oracle's cap, once it succeeded.
    ///
    /// The read and the answer are recorded on the context for the transaction's outcome
    /// ([`OracleReadRecord`](crate::OracleReadRecord)): the answer is in no database, so a
    /// stateless validator is given the included transactions' reads in place of the service.
    fn oracle_sload(
        &mut self,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        if self.detention.refuses(VolatileDataAccess::ORACLE) || skip_cold_load {
            return Err(LoadError::ColdLoadSkipped);
        }
        let loaded = revm::context_interface::Host::sload_skip_cold_load(
            &mut self.inner,
            ORACLE_CONTRACT_ADDRESS,
            key,
            false,
        )?
        .data;
        // A system call reads the chain: it asks the service nothing and records no answer, and
        // the read is priced and loaded as any other.
        let answer = if self.is_system_call() {
            None
        } else {
            let answer = self.external_envs().oracle_env.get_oracle_storage(key);
            self.oracle_reads.record(key, answer);
            answer
        };
        self.detention.observe(VolatileDataAccess::ORACLE);
        Ok(StateLoad::new(answer.unwrap_or(loaded), true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes};
    use revm::context::JournalTr;

    /// The Host's write methods count nothing, whoever calls them — an opcode, or a tool such as
    /// an inspector appending a log in its `step`: a write is counted once its opcode completed,
    /// through the completed-write hook, which only revm's write instructions call.
    #[test]
    fn test_host_writes_count_nothing() {
        use crate::{test_utils::MemoryDatabase, LimitUsage, MegaSpecId, WRITE_RECORD_SIZE};
        use revm::{context_interface::Host, primitives::LogData};

        const CONTRACT: Address = address!("00000000000000000000000000000000000000c1");
        const BENEFICIARY: Address = address!("00000000000000000000000000000000000000c2");
        let db = MemoryDatabase::default().account_balance(CONTRACT, U256::from(3));
        let mut ctx = MegaContext::new(db, MegaSpecId::SATIN);
        ctx.inner.journaled_state.load_account(CONTRACT).unwrap();

        let slot = Host::sstore(&mut ctx, CONTRACT, U256::from(1), U256::from(9)).unwrap();
        assert_eq!(slot.data.new_value, U256::from(9));
        Host::sstore_skip_cold_load(&mut ctx, CONTRACT, U256::from(2), U256::from(4), false)
            .unwrap();
        let log = Log {
            address: CONTRACT,
            data: LogData::new(vec![B256::ZERO, B256::ZERO], Bytes::from_static(b"abc")).unwrap(),
        };
        Host::log(&mut ctx, log);
        let destructed = Host::selfdestruct(&mut ctx, CONTRACT, BENEFICIARY, false).unwrap();
        assert!(destructed.data.had_value);
        assert_eq!(ctx.additional_limit.usage(), LimitUsage::ZERO, "the Host counts nothing");

        // The hook is what counts: the slot's first change is one write record.
        let mut gas = GasTracker::new(1_000_000, 1_000_000, 0);
        let write =
            CompletedWrite::Sstore { address: CONTRACT, key: U256::from(1), result: &slot.data };
        assert_eq!(Host::write_completed(&mut ctx, write, &mut gas), WriteVerdict::Continue);
        assert_eq!(
            ctx.additional_limit.usage(),
            LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1 },
        );
    }
}
