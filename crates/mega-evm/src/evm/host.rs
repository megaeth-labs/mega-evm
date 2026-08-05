use core::cell::RefCell;

#[cfg(not(feature = "std"))]
use alloc as std;
use mega_system_contracts::access_control::IMegaAccessControl::VolatileDataAccessType;
use std::{format, rc::Rc};

use crate::{
    AdditionalLimit, ExternalEnvTypes, MegaContext, MegaSpecId, OracleEnv,
    VolatileDataAccessTracker, ORACLE_CONTRACT_ADDRESS,
};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context::{ContextTr, JournalTr},
    context_interface::{
        cfg::GasParams,
        context::ContextError,
        host::LoadError,
        journaled_state::{AccountInfoLoad, AccountLoad},
    },
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
    primitives::{hash_map::Entry, StorageKey, StorageValue, KECCAK_EMPTY},
    state::{Account, Bytecode, EvmStorageSlot},
    Journal,
};

impl<DB: Database, ExtEnvs: ExternalEnvTypes> Host for MegaContext<DB, ExtEnvs> {
    // Block environment related methods - with tracking
    fn basefee(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BaseFee);
        self.inner.basefee()
    }

    fn gas_limit(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::GasLimit);
        self.inner.gas_limit()
    }

    fn difficulty(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::Difficulty);
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccessType::PrevRandao);
        self.inner.prevrandao()
    }

    fn block_number(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BlockNumber);
        self.inner.block_number()
    }

    fn timestamp(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::Timestamp);
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.mark_block_env_accessed(VolatileDataAccessType::Coinbase);
        self.inner.beneficiary()
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        self.mark_block_env_accessed(VolatileDataAccessType::BlockHash);
        self.inner.block_hash(number)
    }

    // Blob-related block environment methods - with tracking
    fn blob_gasprice(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccessType::BlobBaseFee);
        self.inner.blob_gasprice()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccessType::BlobHash);
        self.inner.blob_hash(number)
    }

    delegate! {
        to self.inner {
            fn slot_num(&self) -> U256;
            fn gas_params(&self) -> &GasParams;
            fn is_amsterdam_eip8037_enabled(&self) -> bool;
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn log(&mut self, log: Log);
            fn caller(&self) -> Address;
            fn max_initcode_size(&self) -> usize;
            fn sstore(
                &mut self,
                address: Address,
                key: U256,
                value: U256,
            ) -> Option<StateLoad<SStoreResult>>;
            fn tstore(&mut self, address: Address, key: U256, value: U256);
            fn tload(&mut self, address: Address, key: U256) -> U256;
        }
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        // Rex4+: Mark beneficiary balance access when SELFDESTRUCT targets the beneficiary.
        // This enables gas detention and the disableVolatileDataAccess check in the instruction
        // wrapper.
        if self.spec.is_enabled(MegaSpecId::REX4) {
            self.check_and_mark_beneficiary_balance_access(&target);
        }

        // Rex4+: Before inner selfdestruct mutates account status, inspect the account
        // to compute state growth refund for same-TX-created accounts (EIP-6780).
        // Uses non-delegating inspect_account to ensure we enumerate storage on the
        // actual selfdestructed address, not a delegation target.
        let selfdestruct_refund = if self.spec.is_enabled(MegaSpecId::REX4) {
            let journal = &mut self.inner.journaled_state;
            // inspect_account may fail if DB errors; treat as no refund.
            inspect_account(journal, address, false).ok().and_then(|account| {
                // Only refund if the account was created in this transaction (EIP-6780:
                // only same-TX-created accounts are actually destroyed by SELFDESTRUCT).
                // Use CreatedLocal flag which matches revm's is_created_locally() check.
                if !account.status.contains(revm::state::AccountStatus::CreatedLocal) {
                    return None;
                }
                // Count new storage slots: original was zero, current is non-zero.
                let slot_count = account
                    .storage
                    .values()
                    .filter(|slot| {
                        slot.original_value().is_zero() && !slot.present_value().is_zero()
                    })
                    .count() as u64;
                // +1 for the account itself (counted in before_frame_init) + slot count.
                Some(1 + slot_count)
            })
        } else {
            None
        };

        // Read before the destruction: it loads `target`, marking its entry warm. The beneficiary
        // load is the one the interpreter prices, so it follows the resident-entry coldness rule
        // described on [`Self::resident_entry_prices_cold`] — and the `inspect_account` above is
        // itself one of the ways `target` ends up resident but cold.
        let resident_entry_is_cold = self.resident_entry_prices_cold(&target);
        let mut result = self.inner.selfdestruct(address, target, skip_cold_load);
        if let Ok(ref mut state_load) = result {
            state_load.is_cold |= resident_entry_is_cold;
        }

        // Record state growth refund only on the first effective destruction.
        // Repeated SELFDESTRUCT on the same account still returns a result but with
        // `previously_destroyed == true` — refunding again would double-count.
        if let Some(refund) = selfdestruct_refund {
            if let Ok(ref state_load) = result {
                if !state_load.data.previously_destroyed {
                    self.additional_limit.borrow_mut().on_selfdestruct(refund);
                }
            }
        }

        result
    }

    /// `SLOAD` entry point for every spec `MegaETH` runs (all are Berlin+, where the interpreter
    /// calls this method rather than [`Host::sload`]).
    ///
    /// Oracle-contract reads carry `MegaETH`'s customizations (external value source, forced-cold
    /// access, gas-detention marking); they live in [`Self::oracle_sload`] so a non-oracle `SLOAD`
    /// pays only one spec check plus one address compare before delegating.
    fn sload_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
            return self.oracle_sload(address, key, skip_cold_load);
        }
        self.inner.sload_skip_cold_load(address, key, skip_cold_load)
    }

    fn sstore_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        self.inner.sstore_skip_cold_load(address, key, value, skip_cold_load)
    }

    /// The single beneficiary-marking site for account loads.
    ///
    /// Every account-reading opcode — BALANCE, EXTCODESIZE, EXTCODECOPY, EXTCODEHASH and the CALL
    /// family — reaches the journal through here, so marking here rather than in the instruction
    /// wrappers keeps each mark at the exact point the account is read (an opcode that runs out of
    /// gas before its load marks nothing) and makes a double mark structurally impossible.
    ///
    /// `account_load_marks_beneficiary` owns the one case where the loaded address alone does not
    /// decide the mark: a CALL-family EIP-7702 delegate hop.
    ///
    /// It is also the single account load the interpreter prices, so it owns the resident-entry
    /// coldness rule described on [`Self::resident_entry_prices_cold`].
    fn load_account_info_skip_cold_load(
        &mut self,
        address: Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountInfoLoad<'_>, LoadError> {
        let is_call_raw_operand = self.call_target_load_phase == CallTargetLoadPhase::RawOperand;
        if self.account_load_marks_beneficiary() {
            self.check_and_mark_beneficiary_balance_access(&address);
        }
        // Rex6+: the pre-revm-40 CALL-family host entry resolved the raw operand's EIP-7702
        // delegate before loading it, and that resolution materialized the operand's journal
        // entry — cold, code hydrated — as a side effect. The materialization is priced: the
        // load below then sees a resident entry and keeps the entry's own coldness (see
        // [`Self::resident_entry_prices_cold`]), where a fresh load would honor the pre-warmed
        // sets (precompiles, coinbase, address-only access-list entries). Reproduce it so a
        // Rex6 CALL-family first touch of a preload-warm address still prices cold. The
        // delegate address itself is not marked here: revm loads it as the delegate hop and
        // that load marks through this same entry point.
        if is_call_raw_operand && self.spec.is_enabled(MegaSpecId::REX6) {
            let _ = self.best_effort_resolve_eip7702_delegate_address(address);
        }
        // Read before the load: the load marks the entry warm.
        let resident_entry_is_cold = self.resident_entry_prices_cold(&address);
        let mut load =
            self.inner.load_account_info_skip_cold_load(address, load_code, skip_cold_load)?;
        load.is_cold |= resident_entry_is_cold;
        Ok(load)
    }

    // NOTE: `Host::sload` is deliberately NOT overridden. Its default implementation is
    // `self.sload_skip_cold_load(address, key, false).ok()`, so the oracle customizations reach
    // every caller of the legacy entry (still a public trait method for third parties) through the
    // single implementation above. Re-adding an override here would fork the oracle logic into two
    // bodies that can drift apart; keep it deleted.

    fn balance(&mut self, address: Address) -> Option<StateLoad<U256>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.balance(address)
    }

    /// Loads `address` and its EIP-7702 delegate, marking beneficiary access for both from Rex6.
    ///
    /// The CALL-family opcodes no longer reach this method: revm resolves the delegate inside the
    /// instruction and issues one [`Host::load_account_info_skip_cold_load`] call per address
    /// instead. This override therefore only serves direct callers of the trait method, and the
    /// `self.inner` delegation below deliberately keeps the inner context's own account loads out
    /// of this impl's marking hook so the address set marked here is exactly `address` plus, from
    /// Rex6, its delegate.
    fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        // Rex6+: also mark the EIP-7702 delegate of `address` if any, so a CALL whose
        // target delegates to the beneficiary triggers detention even though the raw stack
        // operand doesn't match. The Rex4 path only marked the raw input. A resolve DB error
        // falls back to the raw address (no delegate mark) — the `load_account_delegated` below
        // remains responsible for surfacing the failure.
        if self.spec.is_enabled(MegaSpecId::REX6) {
            let resolved = self.best_effort_resolve_eip7702_delegate_address(address);
            if resolved != address {
                self.check_and_mark_beneficiary_balance_access(&resolved);
            }
        }
        self.inner.load_account_delegated(address)
    }

    fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.load_account_code(address)
    }

    fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.load_account_code_hash(address)
    }
}

