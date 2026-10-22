//! Unit tests extracted from `crates/mega-evm/src/evm/host.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/host.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

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
    /// `AccountInfo { code: None, code_hash: <real hash> }` for accounts with
    /// on-chain bytecode, and the bytecode itself is lazy-loaded on demand via
    /// `code_by_hash()`. The workspace's `MemoryDatabase` cannot model this —
    /// it eagerly populates `AccountInfo.code` inside `basic()`, so any cache
    /// miss against it would always see the code already hydrated.
    #[derive(Default)]
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
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
            );
            self.codes.insert(code_hash, code);
            self
        }

        fn with_eip7702_delegation(mut self, address: Address, delegate: Address) -> Self {
            let code = Bytecode::new_eip7702(delegate);
            let code_hash = code.hash_slow();
            self.accounts.insert(
                address,
                AccountInfo { balance: U256::ZERO, nonce: 0, code_hash, code: None },
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
            !matches!(hydrated, Bytecode::Eip7702(_)),
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
}
