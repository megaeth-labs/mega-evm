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
//! the rolled-back amounts because nothing but this hook priced them in the first place. The one
//! exception is an inspector's: a call's upfront charge is given back for the account its frame's
//! inputs name, as revm reads it, so an inspector that rewrites the target has it priced there.
//!
//! A charge on an account created in the transaction and destroyed since is priced at nothing:
//! its destruction gave back what the transaction had charged for it, at the prices charged and
//! outside that rule, so a re-pricing after the mark gives nothing back a second time, and the
//! account keeps no further state gas.
//!
//! The destruction gives the account's leaf back only when no account stood at the address at the
//! transaction's start: the database answered none for it, `None` from
//! [`Database::basic`](revm::Database::basic), or an earlier transaction on the same journal
//! destroyed it. A database that answers an empty account for an address it holds none for gives
//! that leaf back to no destruction: an empty account is a leaf the state holds.
//!
//! Every other Host method delegates to op-revm's context.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::format;

use delegate::delegate;
use revm::{
    context::ContextTr,
    context_interface::{
        cfg::{
            gas::{GasTracker, PoolAmounts, StartCharge},
            GasId, GasParams, StateGasCharge, StateGasSite,
        },
        context::{ContextError, SStoreResult, SelfDestructResult, StateLoad},
        host::{CompletedWrite, FrameStart, LoadError, WriteVerdict},
        journaled_state::AccountInfoLoad,
    },
    primitives::{Address, Log, StorageKey, StorageValue, B256, U256},
    Database,
};