/// Which account load of a CALL-family opcode's target resolution comes next.
///
/// revm's `CALL` / `CALLCODE` / `DELEGATECALL` / `STATICCALL` body loads the raw stack operand and
/// then, when that account carries an EIP-7702 designation, its delegate. Both arrive as
/// [`Host::load_account_info_skip_cold_load`] with the same arguments, so they are
/// indistinguishable at the host boundary — yet beneficiary detention engages on the raw operand on
/// every spec and on the delegate only from `REX6`. The CALL-family handlers therefore bracket
/// revm's body with [`HostExt::begin_call_target_resolution`] /
/// [`HostExt::end_call_target_resolution`], and this phase tells the two loads apart inside the
/// bracket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CallTargetLoadPhase {
    /// No CALL-family target resolution is in flight: the loading opcode reads exactly the address
    /// it was handed, so the load always marks.
    #[default]
    Idle,
    /// A CALL-family body has been entered; its next account load is the raw stack operand.
    RawOperand,
    /// The raw stack operand has been loaded; any further load in this body is the delegate hop.
    DelegateHop,
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Whether the account load being issued may mark beneficiary access, advancing the CALL-family
    /// target-resolution phase past its raw operand.
    ///
    /// Outside a CALL-family body every load marks, and so does a CALL's raw stack operand. A
    /// CALL's EIP-7702 delegate hop marks only from `REX6`: up to `REX5` a CALL engages beneficiary
    /// detention on the raw operand alone, so calling a delegator that points at the beneficiary
    /// must leave the beneficiary unmarked.
    #[inline]
    fn account_load_marks_beneficiary(&mut self) -> bool {
        match self.call_target_load_phase {
            CallTargetLoadPhase::Idle => true,
            CallTargetLoadPhase::RawOperand => {
                self.call_target_load_phase = CallTargetLoadPhase::DelegateHop;
                true
            }
            CallTargetLoadPhase::DelegateHop => self.spec.is_enabled(MegaSpecId::REX6),
        }
    }

    /// Whether `address` must be priced as a cold access because the journal already holds an entry
    /// for it that is cold for the current transaction — no matter which pre-warmed address set
    /// `address` belongs to.
    ///
    /// A journal decides EIP-2929 coldness on two paths: materializing a fresh entry, where the
    /// pre-warmed sets (precompiles, coinbase, address-only access-list entries) make the first
    /// touch warm, and re-reading an entry that is already resident, where coldness comes from that
    /// entry's own transaction id / cold flag. `MegaETH` has always priced the second path from the
    /// entry alone: a resident-but-cold entry is a cold access even for a precompile. revm 40
    /// started consulting the pre-warmed sets on the resident path too, which silently made those
    /// accesses warm — the cold-access surcharge cheaper than every `MegaETH` spec charges.
    ///
    /// Resident-but-cold entries are routine here, so this is not a corner case: the CALL-family
    /// and `SELFDESTRUCT` storage-gas wrappers inspect the target through
    /// [`JournalInspectTr::inspect_account`] — which materializes the entry without warming it —
    /// before the opcode issues its own load, and a journal reused across the transactions of a
    /// block carries every entry earlier transactions materialized.
    ///
    /// [`Self::access_list_preloads_account`] is the one exemption: an account this transaction's
    /// access list pre-loads is warm from the transaction's first instruction onwards, and
    /// re-cooling it here would charge the cold surcharge on top of the access-list fee the
    /// sender already paid.
    ///
    /// Callers OR this into the `is_cold` the journal reports. That is a pricing-only correction:
    /// the journal still takes its resident-entry branch and still records the `account_warmed`
    /// entry that re-cools the account if the frame reverts, so nothing else about the load moves.
    #[inline]
    fn resident_entry_prices_cold(&self, address: &Address) -> bool {
        let journal = &self.inner.journaled_state;
        journal
            .state
            .get(address)
            .is_some_and(|account| account.is_cold_transaction_id(journal.transaction_id)) &&
            !self.access_list_preloads_account(address)
    }

    /// Whether this transaction's EIP-2930 access list pre-loads `address`, making every access to
    /// it in this transaction warm regardless of what the journal already holds for it.
    ///
    /// A listed entry counts as pre-loading its account only when it also carries at least one
    /// storage key. That is the shape revm 27 loaded eagerly in pre-execution — the account plus
    /// each listed slot, stamped with this transaction's id — so every later access to it was warm
    /// however the journal came by its entry. An entry that lists no storage keys loaded nothing:
    /// it only added the address to the pre-warmed set, which the resident path never
    /// consulted. So a resident entry for an address-only listing is still a cold first touch,
    /// and only the fresh-entry path sees such an address as warm.
    ///
    /// The map read here is per-transaction: the journal replaces it in pre-execution and clears it
    /// when the transaction is committed or discarded, so it never carries a neighbouring
    /// transaction's list. It is also empty for legacy transactions, which have no access list.
    #[inline]
    fn access_list_preloads_account(&self, address: &Address) -> bool {
        self.inner
            .journaled_state
            .inner
            .warm_addresses
            .access_list()
            .get(address)
            .is_some_and(|storage_keys| !storage_keys.is_empty())
    }

    /// `SLOAD` against the oracle contract on `MINI_REX`+ — the single home of `MegaETH`'s three
    /// oracle storage-read customizations. Called from [`Host::sload_skip_cold_load`], which owns
    /// the address/spec predicate; [`Host::sload`] reaches it through the same method via its
    /// default implementation, so the legacy and current entries cannot diverge.
    ///
    /// 1. The value comes from the oracle environment when it has one, falling back to the journal.
    /// 2. The access is forced cold either way: the current execution may be a replay of an
    ///    existing block and there is no way to tell whether the payload builder served the slot
    ///    from `oracle_env` or from state, so a single (cold) price keeps the gas cost consistent.
    /// 3. `REX3`+ marks the read in the volatile-data tracker, which is what lets the `SLOAD`
    ///    instruction wrapper lower the remaining compute gas (gas detention). Transactions sent by
    ///    the mega system address are exempt.
    ///
    /// # `skip_cold_load` vs. forced-cold
    ///
    /// `skip_cold_load` is the interpreter saying "I have less gas left than a cold load's
    /// surcharge, so do not pay for one". Because rule 2 makes every oracle read cold, that hint is
    /// always decisive here, and reporting [`LoadError::ColdLoadSkipped`] (which the interpreter
    /// turns into `OutOfGas`) is exactly where the read would have landed anyway: the interpreter
    /// sets `skip_cold_load` on precisely the condition under which its own cold-surcharge charge
    /// fails. So the outcome is unchanged from before the parameter existed — serve the value,
    /// force `is_cold = true`, run out of gas on the surcharge — while skipping an
    /// oracle/journal load whose result would be discarded. Neither under- nor over-charging is
    /// possible: no oracle read is ever served below the cold price, and no read is refused
    /// that could have paid it.
    ///
    /// Note that the tracker mark happens *before* that bail. The pre-`skip_cold_load` `SLOAD`
    /// called into the host first and charged the cold cost afterwards, so an unaffordable oracle
    /// read still counted as an oracle access; marking first preserves that.
    #[inline(never)]
    fn oracle_sload(
        &mut self,
        address: Address,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        debug_assert!(
            self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS,
            "oracle_sload is only reachable through the MINI_REX+ oracle-address predicate",
        );

        // Rex3+: Mark oracle access for gas detention on SLOAD rather than CALL.
        // The actual gas limit enforcement happens in the SLOAD instruction wrapper
        // (detain_gas_ext::sload in instructions.rs).
        // Mega system address transactions are exempted from oracle gas detention.
        // Note: This checks the transaction sender (from TxEnv) via Host::caller(),
        // unlike the pre-Rex3 CALL-based path which checked the frame-level caller.
        if self.spec.is_enabled(MegaSpecId::REX3) && self.caller() != self.system_address {
            self.volatile_data_tracker.borrow_mut().check_and_mark_oracle_access(&address);
        }

        // The read is cold by construction, so an interpreter that cannot afford a cold load
        // cannot afford this one.
        if skip_cold_load {
            return Err(LoadError::ColdLoadSkipped);
        }

        // If the oracle env provides a value, return it. Otherwise, fall back to the inner context.
        if let Some(value) = self.oracle_env.borrow().get_oracle_storage(key) {
            // Accessing oracle contract storage is forced to be cold access, since it always
            // reads from the outside world (oracle_env).
            return Ok(StateLoad::new(value, true));
        }

        // `false`, not `skip_cold_load`: the cold-skip decision was already made above, on the
        // forced-cold premise. The journal's own warm/cold view is not the one being charged, so it
        // must not get a second, contradicting vote — a slot the journal considers warm would skip
        // nothing yet still be charged cold.
        let mut state_load = self.inner.sload_skip_cold_load(address, key, false)?;
        // It is indistinguishable to tell whether a storage access of oracle contract is warm or
        // not even if it is loaded from the inner journal state. This is because the current
        // execution may be a replay of existing blocks and we cannot know whether the payload
        // builder read from the oracle_env or not. So we force such sload always to be cold access
        // to ensure consistent gas cost.
        state_load.is_cold = true;
        Ok(state_load)
    }
}

