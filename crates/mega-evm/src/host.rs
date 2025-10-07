use core::cell::RefCell;
use std::rc::Rc;

use crate::{AdditionalLimit, BlockEnvAccess, ExternalEnvs, MegaContext, MegaSpecId, SaltEnv};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context_interface::journaled_state::AccountLoad,
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
};

impl<DB: Database, ExtEnvs: ExternalEnvs> Host for MegaContext<DB, ExtEnvs> {
    // Block environment related methods - with tracking
    fn basefee(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::BASE_FEE);
        self.inner.basefee()
    }

    fn gas_limit(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::GAS_LIMIT);
        self.inner.gas_limit()
    }

    fn difficulty(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::DIFFICULTY);
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.mark_block_env_accessed(BlockEnvAccess::PREV_RANDAO);
        self.inner.prevrandao()
    }

    fn block_number(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::BLOCK_NUMBER);
        self.inner.block_number()
    }

    fn timestamp(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::TIMESTAMP);
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.mark_block_env_accessed(BlockEnvAccess::COINBASE);
        self.inner.beneficiary()
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        self.mark_block_env_accessed(BlockEnvAccess::BLOCK_HASH);
        self.inner.block_hash(number)
    }

    // Blob-related block environment methods - with tracking
    fn blob_gasprice(&self) -> U256 {
        self.mark_block_env_accessed(BlockEnvAccess::BLOB_BASE_FEE);
        self.inner.blob_gasprice()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        self.mark_block_env_accessed(BlockEnvAccess::BLOB_HASH);
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
            fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>>;
            fn tstore(&mut self, address: Address, key: U256, value: U256);
            fn tload(&mut self, address: Address, key: U256) -> U256;
        }
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

    /// Gets the `AdditionalLimit` instance. Only used when the `MINI_REX` spec is enabled.
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>>;

    /// Gets the gas cost for setting a storage slot to a non-zero value. Only used when the
    /// `MINI_REX` spec is enabled.
    fn sstore_set_gas(&self, address: Address, key: U256) -> Result<u64, Self::Error>;

    /// Gets the gas cost for creating a new account. Only used when the `MINI_REX` spec is enabled.
    fn new_account_gas(&self, address: Address) -> Result<u64, Self::Error>;
}

impl<DB: Database, ExtEnvs: ExternalEnvs> HostExt for MegaContext<DB, ExtEnvs> {
    type Error = <ExtEnvs::SaltEnv as SaltEnv>::Error;

    #[inline]
    fn additional_limit(&self) -> &Rc<RefCell<AdditionalLimit>> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        &self.additional_limit
    }

    #[inline]
    fn sstore_set_gas(&self, address: Address, key: U256) -> Result<u64, Self::Error> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        self.dynamic_gas_cost.borrow_mut().sstore_set_gas(address, key)
    }

    #[inline]
    fn new_account_gas(&self, address: Address) -> Result<u64, Self::Error> {
        debug_assert!(self.spec.is_enabled(MegaSpecId::MINI_REX));
        self.dynamic_gas_cost.borrow_mut().new_account_gas(address)
    }
}
