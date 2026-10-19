//! The Host of the Satin engine.
//!
//! # The Host only observes
//!
//! revm hands the Host the facts of every state-writing opcode: an `SSTORE`'s original, present and
//! new values, a log's topics and data, a `SELFDESTRUCT`'s balance and beneficiary. The Host
//! stages them ([`AdditionalLimit::stage_record`](crate::AdditionalLimit)) and records nothing.
//! The opcode's wrapper commits the staged record after the opcode completed, and charges the
//! frame the history the record costs; it discards the record, and charges nothing, when the
//! opcode failed. Recording in the Host would count a write the opcode's own failure then takes
//! back: `SSTORE` charges its dynamic gas after the Host call, and an out-of-gas there halts the
//! frame with the record already counted — and would have charged the frame for a byte the chain
//! never carries.
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
        cfg::{GasId, GasParams, StateGasSite},
        context::{ContextError, SStoreResult, SelfDestructResult, StateLoad},
        host::LoadError,
        journaled_state::AccountInfoLoad,
    },
    primitives::{Address, Log, StorageKey, StorageValue, B256, U256},
    Database,
};

use crate::{
    system::ORACLE_CONTRACT_ADDRESS, ExternalEnvTypes, MegaContext, OracleEnv, StagedRecord,
    VolatileDataAccess,
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

    /// Writes the slot and stages the write's values.
    #[inline]
    fn sstore_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        let load = self.inner.sstore_skip_cold_load(address, key, value, skip_cold_load)?;
        self.additional_limit.stage_record(StagedRecord::Sstore(load.data.clone()));
        Ok(load)
    }

    /// Writes the slot and stages the write's values.
    #[inline]
    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Option<StateLoad<SStoreResult>> {
        let load = self.inner.sstore(address, key, value)?;
        self.additional_limit.stage_record(StagedRecord::Sstore(load.data.clone()));
        Some(load)
    }

    /// Stages the log's topic count and data length, then appends it.
    #[inline]
    fn log(&mut self, log: Log) {
        self.additional_limit.stage_record(StagedRecord::Log {
            topics: log.topics().len() as u8,
            data_len: log.data.data.len() as u64,
        });
        self.inner.log(log);
    }

    /// Destructs the account and stages whether value moved and where to, marking a read of the
    /// block beneficiary's account when it is either end: the source's balance is read and
    /// cleared, the target's read and credited.
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
        self.additional_limit.stage_record(StagedRecord::SelfDestruct {
            had_value: load.data.had_value,
            target_exists: load.data.target_exists,
            to_other_account: target != address,
            beneficiary: target,
        });
        Ok(load)
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
        let answer = self.external_envs().oracle_env.get_oracle_storage(key);
        self.oracle_reads.record(key, answer);
        self.detention.observe(VolatileDataAccess::ORACLE);
        Ok(StateLoad::new(answer.unwrap_or(loaded), true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes};
    use revm::context::JournalTr;

    /// The Host stages what each state-writing call observed and counts nothing: counting is the
    /// opcode wrapper's, after the opcode completed.
    #[test]
    fn test_host_stages_observations_and_counts_nothing() {
        use crate::{test_utils::MemoryDatabase, MegaSpecId};
        use revm::{context_interface::Host, primitives::LogData};

        const CONTRACT: Address = address!("00000000000000000000000000000000000000c1");
        const BENEFICIARY: Address = address!("00000000000000000000000000000000000000c2");
        let db = MemoryDatabase::default().account_balance(CONTRACT, U256::from(3));
        let mut ctx = MegaContext::new(db, MegaSpecId::SATIN);
        ctx.inner.journaled_state.load_account(CONTRACT).unwrap();

        let slot = Host::sstore(&mut ctx, CONTRACT, U256::from(1), U256::from(9)).unwrap();
        assert_eq!(
            ctx.additional_limit.staged_record(),
            Some(&StagedRecord::Sstore(slot.data.clone())),
            "the non-Berlin entry point stages too"
        );
        assert_eq!(slot.data.new_value, U256::from(9));
        let slot =
            Host::sstore_skip_cold_load(&mut ctx, CONTRACT, U256::from(1), U256::from(4), false)
                .unwrap();
        assert_eq!(ctx.additional_limit.staged_record(), Some(&StagedRecord::Sstore(slot.data)));

        let log = Log {
            address: CONTRACT,
            data: LogData::new(vec![B256::ZERO, B256::ZERO], Bytes::from_static(b"abc")).unwrap(),
        };
        Host::log(&mut ctx, log);
        assert_eq!(
            ctx.additional_limit.staged_record(),
            Some(&StagedRecord::Log { topics: 2, data_len: 3 })
        );

        Host::selfdestruct(&mut ctx, CONTRACT, BENEFICIARY, false).unwrap();
        assert_eq!(
            ctx.additional_limit.staged_record(),
            Some(&StagedRecord::SelfDestruct {
                had_value: true,
                target_exists: false,
                to_other_account: true,
                beneficiary: BENEFICIARY,
            })
        );
        assert_eq!(
            ctx.additional_limit.usage(),
            crate::LimitUsage::ZERO,
            "the Host counts nothing"
        );
    }
}