/// Extension trait for the `Host` trait that provides additional functionality for `MegaETH`.
///
/// Gas cost methods (`sstore_set_storage_gas`, `new_account_storage_gas`,
/// `create_contract_storage_gas`) follow the same error-handling pattern as revm's `Host` trait:
/// on error, stash the error in `self.error()` and return `None`.
/// This ensures that `FatalExternalError` always has a stashed error for revm to drain.
pub trait HostExt: Host {
    /// Gets the `MegaSpecId` of the current execution context.
    fn spec_id(&self) -> MegaSpecId;

    /// Gets the `AdditionalLimit` instance. Only used when the `MINI_REX` spec is enabled.
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>>;

    /// Gets the gas cost for setting a storage slot to a non-zero value. Only used when the
    /// `MINI_REX` spec is enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn sstore_set_storage_gas(&mut self, address: Address, key: U256) -> Option<u64>;

    /// Gets the gas cost for creating a new account. Only used when the `MINI_REX` spec is enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn new_account_storage_gas(&mut self, address: Address) -> Option<u64>;

    /// Gets the gas cost for creating a new contract. Only used when the `REX` spec is
    /// enabled.
    ///
    /// Returns `None` if the underlying SALT environment returns an error (the error is stashed
    /// in `self.error()`).
    fn create_contract_storage_gas(&mut self, address: Address) -> Option<u64>;

    /// Gets the volatile data tracker. Only used when the `MINI_REX` spec is enabled.
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>>;

    /// Checks if volatile data access should cause a revert at the current call depth.
    /// Returns `true` if `disableVolatileDataAccess()` was called and the current
    /// journal depth is deeper than the activation depth.
    fn volatile_access_disabled(&self) -> bool;

    /// Returns the block beneficiary address without triggering volatile data tracking.
    /// Used by instruction handlers to pre-check whether an opcode targets the beneficiary.
    fn beneficiary_address(&self) -> Address;

    /// Resolves the EIP-7702 delegate of `address` one hop on a best-effort basis, returning
    /// `address` itself when there is no delegate or when the resolve hits a DB error.
    ///
    /// Unlike [`JournalInspectTr::resolve_eip7702_delegate_address`], a DB failure here is NOT
    /// stashed in `self.error()`. This is for prechecks and side-marks (the beneficiary-volatile
    /// guard, the detention side-mark) that compare the delegate against a known address and must
    /// never turn a transaction into an error on their own — e.g. a malformed CALL that underflows
    /// before it ever runs the target must keep its `StackUnderflow`, not a spurious DB error from
    /// eagerly loading a delegate's code it never needs. The opcode's real execution path reads the
    /// account again and owns surfacing any genuine DB error.
    fn best_effort_resolve_eip7702_delegate_address(&mut self, address: Address) -> Address;

    /// Opens a CALL-family target-resolution scope: from here until
    /// [`Self::end_call_target_resolution`], the account loads reaching the host are the CALL's raw
    /// stack operand followed, when that operand carries an EIP-7702 designation, by its delegate.
    ///
    /// The host needs the distinction because beneficiary detention engages on the raw operand on
    /// every spec but on the delegate only from `REX6`, while both loads look identical at the
    /// [`Host`] boundary. Every handler that runs revm's CALL-family body must open the scope and
    /// close it on all exit paths; scopes never nest, because an opcode body never runs another
    /// opcode.
    fn begin_call_target_resolution(&mut self);

    /// Closes the scope opened by [`Self::begin_call_target_resolution`], so subsequent account
    /// loads are attributed to the opcode that issues them again.
    fn end_call_target_resolution(&mut self);
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> HostExt for MegaContext<DB, ExtEnvs> {
    #[inline]
    fn spec_id(&self) -> MegaSpecId {
        self.spec
    }

    #[inline]
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        &self.additional_limit
    }

    #[inline]
    fn sstore_set_storage_gas(&mut self, address: Address, key: U256) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        // System-tx exemption (REX6+ `LimitCheck::Exempt` stamp): charge un-scaled (min-bucket)
        // storage gas so the write never depends on SALT bucket capacity and can never OOG as
        // buckets grow. This path also avoids querying the SALT env.
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().sstore_set_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().sstore_set_gas(address, key);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn new_account_storage_gas(&mut self, address: Address) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().new_account_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().new_account_gas(address);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn create_contract_storage_gas(&mut self, address: Address) -> Option<u64> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        if self.additional_limit.borrow().has_exceeded_limit.is_exempt() {
            return Some(self.dynamic_storage_gas_cost.borrow().create_contract_gas_unscaled());
        }
        let result = self.dynamic_storage_gas_cost.borrow_mut().create_contract_gas(address);
        result
            .map_err(|e| {
                *self.error() = Err(ContextError::Custom(format!("{e}")));
            })
            .ok()
    }

    #[inline]
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>> {
        &self.volatile_data_tracker
    }

    #[inline]
    fn volatile_access_disabled(&self) -> bool {
        let current_depth = self.journal_ref().depth();
        self.volatile_data_tracker.borrow().volatile_access_disabled(current_depth)
    }

    #[inline]
    fn beneficiary_address(&self) -> Address {
        self.inner.block.beneficiary
    }

    #[inline]
    fn best_effort_resolve_eip7702_delegate_address(&mut self, address: Address) -> Address {
        // Resolve through the journal directly so a DB error propagates as `Err` here (and is
        // discarded) rather than being stashed into `self.error()` by
        // `MegaContext::inspect_account`.
        let spec = self.spec;
        self.inner
            .journaled_state
            .resolve_eip7702_delegate_address(spec, address)
            .unwrap_or(address)
    }

    #[inline]
    fn begin_call_target_resolution(&mut self) {
        debug_assert_eq!(
            self.call_target_load_phase,
            CallTargetLoadPhase::Idle,
            "CALL-family target-resolution scopes must not nest",
        );
        self.call_target_load_phase = CallTargetLoadPhase::RawOperand;
    }

    #[inline]
    fn end_call_target_resolution(&mut self) {
        self.call_target_load_phase = CallTargetLoadPhase::Idle;
    }
}

/// Trait to inspect the journal's internal state without marking any accounts or storage slots as
/// warm.
///
/// # EIP-7702 address semantics
///
/// Address handling is intentionally split by call-site purpose. The guiding rule: follow the
/// delegate ONLY to (1) execute the delegate's code and (2) decide whether an operand observes the
/// block beneficiary's state. Everything that *attributes* balance / nonce / storage / state-growth
/// / data-size uses the ORIGINAL address — a delegated account's state is its own, not the
/// delegate's. Before adding a new 7702-touching call site, find its row in this table and use the
/// listed address; do not re-derive the raw-vs-delegate decision ad-hoc.
///
/// | Purpose | Address | Mechanism | Spec gate |
/// | --- | --- | --- | --- |
/// | Code / execution target | delegate | revm's `load_account_delegated` (not these primitives) | revm-owned |
/// | Storage / SALT / state-growth / account-write accounting | original | `inspect_account` | REX5+ original; pre-REX5 followed the delegate (frozen) |
/// | CALL-family beneficiary / volatile-access check | delegate | `resolve_eip7702_delegate_address` | REX6+ only; `<=` REX5 compares the raw operand (frozen) |
/// | Validate-time authorization accounting | original authority | `inspect_account` with `load_code = true` (EIP-7702 / EIP-3607 detection) | scan-wide |
///
/// `inspect_account_delegated` follows the delegate; it backs the frozen pre-REX5 accounting path
/// and any caller that genuinely needs the delegate's account. New accounting call sites should use
/// `inspect_account` (original address), not this.
///
/// To improve performance, when journal does not have the account or storage slot, it will be
/// loaded from the database and cached in the journal.
/// However, since we explicitly mark the account or storage slot as cold, this pre-loading before
/// executing the original instruction will make no difference on gas cost.
///
/// Both `Journal<DB>` and `MegaContext` implement this trait:
/// - `Journal<DB>`: `DBError = DB::Error` — returns DB errors for propagation.
/// - `MegaContext`: `DBError = ()` — stashes errors in `self.error()` and returns `Err(())`.
pub trait JournalInspectTr {
    /// The error type returned on DB failures.
    type DBError: core::fmt::Debug;