use crate::{
    charge_write_records_history, history_gas,
    limit::{DestroyedParts, HistoryBytes},
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

    /// Loads the account, marking a read of the block beneficiary's, or refusing it in a frame
    /// whose volatile-data access is off, when the load is a read: when an opcode's read wrapper
    /// makes it. `CREATE`'s and `CREATE2`'s loads of their creator and of the created address are
    /// no reads (see [`Detention`](crate::Detention)).
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
    /// lands on ([`priced_state_gas`](MegaContext::priced_state_gas)).
    ///
    /// A charge on an account created in the transaction and destroyed since is priced at
    /// nothing: from that mark until the transaction ends the account keeps no further state gas,
    /// so an `SSTORE` of one of its slots neither charges nor refills fresh-slot state gas, and
    /// code its own constructor deposits after destroying it is charged no state gas, whatever gas
    /// the frame holds. The destruction gave back what the transaction had charged for the
    /// account at the prices charged, outside this rule. The neutral configuration prices as the
    /// fixture's fork does ([`destruction_table_prices_state_gas`]).
    ///
    /// [`destruction_table_prices_state_gas`]: MegaContext::destruction_table_prices_state_gas
    #[inline]
    fn state_gas_price(&mut self, id: GasId, site: StateGasSite) -> Option<u64> {
        if self.destruction_table_prices_state_gas() && self.accounts().is_destroyed(site.address) {
            return Some(0);
        }
        self.priced_state_gas(id, site)
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

    /// Counts the record of a write an opcode completed on the running frame's pool, holds it to
    /// the limits, and charges the frame the history it costs. revm calls it at the end of
    /// `SSTORE`, `LOG0`..`LOG4` and `SELFDESTRUCT`, after every charge the opcode makes, so a write
    /// the opcode's own failure takes back is never counted.
    ///
    /// The record is held to the limits first, and only then is its history charged: a record the
    /// limit rejects is not kept, so its history is no charge, and the stop is what the frame
    /// reports — a revert whose output is the stop's
    /// [`MegaLimitExceeded`](crate::MegaLimitExceeded), on the frame's gas, so its caller resumes
    /// as after any revert. A record the limit accepts is charged, and a charge the frame cannot
    /// pay, or a byte count with no price, is an ordinary out-of-gas.
    ///
    /// A log's history is drawn from the frame's history allowance first, and the frame's gas pays
    /// the rest ([`history_allowance_gas`](crate::history_allowance_gas)): a write record is the
    /// frame's own to pay for, and a record taken back — a slot written back to its original
    /// value, an account a `SELFDESTRUCT` returns to what the transaction loaded — gives back what
    /// the frame's gas paid, never what an allowance did. Both sides are priced the same way, so a
    /// record taken back cancels its own charge exactly, whichever frame made it: a refill below
    /// zero is reconciled when the frame merges into its caller. A write that takes records back
    /// and makes others is refilled before it is charged, so what it nets to nothing costs
    /// nothing.
    ///
    /// The first effective `SELFDESTRUCT` of an account created in the transaction gives back every
    /// part of it the transaction made ([`DestroyedParts`]) to the destroying frame, whichever
    /// frame paid: what the parts take back is in the same check as what the operation adds, so
    /// the positive effects are held after the give-back; once the check passed, each part's
    /// state gas is refilled at the price it was charged, with the history of its records and of
    /// its code, before the operation's own records are charged. From that mark an `SSTORE` of
    /// one of the account's slots counts nothing, and the pricing hook prices its state gas at
    /// nothing ([`state_gas_price`](revm::context_interface::Host::state_gas_price)).
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
        let (counted, history, destroyed) = self.accounts().write_counted(&write);
        let check = self.additional_limit.draw(gas, &counted);
        if check.exceeded_limit() {
            return WriteVerdict::Revert(self.additional_limit.note_frame_stop(&check));
        }
        let state_given_back = match write {
            CompletedWrite::SelfDestruct { address, .. } => destroyed
                .as_ref()
                .is_none_or(|parts| self.refill_destroyed_parts(address, parts, gas)),
            CompletedWrite::Sstore { .. } | CompletedWrite::Log { .. } => true,
        };
        if !state_given_back {
            return WriteVerdict::OutOfGas;
        }
        if self.prices_history() {
            let HistoryBytes { appended, taken } = history;
            if taken != 0 {
                gas.refill_history(history_gas(taken).unwrap_or(0));
            }
            if let Some(parts) = &destroyed {
                let code = usize::try_from(parts.code).unwrap_or(usize::MAX);
                gas.refill_history(self.inner.cfg.gas_params.code_deposit_history_gas(code));
            }
            if appended != 0 {
                let Some(cost) = history_gas(appended) else { return WriteVerdict::OutOfGas };
                let cost = if from_allowance { gas.draw_history_allowance(cost) } else { cost };
                if !gas.record_history_cost(cost) {
                    return WriteVerdict::OutOfGas;
                }
            }
        }
        if holds_again {
            self.detention.hold(gas);
        }
        WriteVerdict::Continue
    }

    /// Charges the running frame the history of the write records the frame it is starting makes
    /// — the accounts a value transfer changes from what the transaction loaded, a creation's
    /// created account when the transaction had not changed it, and its creator's nonce — as the
    /// journal's state stands ([`Accounts::start_records`](crate::limit::Accounts)). revm calls it
    /// at `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE` and `CREATE2`, once the opcode
    /// made every charge and took the gas it forwards, and before it builds the frame's input.
    ///
    /// It is a price, made at the opcode as revm charges a new account's gas at `CALL`, and counts
    /// nothing: the records themselves are counted once revm decided the start, the frame's on the
    /// pool it starts with and the creator's on the creator once the creation returned.
    ///
    /// The charge comes out of what the caller kept rather than out of what the callee gets, so
    /// the frame's allowance is free for what the recipient does. What the frame's own records
    /// cost is declared on the start ([`FrameStart::charge`]), which the fork carries on the
    /// frame's input and gives back to the caller when the frame fails; the creator's record
    /// outlives the creation and is not declared. The frame's input copies the caller's
    /// reservoir after the charge, so the frame, or the answer revm gives in its place,
    /// inherits the reservoir the charge left. An opcode that starts no frame — a creation the
    /// balance, the nonce or the depth refuses, an out-of-gas before the forward — never calls
    /// it, and makes no records; with EIP-8037 off, such a creation calls it without the address
    /// it would take, which makes none either. A start revm then refuses — a value the caller
    /// cannot fund, a frame past the call-stack limit — makes no records, and the charge comes
    /// back when it returns, as does what a frame that ran did not keep.
    ///
    /// A charge the caller cannot pay, or a price that does not fit in a `u64`, fails the opcode
    /// with an out-of-gas and starts no frame, even where revm would have refused the start. The
    /// gas the opcode forwarded goes back to the caller first, so the halt burns it with the rest:
    /// the frame never ran, and gas detention must not count the forward as compute. The halt's
    /// record of what the frame had, which detention reads where the frame ends, is written again
    /// once the forward is back: a failed charge recorded the gas left after the forward was
    /// taken, and a price that could not be computed failed before any charge.
    #[inline]
    fn frame_starting(&mut self, start: &mut FrameStart, gas: &mut GasTracker) -> bool {
        let charge = if self.prices_history() {
            let records =
                self.accounts().start_records(start.kind, start.caller, start.target, start.value);
            let on_frame = records.on_frame().added;
            let creator = u64::from(records.creator);
            let Some((charge, _)) = charge_write_records_history(gas, on_frame, creator) else {
                gas.erase_cost(start.forward);
                gas.set_failed_charge_remaining(Some(gas.remaining()));
                return false;
            };
            charge
        } else {
            0
        };
        start.charge = StartCharge::new(charge, PoolAmounts::ZERO);
        true
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Prices one EIP-8037 state gas unit `id` at `site`, scaling the schedule's entry by the SALT
    /// bucket capacity of the account — or, for a slot-scoped entry, of the storage slot — the
    /// charge lands on.
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
    fn priced_state_gas(&mut self, id: GasId, site: StateGasSite) -> Option<u64> {
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

    /// Refills on `gas`, the destroying frame's, the state gas of the `parts` of the account at
    /// `address` its first effective destruction gives back, each at the price it was charged:
    /// the leaf's new account, the code deposit's bytes and each slot made non-zero, priced at the
    /// site the charge landed on, so SALT gives back what it scaled. The leaf was charged once as a
    /// new account, by whichever site made it — a value transfer, a `SELFDESTRUCT`'s balance, the
    /// transaction's value, or the creation itself — and every such charge is the same entry. The
    /// account is destroyed by now, which the pricing hook prices at nothing, so the parts are
    /// priced as they were charged, before the mark ([`priced_state_gas`](Self::priced_state_gas)).
    ///
    /// Nothing is refilled where the transaction's state gas is not the spec's: the neutral
    /// configuration prices as the fixture's fork does, and EIP-8037 gives back nothing for this
    /// destruction. `false` when a price could not be looked up, whose cause is recorded and fails
    /// the transaction.
    fn refill_destroyed_parts(
        &mut self,
        address: Address,
        parts: &DestroyedParts,
        gas: &mut GasTracker,
    ) -> bool {
        if !self.destruction_table_prices_state_gas() {
            return true;
        }
        debug_assert_eq!(
            self.inner.cfg.gas_params.get(GasId::new_account_state_gas()),
            self.inner.cfg.gas_params.get(GasId::create_state_gas()),
            "a leaf a creation charged comes back at the price of one a transfer charged"
        );
        let account = StateGasSite::account(address);
        let leaf = parts.leaf.then(|| StateGasCharge::one(GasId::new_account_state_gas(), account));
        let code = (parts.code != 0)
            .then(|| StateGasCharge::units(GasId::code_deposit_state_gas(), account, parts.code));
        let slots = parts.slots.iter().map(|&key| {
            StateGasCharge::one(GasId::sstore_set_state_gas(), StateGasSite::slot(address, key))
        });
        for charge in leaf.into_iter().chain(code).chain(slots) {
            let Some(price) = self.priced_state_gas(charge.id, charge.site) else { return false };
            gas.refill_reservoir(charge.total(price));
        }
        true
    }

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
        assert_eq!(ctx.limit_usage(), LimitUsage::ZERO, "the Host counts nothing");

        // The hook is what counts: the slot's first change is one write record, and a slot made
        // non-zero from an original of zero is one of state growth.
        let mut gas = GasTracker::new(1_000_000, 1_000_000, 0);
        let write =
            CompletedWrite::Sstore { address: CONTRACT, key: U256::from(1), result: &slot.data };
        assert_eq!(Host::write_completed(&mut ctx, write, &mut gas), WriteVerdict::Continue);
        assert_eq!(
            ctx.limit_usage(),
            LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1, state_growth: 1 }
        );
    }
}
