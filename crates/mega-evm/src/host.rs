use crate::{BlockEnvAccess, Context, SpecId};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context::{Cfg, ContextTr},
    context_interface::journaled_state::AccountLoad,
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
};

impl<DB: Database> Host for Context<DB> {
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

    // Non-block environment methods - no tracking needed
    delegate! {
        to self.inner {
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn caller(&self) -> Address;
            fn max_initcode_size(&self) -> usize;
            fn selfdestruct(&mut self, address: Address, target: Address) -> Option<StateLoad<SelfDestructResult>>;
            fn sstore(&mut self, address: Address, key: U256, value: U256) -> Option<StateLoad<SStoreResult>>;
            fn sload(&mut self, address: Address, key: U256) -> Option<StateLoad<U256>>;
            fn tstore(&mut self, address: Address, key: U256, value: U256);
            fn tload(&mut self, address: Address, key: U256) -> U256;
            fn balance(&mut self, address: Address) -> Option<StateLoad<U256>>;
            fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>>;
            fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>>;
            fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>>;
        }
    }

    fn log(&mut self, log: Log) {
        self.log_data_size += log.data.data.len() as u64;
        self.inner.log(log);
    }
}

/// Extension trait for the `Host` trait that provides additional functionality for `MegaETH`.
pub trait HostExt: Host {
    /// Get the total size of all previous log data, excluding current opcode.
    fn log_data_size(&self) -> u64;
}

impl<DB: Database> HostExt for Context<DB> {
    fn log_data_size(&self) -> u64 {
        self.log_data_size
    }
}