    /// Inspect the account at the given address without marking it as warm and without
    /// following EIP-7702 delegation.
    ///
    /// Loads the account from the database into the journal cache (so subsequent
    /// in-block reads see this committed state), then explicitly marks it cold so the
    /// inspection does not show up in EIP-2929's access list and produces no
    /// `account_warmed` journal entry. Use this for metering inspections where the
    /// authority's own state matters (e.g., new-account storage-gas premium, SALT
    /// bucket lookup, state-growth emptiness check) rather than the delegate's state,
    /// and for validate-path reads (nonce, code) that must not participate in the
    /// access-list accounting the execution path will perform later.
    ///
    /// When `load_code` is `true`, additionally invokes `code_by_hash` if the database
    /// left `info.code` lazy (production reth-style `StateProviderDatabase::basic`
    /// returns `code: None` for accounts with on-chain bytecode, deferring code load).
    /// Set this only on call sites that read `info.code` (EIP-7702 detection, EIP-3607
    /// check) — the cheaper `false` path skips `code_by_hash` for every other cold
    /// first-touch. Parallels revm's `JournalTr::load_account_optional(.., load_code,
    /// ..)` shape.
    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the account at the given address without marking it as warm.
    /// If the account is EIP-7702 type, follows delegation.
    ///
    /// Starting from REX4, resolves exactly one hop (matching upstream revm behavior).
    /// Pre-REX4, follows delegation recursively but detects cycles to prevent stack overflow.
    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the storage at the given address and key without marking it as warm.
    ///
    /// Starting from REX4, storage is always loaded from the original address without following
    /// EIP-7702 delegation (matching upstream revm's sload behavior).
    /// Pre-REX4 specs retain the original behavior that follows delegation.
    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError>;

    /// Resolve the EIP-7702 delegate of `address` one hop and return the target address.
    ///
    /// Returns `address` itself when the account is not EIP-7702-delegated. Hydrates `info.code`
    /// on REX5+ so EIP-7702 detection works against lazy-code databases (mirrors
    /// `inspect_account_delegated`'s rule for stable-spec freeze).
    ///
    /// Callers gate this on REX6 (the only place it is used). EIP-7702 does not exist on earlier
    /// specs, so an account there simply has no delegate code and resolves to `address` — no
    /// pre-REX4 special case is needed.
    ///
    /// Useful for instruction-wrapper checks that need to compare the effective code-running
    /// address against a known target (e.g., the block beneficiary) before delegating to revm.
    fn resolve_eip7702_delegate_address(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<Address, Self::DBError> {
        let load_code = spec.is_enabled(MegaSpecId::REX5);
        let account = self.inspect_account(address, load_code)?;
        let delegate = account.info.code.as_ref().and_then(Bytecode::eip7702_address);
        Ok(delegate.unwrap_or(address))
    }
}

/// Load an account into the journal cache without following EIP-7702 delegation
/// and mark it cold. When `load_code` is `true`, additionally invokes
/// `code_by_hash` if the database left `info.code` lazy.
///
/// The occupied branch's `code_by_hash` hydration is load-bearing and must
/// stay: removing it would shift observable behavior on stable specs (a second
/// `inspect_account` against the same lazy-code DB would no longer see hydrated
/// code).
fn inspect_account<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
    load_code: bool,
) -> Result<&mut Account, <DB as revm::Database>::Error> {
    let transaction_id = journal.transaction_id;
    match journal.inner.state.entry(address) {
        Entry::Occupied(entry) => {
            let account = entry.into_mut();
            if account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                // Load code if not loaded before
                account.info.code = Some(journal.database.code_by_hash(account.info.code_hash)?);
            }
            Ok(account)
        }
        Entry::Vacant(entry) => {
            let mut account = journal
                .database
                .basic(address)?
                .map(|info| info.into())
                .unwrap_or_else(|| Account::new_not_existing(transaction_id));
            if load_code && account.info.code_hash != KECCAK_EMPTY && account.info.code.is_none() {
                account.info.code = Some(journal.database.code_by_hash(account.info.code_hash)?);
            }
            // deliberately mark the account as cold since we are only inspecting it, not warming
            // it.
            account.mark_cold();
            Ok(entry.insert(account))
        }
    }
}

/// Cold occupancy read that returns an account's `code_hash` and loads it into the journal (so it
/// appears in the returned `EvmState` / witness) but — unlike [`inspect_account`] — never hydrates
/// `info.code`, even when the account is already resident (`inspect_account`'s already-loaded
/// branch force-loads code via `code_by_hash` regardless of its `load_code` argument). An occupancy
/// check needs only the hash; forcing `code_by_hash` on an already-warmed occupied address would
/// require its bytecode in a stateless witness that need only carry the account proof, turning the
/// expected revert into a spurious DB error on replay.
///
/// Only [`Journal`] needs this (the REX6 keyless-deploy occupancy check calls it on
/// `ctx.journal_mut()`), so it is a free function rather than a `JournalInspectTr` method.
pub(crate) fn inspect_account_code_hash<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
) -> Result<B256, <DB as revm::Database>::Error> {
    let transaction_id = journal.transaction_id;
    match journal.inner.state.entry(address) {
        Entry::Occupied(entry) => Ok(entry.get().info.code_hash),
        Entry::Vacant(entry) => {
            let mut account = journal
                .database
                .basic(address)?
                .map(|info| info.into())
                .unwrap_or_else(|| Account::new_not_existing(transaction_id));
            account.mark_cold();
            Ok(entry.insert(account).info.code_hash)
        }
    }
}

impl<DB: revm::Database> JournalInspectTr for Journal<DB> {
    type DBError = <DB as revm::Database>::Error;

    fn inspect_account(
        &mut self,
        address: Address,
        load_code: bool,
    ) -> Result<&mut Account, Self::DBError> {
        inspect_account(self, address, load_code)
    }

    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, Self::DBError> {
        // REX5+ hydrates code before the 7702 detection below; pre-REX5 must not —
        // stable specs preserve the latent lazy-DB EIP-7702 detection gap.
        let is_rex5_enabled = spec.is_enabled(MegaSpecId::REX5);

        let account = inspect_account(self, address, is_rex5_enabled)?;

        let delegated_address = account.info.code.as_ref().and_then(Bytecode::eip7702_address);
        let Some(delegated_address) = delegated_address else {
            // Not delegated — reload to satisfy borrow checker and return.
            let account = self.inner.state.get_mut(&address).unwrap();
            return Ok(account);
        };

        if spec.is_enabled(MegaSpecId::REX4) {
            // REX4+: resolve exactly one hop (matching upstream revm behavior).
            return inspect_account(self, delegated_address, is_rex5_enabled);
        }

        // Pre-REX4: follow delegation recursively with cycle detection.
        // Stays on non-hydrating `inspect_account` (load_code = false) deliberately —
        // pre-REX4 is pre-REX5, so the lazy-DB EIP-7702 detection gap is the frozen
        // behavior on these specs.
        let mut current = delegated_address;
        let mut visited = std::vec![address];
        loop {
            let account = inspect_account(self, current, false)?;
            let next = account.info.code.as_ref().and_then(Bytecode::eip7702_address);
            let Some(next) = next else {
                // End of chain — reload and return.
                let account = self.inner.state.get_mut(&current).unwrap();
                return Ok(account);
            };
            if visited.contains(&next) {
                // Cycle detected — stop here.
                let account = self.inner.state.get_mut(&current).unwrap();
                return Ok(account);
            }
            visited.push(current);
            current = next;
        }
    }

    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError> {
        let transaction_id = self.transaction_id;
        let is_rex4_enabled = spec.is_enabled(MegaSpecId::REX4);
        // EIP-7702 storage semantics: storage belongs to the original address (delegator),
        // not the delegate. So `is_created` must be checked on the original address — an
        // EOA delegating via 7702 is never CREATEd, so its flag is always false. Checking
        // the delegate's flag instead would mistakenly short-circuit storage reads when the
        // delegate happens to be a freshly-CREATEd contract in the same tx, corrupting
        // SSTORE accounting (gas / kv_updates / data_size) on the delegator's slots.
        // Fold two inspect_account calls into one hydrating load on REX4 path.
        // inspect_account's occupied branch hydrates lazy code unconditionally,
        // so the old second (always-occupied) pass hydrated the same code that
        // load_code=true hydrates inline here — identical final account state,
        // identical DB-call sequence and error position, one fewer state.entry lookup.
        // Newly-created accounts must short-circuit storage misses to ZERO before any DB call.
        // Querying here would otherwise trigger a witness lookup for a slot with no meaningful
        // pre-state value, which breaks stateless replay when CREATE lands on a pre-funded
        // address: its `Loaded` cache status bypasses revm's `State::storage` short-circuit and
        // exposes the call to the witness backend.
        let is_newly_created;
        let account = if is_rex4_enabled {
            let account = inspect_account(self, address, true)?;
            is_newly_created = account.is_created();
            debug_assert!(account.info.code_hash == KECCAK_EMPTY || account.info.code.is_some());
            account
        } else {
            // Non-REX4: is_created must be read on the original address (an EOA delegating
            // via 7702 is never CREATEd), but the storage account follows delegation —
            // genuinely two different accounts, so the two loads cannot be folded.
            is_newly_created = inspect_account(self, address, false)?.is_created();
            self.inspect_account_delegated(spec, address)?
        };
        // REX4/REX5 hot path: use entry() API for a single HashMap probe.
        // The prologue guarantees the account is in inner.state; reload via get_mut
        // to narrow the borrow from &mut self to &mut inner.state, keeping
        // self.database reachable for the miss path.
        if is_rex4_enabled {
            let account = self.inner.state.get_mut(&address).unwrap();
            return match account.storage.entry(key) {
                Entry::Occupied(entry) => Ok(entry.into_mut()),
                Entry::Vacant(entry) => {
                    let slot_value = if is_newly_created {
                        U256::ZERO
                    } else {
                        self.database.storage(address, key)?
                    };
                    let mut slot = EvmStorageSlot::new(slot_value, transaction_id);
                    slot.mark_cold();
                    Ok(entry.insert(slot))
                }
            };
        }

        // Pre-REX4: original contains_key + reload pattern (genuinely two different accounts).
        if account.storage.contains_key(&key) {
            // Need to reload account to satisfy borrow checker.
            let account = self.inspect_account_delegated(spec, address)?;
            return Ok(account.storage.get(&key).unwrap());
        }
        // Slot doesn't exist. For newly-created accounts, post-CREATE storage is
        // guaranteed empty (EIP-161 / EIP-6780), so return ZERO without touching the DB.
        let slot_value =
            if is_newly_created { U256::ZERO } else { self.database.storage(address, key)? };
        let mut slot = EvmStorageSlot::new(slot_value, transaction_id);
        // deliberately mark the slot as cold since we are only inspecting it, not warming it
        slot.mark_cold();
        // Load account again to bypass the borrow checker and insert the slot
        let account = self.inspect_account_delegated(spec, address)?;
        account.storage.insert(key, slot);
        // Return reference to the newly inserted slot
        Ok(account.storage.get(&key).expect("slot should exist"))
    }
}

