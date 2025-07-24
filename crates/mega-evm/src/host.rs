use crate::{Context, SpecId};
use alloy_evm::Database;
use alloy_primitives::{Address, Bytes, Log, B256, U256};
use delegate::delegate;
use revm::{
    context::{Cfg, ContextTr},
    context_interface::journaled_state::AccountLoad,
    interpreter::{Host, SStoreResult, SelfDestructResult, StateLoad},
};

impl<DB: Database> Host for Context<DB> {
    delegate! {
        to self.inner {
            fn basefee(&self) -> U256;
            fn blob_gasprice(&self) -> U256;
            fn gas_limit(&self) -> U256;
            fn difficulty(&self) -> U256;
            fn prevrandao(&self) -> Option<U256>;
            fn block_number(&self) -> u64;
            fn timestamp(&self) -> U256;
            fn beneficiary(&self) -> Address;
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn caller(&self) -> Address;
            fn blob_hash(&self, number: usize) -> Option<U256>;
            fn block_hash(&mut self, number: u64) -> Option<B256>;
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

    fn max_initcode_size(&self) -> usize {
        if self.megaeth_spec().is_enabled_in(SpecId::MINI_REX) {
            // (contract size limit) + 24KB
            self.cfg().max_code_size() + 24 * 1024
        } else {
            self.inner.max_initcode_size()
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
