use core::cell::RefCell;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::rc::Rc;

use crate::{
    AdditionalLimit, ExternalEnvTypes, MegaContext, MegaSpecId, OracleEnv, SaltEnv,
    VolatileDataAccess, VolatileDataAccessTracker, ORACLE_CONTRACT_ADDRESS,
};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context_interface::journaled_state::AccountLoad,
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
};

impl<DB: Database, ExtEnvs: ExternalEnvTypes> Host for MegaContext<DB, ExtEnvs> {
    // Block environment related methods - with tracking
    fn basefee(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::BASE_FEE);
        self.inner.basefee()
    }

    fn gas_limit(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::GAS_LIMIT);
        self.inner.gas_limit()
    }

    fn difficulty(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::DIFFICULTY);
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccess::PREV_RANDAO);
        self.inner.prevrandao()
    }

    fn block_number(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::BLOCK_NUMBER);
        self.inner.block_number()
    }

    fn timestamp(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::TIMESTAMP);
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.mark_block_env_accessed(VolatileDataAccess::COINBASE);
        self.inner.beneficiary()
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        self.mark_block_env_accessed(VolatileDataAccess::BLOCK_HASH);
        self.inner.block_hash(number)
    }

    // Blob-related block environment methods - with tracking
    fn blob_gasprice(&self) -> U256 {
        self.mark_block_env_accessed(VolatileDataAccess::BLOB_BASE_FEE);
        self.inner.blob_gasprice()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        self.mark_block_env_accessed(VolatileDataAccess::BLOB_HASH);
        self.inner.blob_hash(number)
    }

    delegate! {
        to self.inner {
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn log(&mut self, log: Log);
            fn caller(&self) -> Address;
            fn max_initcode_size(&self) -> usize;
            fn selfdestruct(&mut self, address: Address, target: Address) -> Option<StateLoad<SelfDestructResult>>;
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

    fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>> {
        if self.spec.is_enabled(MegaSpecId::MINI_REX) && address == ORACLE_CONTRACT_ADDRESS {
            // Rex3+: Mark oracle access for gas detention on SLOAD rather than CALL.
            // The actual gas limit enforcement happens in the SLOAD instruction wrapper
            // (detain_gas_ext::sload in instructions.rs).
            if self.spec.is_enabled(MegaSpecId::REX3) {
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
pub trait HostExt: Host {
    /// The error type for the oracle.
    type Error;

    /// Gets the `MegaSpecId` of the current execution context.
    fn spec_id(&self) -> MegaSpecId;

    /// Gets the `AdditionalLimit` instance. Only used when the `MINI_REX` spec is enabled.
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>>;

    /// Gets the gas cost for setting a storage slot to a non-zero value. Only used when the
    /// `MINI_REX` spec is enabled.
    fn sstore_set_storage_gas(&self, address: Address, key: U256) -> Result<u64, Self::Error>;

    /// Gets the gas cost for creating a new account. Only used when the `MINI_REX` spec is enabled.
    fn new_account_storage_gas(&self, address: Address) -> Result<u64, Self::Error>;

    /// Gets the gas cost for creating a new contract. Only used when the `REX` spec is
    /// enabled.
    fn create_contract_storage_gas(&self, address: Address) -> Result<u64, Self::Error>;

    /// Gets the volatile data tracker. Only used when the `MINI_REX` spec is enabled.
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>>;
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> HostExt for MegaContext<DB, ExtEnvs> {
    type Error = <ExtEnvs::SaltEnv as SaltEnv>::Error;

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
    fn sstore_set_storage_gas(&self, address: Address, key: U256) -> Result<u64, Self::Error> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        self.dynamic_storage_gas_cost.borrow_mut().sstore_set_gas(address, key)
    }

    #[inline]
    fn new_account_storage_gas(&self, address: Address) -> Result<u64, Self::Error> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        self.dynamic_storage_gas_cost.borrow_mut().new_account_gas(address)
    }

    #[inline]
    fn create_contract_storage_gas(&self, address: Address) -> Result<u64, Self::Error> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        self.dynamic_storage_gas_cost.borrow_mut().create_contract_gas(address)
    }

    #[inline]
    fn volatile_data_tracker(&self) -> &Rc<RefCell<VolatileDataAccessTracker>> {
        &self.volatile_data_tracker
    }
}