/// `MegaContext` delegates to `Journal<DB>` and stashes DB errors via `self.error()`.
///
/// On DB error, the real error is stashed as `ContextError::Custom` and `Err(())` is returned.
/// Callers should halt with `FatalExternalError` when receiving `Err`.
impl<DB: Database, ExtEnvs: ExternalEnvTypes> JournalInspectTr for MegaContext<DB, ExtEnvs> {
    type DBError = ();

    fn inspect_account(&mut self, address: Address, load_code: bool) -> Result<&mut Account, ()> {
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_account(address, load_code).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }

    fn inspect_account_delegated(
        &mut self,
        spec: MegaSpecId,
        address: Address,
    ) -> Result<&mut Account, ()> {
        // Split borrow: `journaled_state` and `error` are sibling fields on the inner context,
        // so we can borrow them independently to avoid the double-call workaround.
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_account_delegated(spec, address).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }

    fn inspect_storage(
        &mut self,
        spec: MegaSpecId,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, ()> {
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_storage(spec, address, key).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};
    use core::cell::Cell;
    use revm::{
        primitives::HashMap,
        state::{AccountInfo, Bytecode},
        Database,
    };

    /// Minimal `revm::Database` implementation that mimics the production
    /// `reth`-style `StateProviderDatabase` contract: `basic()` returns
    /// `AccountInfo { code: None, code_hash: <real hash>, account_id: None }` for accounts with
    /// on-chain bytecode, and the bytecode itself is lazy-loaded on demand via
    /// `code_by_hash()`. The workspace's `MemoryDatabase` cannot model this —
    /// it eagerly populates `AccountInfo.code` inside `basic()`, so any cache
    /// miss against it would always see the code already hydrated.
    #[derive(Default, Debug)]
    struct LazyCodeDatabase {
        accounts: HashMap<Address, AccountInfo>,
        codes: HashMap<B256, Bytecode>,
        storage_calls: Cell<usize>,
    }

    impl LazyCodeDatabase {
        fn with_account_code(mut self, address: Address, bytecode: Bytes) -> Self {
            let code = Bytecode::new_raw(bytecode);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash,
                    code: None,
                    account_id: None,
                },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn with_eip7702_delegation(mut self, address: Address, delegate: Address) -> Self {
            let code = Bytecode::new_eip7702(delegate);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash,
                    code: None,
                    account_id: None,
                },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn storage_calls(&self) -> usize {
            self.storage_calls.get()
        }
    }

    impl revm::Database for LazyCodeDatabase {
        type Error = core::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            // Mirror reth's `StateProviderDatabase::basic`: return AccountInfo without
            // populating `code`, even when the account has on-chain bytecode.
            Ok(self.accounts.get(&address).cloned())
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(self.codes.get(&code_hash).cloned().unwrap_or_default())
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            self.storage_calls.set(self.storage_calls.get() + 1);
            Ok(U256::ZERO)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    /// `inspect_account(addr, false)` must not hydrate `info.code` on the vacant
    /// branch — callers that need it pass `load_code = true`.
    #[test]
    fn test_inspect_account_vacant_path_does_not_hydrate_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]); // PUSH1 1 PUSH1 1 ADD
        let expected_hash = keccak256(&bytecode);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let account =
            inspect_account(&mut journal, ADDR, false).expect("inspect_account must succeed");

