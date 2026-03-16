use core::cell::RefCell;

#[cfg(not(feature = "std"))]
use alloc as std;
use mega_system_contracts::access_control::IMegaAccessControl::VolatileDataAccessType;
use std::{format, rc::Rc};

use crate::{
    AdditionalLimit, ExternalEnvTypes, MegaContext, MegaSpecId, OracleEnv,
    VolatileDataAccessTracker, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context::{ContextTr, JournalTr},
    context_interface::{context::ContextError, journaled_state::AccountLoad},
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
    primitives::{hash_map::Entry, StorageKey, KECCAK_EMPTY},
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
    ) -> Option<StateLoad<SelfDestructResult>> {
        // Rex4+: Mark beneficiary balance access when SELFDESTRUCT targets the beneficiary.
        // This enables gas detention and the disableVolatileDataAccess check in the instruction
        // wrapper.
        if self.spec.is_enabled(MegaSpecId::REX4) {
            self.check_and_mark_beneficiary_balance_access(&target);
        }
        self.inner.selfdestruct(address, target)
    }

    fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>> {
        if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
            // Rex3+: Mark oracle access for gas detention on SLOAD rather than CALL.
            // The actual gas limit enforcement happens in the SLOAD instruction wrapper
            // (detain_gas_ext::sload in instructions.rs).
            // Mega system address transactions are exempted from oracle gas detention.
            // Note: This checks the transaction sender (from TxEnv) via Host::caller(),
            // unlike the pre-Rex3 CALL-based path which checked the frame-level caller.
            if self.spec.is_enabled(MegaSpecId::REX3) && self.caller() != MEGA_SYSTEM_ADDRESS {
                self.volatile_data_tracker.borrow_mut().check_and_mark_oracle_access(&address);
            }

            // if the oracle env provides a value, return it. Otherwise, fallback to the inner
            // context.
            if let Some(value) = self.oracle_env.borrow().get_oracle_storage(key) {
                // Accessing oracle contract storage is forced to be cold access, since it always
                // reads from the outside world (oracle_env).
                return Some(StateLoad::new(value, true));
            }
        }
        let state_load = self.inner.sload(address, key);
        state_load.map(|mut state_load| {
            if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
                // It is indistinguishable to tell whether a storage access of oracle contract is
                // warm or not even if it is loaded from the inner journal state. This is because
                // the current execution may be a replay of existing blocks and we cannot know
                // whether the payload builder read from the oracle_env or not. So we force such
                // sload always to be cold access to ensure consistent gas cost.
                state_load.is_cold = true;
            }
            state_load
        })
    }

    fn balance(&mut self, address: Address) -> Option<StateLoad<U256>> {
        self.check_and_mark_beneficiary_balance_access(&address);
        self.inner.balance(address)
    }

    fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>> {
        self.check_and_mark_beneficiary_balance_access(&address);
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
}

/// Trait to inspect the journal's internal state without marking any accounts or storage slots as
/// warm.
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

    /// Inspect the account at the given address without marking it as warm.
    /// If the account is EIP-7702 type, it should inspect the delegated account.
    fn inspect_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<&mut Account, Self::DBError>;

    /// Inspect the storage at the given address and key without marking it as warm.
    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError>;
}

/// Load an account into the journal state without following EIP-7702 delegation.
/// Deliberately marks the account as cold since this is an inspection, not a warming access.
fn inspect_account<DB: revm::Database>(
    journal: &mut Journal<DB>,
    address: Address,
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
            // delibrately mark the account as cold since we are only inpecting it, not warming
            // it.
            account.mark_cold();
            Ok(entry.insert(account))
        }
    }
}

impl<DB: revm::Database> JournalInspectTr for Journal<DB> {
    type DBError = <DB as revm::Database>::Error;

    fn inspect_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<&mut Account, Self::DBError> {
        let account = inspect_account(self, address)?;

        // If EIP-7702 delegated, resolve exactly one hop (matching upstream revm behavior).
        let delegated_address = account.info.code.as_ref().and_then(|code| match code {
            Bytecode::Eip7702(code) => Some(code.address()),
            _ => None,
        });
        if let Some(delegated_address) = delegated_address {
            return inspect_account(self, delegated_address);
        }

        // Load account again to bypass the borrow checker.
        let account = self.inner.state.get_mut(&address).unwrap();
        Ok(account)
    }

    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, Self::DBError> {
        let transaction_id = self.transaction_id;
        let account = self.inspect_account_delegated(address)?;
        if account.storage.contains_key(&key) {
            // Slot already exists, return reference to it
            // Need to reload account to satisfy borrow checker
            let account = self.inspect_account_delegated(address)?;
            return Ok(account.storage.get(&key).unwrap());
        }
        // Slot doesn't exist, load from DB and insert
        let slot = self.database.storage(address, key)?;
        let mut slot = EvmStorageSlot::new(slot, transaction_id);
        // delibrately mark the slot as cold since we are only inpecting it, not warming it
        slot.mark_cold();
        // Load account again to bypass the borrow checker and insert the slot
        let account = self.inspect_account_delegated(address)?;
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

    fn inspect_account_delegated(&mut self, address: Address) -> Result<&mut Account, ()> {
        // Split borrow: `journaled_state` and `error` are sibling fields on the inner context,
        // so we can borrow them independently to avoid the double-call workaround.
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_account_delegated(address).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }

    fn inspect_storage(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<&EvmStorageSlot, ()> {
        let journal = &mut self.inner.journaled_state;
        let error = &mut self.inner.error;
        journal.inspect_storage(address, key).map_err(|e| {
            *error = Err(ContextError::Custom(format!("{e}")));
        })
    }
}