        assert_eq!(
            account.info.code_hash, expected_hash,
            "code_hash must propagate from the database's `basic()` result",
        );
        assert!(
            account.info.code.is_none(),
            "`load_code = false` must leave `info.code` as-is on the vacant branch",
        );
    }

    /// `inspect_account(addr, true)` hydrates `info.code` from `code_by_hash` on
    /// first cold inspection against a lazy-code database.
    #[test]
    fn test_inspect_account_with_load_code_hydrates_lazy_bytecode_on_first_touch() {
        const ADDR: Address = address!("00000000000000000000000000000000000000aa");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);

        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode.clone());
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, ADDR, true)
            .expect("inspect_account must succeed on first cold-touch");
        let hydrated = account
            .info
            .code
            .as_ref()
            .expect("`load_code = true` must populate `info.code` from code_by_hash");
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            bytecode.as_ref(),
            "hydrated bytecode must match what `code_by_hash` would return",
        );
    }

    /// The occupied-branch hydration must keep firing — a second `inspect_account`
    /// against the same lazy-DB address must observe hydrated `info.code` even with
    /// `load_code = false`.
    #[test]
    fn test_inspect_account_occupied_branch_hydrates_on_second_inspection() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let first_code_hash = inspect_account(&mut journal, ADDR, false)
            .expect("first inspection must succeed")
            .info
            .code_hash;
        let second =
            inspect_account(&mut journal, ADDR, false).expect("second inspection must succeed");

        assert_eq!(
            second.info.code_hash, first_code_hash,
            "code_hash must be identical across cache miss and cache hit",
        );
        assert!(
            second.info.code.is_some(),
            "second inspection must observe the hydrated code via the occupied-branch \
             `code_by_hash` load",
        );
    }

    /// `inspect_account(addr, true)` must short-circuit on EOAs — the `code_hash !=
    /// KECCAK_EMPTY` guard keeps `code_by_hash` off the hot path.
    #[test]
    fn test_inspect_account_with_load_code_leaves_eoa_code_empty() {
        const EOA: Address = address!("00000000000000000000000000000000000000cc");

        let mut db = LazyCodeDatabase::default();
        db.accounts.insert(
            EOA,
            AccountInfo {
                balance: U256::from(1_000_000u64),
                nonce: 5,
                code_hash: KECCAK_EMPTY,
                code: None,
                account_id: None,
            },
        );
        let mut journal = Journal::new(db);

        let account = inspect_account(&mut journal, EOA, true)
            .expect("inspect_account must succeed and be a no-op on EOAs");
        assert_eq!(account.info.code_hash, KECCAK_EMPTY, "EOA code_hash must remain KECCAK_EMPTY");
        assert!(
            account.info.code.is_none(),
            "EOA code must stay `None`; the `code_hash != KECCAK_EMPTY` guard keeps \
             `code_by_hash` off the hot path for accounts without on-chain code",
        );
    }

    /// `inspect_account_code_hash` returns the account's `code_hash` but must NEVER hydrate
    /// `info.code` — on either branch. In particular the occupied branch must not fall through to
    /// `code_by_hash` the way `inspect_account(.., false)` does (see
    /// `test_inspect_account_occupied_branch_hydrates_on_second_inspection`). This is what lets the
    /// REX6 keyless-deploy occupancy check decide on the hash alone, without demanding an
    /// already-warmed occupied address's bytecode in a stateless witness carrying only its proof.
    #[test]
    fn test_inspect_account_code_hash_never_hydrates_code() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x5b]); // JUMPDEST
        let expected_hash = keccak256(&bytecode);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        // Vacant cache-miss: returns the hash from `basic()` without hydrating code.
        let vacant_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("vacant read must succeed");
        assert_eq!(vacant_hash, expected_hash, "vacant branch must return the code_hash");
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "vacant branch must not hydrate info.code",
        );

        // The address is now resident with `code == None`. `inspect_account` would hydrate on this
        // occupied branch; `inspect_account_code_hash` must not.
        let occupied_hash =
            inspect_account_code_hash(&mut journal, ADDR).expect("occupied read must succeed");
        assert_eq!(
            occupied_hash, expected_hash,
            "occupied branch must return the cached code_hash"
        );
        assert!(
            journal.inner.state.get(&ADDR).is_some_and(|a| a.info.code.is_none()),
            "inspect_account_code_hash must NOT hydrate info.code on the occupied branch",
        );
    }

    /// `resolve_eip7702_delegate_address` hydrates lazy delegation bytecode only from REX5:
    /// pin both sides of the boundary so a shifted gate (e.g. REX4) fails here. At REX4 a
    /// lazy-code delegator resolves to itself (its `0xef0100…` code is never loaded); at REX5
    /// the same first cold touch loads the code and follows the hop.
    #[test]
    fn test_resolve_eip7702_delegate_loads_code_from_rex5_exactly() {
        const DELEGATOR: Address = address!("00000000000000000000000000000000000000e1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000e2");

        // REX4: no hydration — the delegation is invisible and resolution degrades to identity.
        let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, DELEGATE);
        let mut journal = Journal::new(db);
        let resolved = journal
            .resolve_eip7702_delegate_address(MegaSpecId::REX4, DELEGATOR)
            .expect("resolve must succeed");
        assert_eq!(resolved, DELEGATOR, "REX4 must not load code, so the hop is not followed");
        assert!(
            journal.inner.state.get(&DELEGATOR).is_some_and(|a| a.info.code.is_none()),
            "REX4 resolution must leave the delegator's code lazy",
        );

        // REX5: the first cold touch hydrates the delegation bytecode and follows the hop.
        let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, DELEGATE);
        let mut journal = Journal::new(db);
        let resolved = journal
            .resolve_eip7702_delegate_address(MegaSpecId::REX5, DELEGATOR)
            .expect("resolve must succeed");
        assert_eq!(resolved, DELEGATE, "REX5 must hydrate the code and follow the hop");
    }

    /// On REX5+, `inspect_account_delegated` must follow the EIP-7702 hop on the
    /// very first cold inspection against a lazy-code database. Regression guard:
    /// any refactor that re-introduces a code-None branch silently degrades the
    /// walk to "treat the delegator as a regular EOA".
    #[test]
    fn test_inspect_account_delegated_follows_eip7702_on_cold_first_touch() {
        use revm::context::JournalTr;

        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000d2");
        let delegate_bytecode = Bytes::from_static(&[0x60, 0x42, 0x60, 0x00, 0x55]); // PUSH1 0x42 PUSH1 0 SSTORE

        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_bytecode.clone());

        let mut journal = Journal::new(db);

        let resolved = journal
            .inspect_account_delegated(MegaSpecId::REX5, DELEGATOR)
            .expect("inspect_account_delegated must succeed on a cold-cache first touch");

        // The resolved account must be the delegate, not the delegator. The only way
        // to distinguish them is the code: the delegator's code is the EIP-7702
        // designation pointing at DELEGATE; the delegate's code is the raw bytecode.
        let hydrated = resolved.info.code.as_ref().expect(
            "delegate's bytecode must be hydrated by the inner inspect_account call — \
             without the vacant-path hydration, the cold-touch on DELEGATE would leave \
             code as None and any subsequent EIP-7702 walk would see a wrongly-empty target",
        );
        assert!(
            !hydrated.is_eip7702(),
            "resolved account must NOT be the delegator (whose code is the EIP-7702 \
             designation); got: {hydrated:?}",
        );
        assert_eq!(
            hydrated.original_bytes().as_ref(),
            delegate_bytecode.as_ref(),
            "resolved account's code must match the delegate's raw bytecode — confirms \
             the delegation was followed exactly one hop",
        );
    }

    /// Pre-REX5 `inspect_account_delegated` must NOT hydrate `info.code` — the
    /// latent EIP-7702 lazy-DB detection gap is the frozen observable behavior of
    /// stable specs (hydrating would flip `state_clear_aware_is_empty` and the
    /// SALT-bucket nonce in CALL/CREATE/state-growth, breaking spec immutability).
    #[test]
    fn test_inspect_account_delegated_does_not_hydrate_pre_rex5() {
        use revm::context::JournalTr;

        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000d2");
        let delegate_bytecode = Bytes::from_static(&[0x60, 0x42, 0x60, 0x00, 0x55]);

        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_bytecode);

        let mut journal = Journal::new(db);

        // REX4 (pre-REX5): the one-hop walk runs but `code = None` against the
        // lazy DB hides the 7702 designation, so the walk returns the delegator.
        let resolved = journal
            .inspect_account_delegated(MegaSpecId::REX4, DELEGATOR)
            .expect("inspect_account_delegated must succeed on pre-REX5");

        assert!(
            resolved.info.code.is_none(),
            "pre-REX5: `inspect_account_delegated` must NOT hydrate `info.code` — the \
             latent lazy-DB EIP-7702 detection gap on stable specs is intentionally \
             preserved. Resolved account's code: {:?}",
            resolved.info.code,
        );
    }

    /// Pins the `LazyCodeDatabase` fixture's contract against the production
    /// `revm::Database` shape it is modeling: `basic()` returns
    /// `code: None` for known accounts, `None` for unknown addresses;
    /// `code_by_hash()` falls back to empty bytecode for an unknown hash;
    /// `storage()` and `block_hash()` are inert stubs (no tests exercise them via
    /// `inspect_account`, but they must remain wired so the fixture is a complete
    /// `revm::Database`). If the fixture ever drifts (e.g. someone "helpfully"
    /// makes `basic()` eagerly populate `code` like `MemoryDatabase` does), the
    /// inspect-account tests above silently lose their load-bearing property —
    /// this test fails fast in that case.
    #[test]
    fn test_lazy_code_database_fixture_pins_reth_style_contract() {
        const KNOWN: Address = address!("00000000000000000000000000000000000000ee");
        let bytecode = Bytes::from_static(&[0x00]);
        let mut db = LazyCodeDatabase::default().with_account_code(KNOWN, bytecode);

        let known = db.basic(KNOWN).unwrap().expect("known account must resolve");
        assert!(
            known.code.is_none(),
            "LazyCodeDatabase::basic must NOT pre-populate code — that is the \
             behavior `inspect_account` is being tested against",
        );
        assert!(
            db.basic(Address::ZERO).unwrap().is_none(),
            "unknown address must return None from basic()",
        );

        let unknown_hash = keccak256([0xffu8]);
        assert_eq!(
            db.code_by_hash(unknown_hash).unwrap().original_bytes().len(),
            0,
            "unknown code_hash must fall back to empty bytecode",
        );

        assert_eq!(db.storage(KNOWN, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(db.block_hash(0).unwrap(), B256::ZERO);
    }

    // === inspect_storage coverage tests (PR #334) ===

    #[test]
    fn test_inspect_storage_rex4_slot_hit_returns_existing_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000bb");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(3);
        let expected_value = U256::from(42);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected_value, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on existing slot");

        assert_eq!(
            slot.present_value, expected_value,
            "REX4 slot hit must return the pre-seeded value"
        );
        assert!(slot.is_cold, "inspected slot must remain cold");
    }

    #[test]
    fn test_inspect_storage_rex4_slot_miss_inserts_and_returns_db_value() {
        const ADDR: Address = address!("00000000000000000000000000000000000000cc");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        let key = U256::from(7);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on absent slot");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "absent slot on non-created account must return ZERO from database"
        );
        assert!(slot.is_cold, "newly inserted slot must be marked cold");

        let calls_after_first = journal.database.storage_calls();
        let slot2 =
            journal.inspect_storage(spec, ADDR, key).expect("second inspect_storage must succeed");

        assert_eq!(slot2.present_value, U256::ZERO, "second call must return the same value");
        assert_eq!(
            journal.database.storage_calls(),
            calls_after_first,
            "second inspect_storage on the same slot must hit the cache, not the DB",
        );
    }

    #[test]
    fn test_inspect_storage_rex4_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000dd");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(1);
        let spec = MegaSpecId::REX4;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
    }

    #[test]
    fn test_inspect_storage_pre_rex4_uses_delegation_path() {
        const ADDR: Address = address!("00000000000000000000000000000000000000ee");
        const DELEGATE: Address = address!("00000000000000000000000000000000000000ed");
        let delegate_code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(ADDR, DELEGATE)
            .with_account_code(DELEGATE, delegate_code);
        let mut journal = Journal::new(db);

        let key = U256::from(5);
        let expected = U256::from(123);
        let spec = MegaSpecId::MINI_REX;

        // Preload the delegator so the subsequent occupied-branch inspection hydrates the
        // EIP-7702 designation and the pre-REX4 delegated walk can follow it.
        let _ = inspect_account(&mut journal, ADDR, false).unwrap();
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, DELEGATE, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed pre-REX4");

        assert_eq!(
            slot.present_value, expected,
            "pre-REX4 storage inspection must follow the delegator to the delegate's slot",
        );
        assert!(slot.is_cold, "slot must be marked cold");
        assert_eq!(
            journal.database.storage_calls(),
            0,
            "delegate slot pre-seeded in the cache must be returned without hitting the DB",
        );
    }

    #[test]
    fn test_inspect_storage_pre_rex4_newly_created_short_circuits_db() {
        const ADDR: Address = address!("00000000000000000000000000000000000000ef");
        let bytecode = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default().with_account_code(ADDR, bytecode);
        let mut journal = Journal::new(db);

        {
            let account = inspect_account(&mut journal, ADDR, false).unwrap();
            account.mark_created();
        }

        let key = U256::from(6);
        let spec = MegaSpecId::MINI_REX;

        let slot = journal
            .inspect_storage(spec, ADDR, key)
            .expect("inspect_storage must succeed pre-REX4 on newly-created account");

        assert_eq!(
            slot.present_value,
            U256::ZERO,
            "pre-REX4 newly-created account must return ZERO without querying database"
        );
        assert!(slot.is_cold, "slot must be marked cold");
        assert_eq!(
            journal.database.storage_calls(),
            0,
            "newly-created pre-REX4 path must not hit the database storage lookup",
        );
    }

    #[test]
    fn test_inspect_storage_rex4_ignores_eip7702_delegation() {
        const DELEGATOR: Address = address!("0000000000000000000000000000000000000d01");
        const DELEGATE: Address = address!("0000000000000000000000000000000000000d02");
        let delegate_code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x01, 0x01]);
        let db = LazyCodeDatabase::default()
            .with_eip7702_delegation(DELEGATOR, DELEGATE)
            .with_account_code(DELEGATE, delegate_code);
        let mut journal = Journal::new(db);

        let key = U256::from(2);
        let expected = U256::from(99);
        {
            let tid = journal.transaction_id;
            let account = inspect_account(&mut journal, DELEGATOR, false).unwrap();
            let mut slot = EvmStorageSlot::new(expected, tid);
            slot.mark_cold();
            account.storage.insert(key, slot);
        }

        let spec = MegaSpecId::REX4;
        let slot = journal
            .inspect_storage(spec, DELEGATOR, key)
            .expect("inspect_storage must succeed for REX4 delegator");

        assert_eq!(
            slot.present_value, expected,
            "REX4 must read storage from delegator (original address), not delegate"
        );
    }

    // === `sload_skip_cold_load`: oracle-contract customizations ===
    //
    // Every MegaETH spec is Berlin+, so the interpreter's SLOAD reaches
    // `Host::sload_skip_cold_load`, not `Host::sload`. These tests drive that entry directly.

    /// External environment type used by the oracle `SLOAD` tests.
    type OracleTestEnvs = crate::TestExternalEnvs<core::convert::Infallible>;

    /// Transaction sender for the oracle `SLOAD` tests — deliberately not the system address.
    const ORACLE_TX_CALLER: Address = address!("00000000000000000000000000000000000000c1");
    /// A non-oracle contract used as the control address.
    const PLAIN_CONTRACT: Address = address!("00000000000000000000000000000000000000c2");
    /// The storage slot every oracle `SLOAD` test reads.
    const ORACLE_TEST_SLOT: StorageKey = StorageKey::from_limbs([7, 0, 0, 0]);

    /// Builds a `MegaContext` at `spec` over `env` and `db`, with `ORACLE_TX_CALLER` as the
    /// transaction sender and both the oracle contract and `PLAIN_CONTRACT` resident in the
    /// journal.
    ///
    /// Residency matters: revm's `sload_skip_cold_load` assumes the account is already loaded (a
    /// real `SLOAD` always runs with its own contract as the executing frame) and reports
    /// `ColdLoadSkipped` otherwise. `inspect_account` seeds it while leaving every slot cold.
    fn oracle_sload_context(
        spec: MegaSpecId,
        env: OracleTestEnvs,
        db: crate::test_utils::MemoryDatabase,
    ) -> MegaContext<crate::test_utils::MemoryDatabase, OracleTestEnvs> {
        let mut ctx = MegaContext::<_, OracleTestEnvs>::new_with_ext_envs(
            db,
            spec,
            Rc::new(env.clone()),
            Rc::new(RefCell::new(env)),
        );
        ctx.inner.tx.base.caller = ORACLE_TX_CALLER;
        ctx.inspect_account(ORACLE_CONTRACT_ADDRESS, false)
            .expect("oracle account must load into the journal");
        ctx.inspect_account(PLAIN_CONTRACT, false)
            .expect("plain contract must load into the journal");
        ctx
    }

    /// Whether the journal holds a cached slot for `address` — used to show that a skipped cold
    /// load never reached the inner context.
    fn journal_has_slot(
        ctx: &MegaContext<crate::test_utils::MemoryDatabase, OracleTestEnvs>,
        address: Address,
        key: StorageKey,
    ) -> bool {
        ctx.inner
            .journaled_state
            .inner
            .state
            .get(&address)
            .is_some_and(|account| account.storage.contains_key(&key))
    }

    /// An oracle read served by the oracle environment wins over the value in state, and is
    /// reported cold.
    #[test]
    fn test_oracle_sload_serves_oracle_env_value_forced_cold() {
        let env_value = StorageValue::from(0xaaaa_u64);
        let state_value = StorageValue::from(0xbbbb_u64);
        let env = OracleTestEnvs::new().with_oracle_storage(ORACLE_TEST_SLOT, env_value);
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_TEST_SLOT,
            state_value,
        );
        let mut ctx = oracle_sload_context(MegaSpecId::MINI_REX, env, db);

        let load = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("oracle sload must succeed");

        assert_eq!(
            load.data, env_value,
            "the oracle environment must take precedence over the value in state",
        );
        assert!(load.is_cold, "an oracle read served by the oracle environment must be cold");
        assert!(
            !journal_has_slot(&ctx, ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT),
            "an oracle-env hit must not fall through to the journal",
        );
    }

    /// With no oracle-environment value the read falls back to the inner context — and stays cold
    /// even on the second read, when the journal considers the slot warm. This is the frozen rule:
    /// a replay cannot tell whether the payload builder served the slot from `oracle_env` or from
    /// state, so both are charged the cold price.
    #[test]
    fn test_oracle_sload_journal_fallback_is_forced_cold_even_when_warm() {
        let state_value = StorageValue::from(0xbbbb_u64);
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_TEST_SLOT,
            state_value,
        );
        // Oracle env configured, but with no value for this slot.
        let env = OracleTestEnvs::new()
            .with_oracle_storage(StorageKey::from(999), StorageValue::from(1_u64));
        let mut ctx = oracle_sload_context(MegaSpecId::MINI_REX, env, db);

        let first = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("first oracle sload must succeed");
        assert_eq!(first.data, state_value, "fallback must read the value out of state");
        assert!(first.is_cold, "first oracle read must be cold");

        let second = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("second oracle sload must succeed");
        assert_eq!(second.data, state_value, "second read must return the same value");
        assert!(
            second.is_cold,
            "an oracle read must stay cold even once the journal holds the slot warm — this is \
             what keeps the gas cost identical between building and replaying a block",
        );
    }

    /// A non-oracle address keeps plain revm semantics: state value, cold then warm, no oracle
    /// tracker mark — even at a spec where oracle detention is active and the oracle env has a
    /// value for the very same slot.
    #[test]
    fn test_sload_skip_cold_load_non_oracle_address_keeps_normal_cold_warm() {
        let state_value = StorageValue::from(0xcccc_u64);
        let env = OracleTestEnvs::new()
            .with_oracle_storage(ORACLE_TEST_SLOT, StorageValue::from(0xaaaa_u64));
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            PLAIN_CONTRACT,
            ORACLE_TEST_SLOT,
            state_value,
        );
        let mut ctx = oracle_sload_context(MegaSpecId::REX3, env, db);

        let first = ctx
            .sload_skip_cold_load(PLAIN_CONTRACT, ORACLE_TEST_SLOT, false)
            .expect("first plain sload must succeed");
        assert_eq!(
            first.data, state_value,
            "a non-oracle read must never be served by the oracle environment",
        );
        assert!(first.is_cold, "first read of a cold slot must be cold");

        let second = ctx
            .sload_skip_cold_load(PLAIN_CONTRACT, ORACLE_TEST_SLOT, false)
            .expect("second plain sload must succeed");
        assert!(!second.is_cold, "second read of a non-oracle slot must be warm");

        assert!(
            !ctx.volatile_data_tracker.borrow().has_accessed_oracle(),
            "a non-oracle read must not mark oracle access",
        );
        assert_eq!(
            ctx.volatile_data_tracker.borrow().get_compute_gas_limit(),
            None,
            "a non-oracle read must not engage gas detention",
        );
    }

    /// The oracle access mark (and with it the detention compute-gas cap) starts at REX3 exactly:
    /// REX2 reads the oracle without marking, REX3 marks and caps.
    #[test]
    fn test_oracle_sload_marks_oracle_access_from_rex3_exactly() {
        for (spec, should_mark) in [(MegaSpecId::REX2, false), (MegaSpecId::REX3, true)] {
            let env = OracleTestEnvs::new()
                .with_oracle_storage(ORACLE_TEST_SLOT, StorageValue::from(0xaaaa_u64));
            let mut ctx =
                oracle_sload_context(spec, env, crate::test_utils::MemoryDatabase::default());

            ctx.sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
                .expect("oracle sload must succeed");

            let tracker = ctx.volatile_data_tracker.borrow();
            assert_eq!(
                tracker.has_accessed_oracle(),
                should_mark,
                "{spec:?}: oracle access mark must be {should_mark}",
            );
            let expected_limit = should_mark.then(|| {
                crate::EvmTxRuntimeLimits::from_spec(spec).oracle_access_compute_gas_limit
            });
            assert_eq!(
                tracker.get_compute_gas_limit(),
                expected_limit,
                "{spec:?}: detention cap after an oracle SLOAD",
            );
        }
    }

    /// Transactions sent by the block's system address read the oracle without triggering
    /// detention; the same read from any other sender marks it.
    #[test]
    fn test_oracle_sload_exempts_system_address_caller() {
        for exempt in [true, false] {
            let env = OracleTestEnvs::new()
                .with_oracle_storage(ORACLE_TEST_SLOT, StorageValue::from(0xaaaa_u64));
            let mut ctx = oracle_sload_context(
                MegaSpecId::REX3,
                env,
                crate::test_utils::MemoryDatabase::default(),
            );
            if exempt {
                ctx.inner.tx.base.caller = ctx.system_address;
            }

            ctx.sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
                .expect("oracle sload must succeed");

            assert_eq!(
                ctx.volatile_data_tracker.borrow().has_accessed_oracle(),
                !exempt,
                "system-address exemption (exempt = {exempt}) must decide the oracle mark",
            );
        }
    }

    /// `skip_cold_load` means "I cannot pay a cold load's surcharge". Because an oracle read is
    /// always cold, the read is refused with `ColdLoadSkipped` (which the interpreter turns into
    /// `OutOfGas`, exactly where the cold charge would have landed) without consulting the oracle
    /// env or the journal. The oracle access is still marked: the pre-`skip_cold_load` SLOAD called
    /// the host before charging, so an unaffordable oracle read counted as an access.
    #[test]
    fn test_oracle_sload_skip_cold_load_is_refused_but_still_marks() {
        let env = OracleTestEnvs::new()
            .with_oracle_storage(ORACLE_TEST_SLOT, StorageValue::from(0xaaaa_u64));
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_TEST_SLOT,
            StorageValue::from(0xbbbb_u64),
        );
        let mut ctx = oracle_sload_context(MegaSpecId::REX3, env, db);

        let error = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, true)
            .expect_err("a forced-cold read must be refused when the caller cannot pay for it");
        assert_eq!(error, LoadError::ColdLoadSkipped);
        assert!(
            ctx.volatile_data_tracker.borrow().has_accessed_oracle(),
            "a refused oracle read must still mark the access",
        );
        assert!(
            !journal_has_slot(&ctx, ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT),
            "a refused oracle read must not warm or cache the slot",
        );

        // The very same read succeeds once the caller can afford the cold surcharge — the refusal
        // is about affordability, not about the slot.
        let load = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("the same read must succeed with skip_cold_load = false");
        assert!(load.is_cold);
    }

    /// A non-oracle address is unaffected by the forced-cold rule: `skip_cold_load` only refuses a
    /// slot that is genuinely cold, and a warm one is served normally.
    #[test]
    fn test_sload_skip_cold_load_non_oracle_warm_slot_ignores_skip_flag() {
        let state_value = StorageValue::from(0xcccc_u64);
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            PLAIN_CONTRACT,
            ORACLE_TEST_SLOT,
            state_value,
        );
        let mut ctx = oracle_sload_context(MegaSpecId::REX3, OracleTestEnvs::new(), db);

        assert_eq!(
            ctx.sload_skip_cold_load(PLAIN_CONTRACT, ORACLE_TEST_SLOT, true),
            Err(LoadError::ColdLoadSkipped),
            "a cold non-oracle slot must be refused under skip_cold_load",
        );

        // Warm it, then repeat with the flag set: warm reads need no surcharge.
        ctx.sload_skip_cold_load(PLAIN_CONTRACT, ORACLE_TEST_SLOT, false)
            .expect("warming read must succeed");
        let warm = ctx
            .sload_skip_cold_load(PLAIN_CONTRACT, ORACLE_TEST_SLOT, true)
            .expect("a warm non-oracle slot must be served even under skip_cold_load");
        assert_eq!(warm.data, state_value);
        assert!(!warm.is_cold);
    }

    /// Before `MINI_REX` the oracle address is an ordinary contract: no external value source, no
    /// forced-cold, no mark.
    #[test]
    fn test_sload_skip_cold_load_pre_mini_rex_treats_oracle_as_plain_storage() {
        let state_value = StorageValue::from(0xbbbb_u64);
        let env = OracleTestEnvs::new()
            .with_oracle_storage(ORACLE_TEST_SLOT, StorageValue::from(0xaaaa_u64));
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_TEST_SLOT,
            state_value,
        );
        let mut ctx = oracle_sload_context(MegaSpecId::EQUIVALENCE, env, db);

        let first = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("first sload must succeed");
        assert_eq!(
            first.data, state_value,
            "pre-MINI_REX must ignore the oracle environment and read state",
        );
        assert!(first.is_cold);

        let second = ctx
            .sload_skip_cold_load(ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT, false)
            .expect("second sload must succeed");
        assert!(!second.is_cold, "pre-MINI_REX must warm the oracle slot like any other");
        assert!(!ctx.volatile_data_tracker.borrow().has_accessed_oracle());
    }

    /// The legacy `Host::sload` entry is not overridden any more; it reaches the oracle path
    /// through its default implementation. Pin that so the two entries cannot drift apart.
    #[test]
    fn test_legacy_host_sload_shares_the_oracle_path() {
        let env_value = StorageValue::from(0xaaaa_u64);
        let env = OracleTestEnvs::new().with_oracle_storage(ORACLE_TEST_SLOT, env_value);
        let db = crate::test_utils::MemoryDatabase::default().account_storage(
            ORACLE_CONTRACT_ADDRESS,
            ORACLE_TEST_SLOT,
            StorageValue::from(0xbbbb_u64),
        );
        let mut ctx = oracle_sload_context(MegaSpecId::REX3, env, db);

        let load = Host::sload(&mut ctx, ORACLE_CONTRACT_ADDRESS, ORACLE_TEST_SLOT)
            .expect("legacy Host::sload must succeed");

        assert_eq!(load.data, env_value, "legacy sload must also read the oracle environment");
        assert!(load.is_cold, "legacy sload must also force the access cold");
        assert!(
            ctx.volatile_data_tracker.borrow().has_accessed_oracle(),
            "legacy sload must also mark oracle access",
        );
    }

    /// Direct unit coverage for `Host::load_account_delegated`'s REX6 delegate mark.
    ///
    /// Production CALL-family opcodes no longer reach this trait method (revm resolves
    /// the delegate via `load_account_info_skip_cold_load` under a
    /// `begin_call_target_resolution` bracket). The override therefore only serves
    /// direct trait callers. Both polarities of the REX6 gate at this site survived
    /// the M2 campaign because no test observed the mark set through this entry.
    ///
    /// Setup: EIP-7702 delegator whose code points at the block beneficiary.
    /// - REX6: loading the delegator must also mark beneficiary access.
    /// - REX5: loading the delegator must **not** mark beneficiary access (only the raw address is
    ///   marked, and the raw address is not the beneficiary).
    #[test]
    fn test_load_account_delegated_marks_eip7702_delegate_only_from_rex6() {
        const DELEGATOR: Address = address!("00000000000000000000000000000000000000d1");
        const BENEFICIARY: Address = address!("00000000000000000000000000000000000000b1");
        const UNRELATED: Address = address!("00000000000000000000000000000000000000aa");

        // REX6: delegate hop must mark beneficiary.
        {
            let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, BENEFICIARY);
            let block = revm::context::BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
            let mut ctx = MegaContext::new(db, MegaSpecId::REX6).with_block(block);

            let _loaded = Host::load_account_delegated(&mut ctx, DELEGATOR)
                .expect("load_account_delegated must succeed");

            assert!(
                ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance(),
                "REX6 load_account_delegated(delegator→beneficiary) must mark beneficiary access",
            );
        }

        // REX5: freeze — only the raw address is marked; delegator ≠ beneficiary so no mark.
        {
            let db = LazyCodeDatabase::default().with_eip7702_delegation(DELEGATOR, BENEFICIARY);
            let block = revm::context::BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
            let mut ctx = MegaContext::new(db, MegaSpecId::REX5).with_block(block);

            let _ = Host::load_account_delegated(&mut ctx, DELEGATOR)
                .expect("load_account_delegated must succeed");

            assert!(
                !ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance(),
                "REX5 load_account_delegated must not resolve the EIP-7702 delegate for marking",
            );
        }

        // Control: loading an unrelated non-delegating account never marks.
        {
            let db = LazyCodeDatabase::default();
            let block = revm::context::BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
            let mut ctx = MegaContext::new(db, MegaSpecId::REX6).with_block(block);

            let _ = Host::load_account_delegated(&mut ctx, UNRELATED)
                .expect("load_account_delegated must succeed");

            assert!(
                !ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance(),
                "loading an unrelated address must not mark beneficiary access",
            );
        }
    }
}
