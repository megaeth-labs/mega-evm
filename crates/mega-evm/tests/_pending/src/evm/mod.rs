//! Unit tests extracted from `crates/mega-evm/src/evm/mod.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/mod.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use alloy_primitives::{address, Bytes, U256};
    use revm::{
        context::{
            result::{ExecResultAndState, ExecutionResult},
            ContextSetters, TxEnv,
        },
        database::State,
        inspector::NoOpInspector,
        state::EvmState,
        ExecuteCommitEvm, ExecuteEvm, InspectEvm, SystemCallEvm,
    };

    const CALLER: Address = address!("4000000000000000000000000000000000000001");
    const CALLEE: Address = address!("5000000000000000000000000000000000000001");

    fn configure_context<DB: Database>(db: DB) -> MegaContext<DB, EmptyExternalEnv> {
        let mut context = MegaContext::new(db, MegaSpecId::REX4);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context
    }

    fn tx_env() -> TxEnv {
        TxEnv {
            caller: CALLER,
            gas_limit: 100_000,
            kind: alloy_primitives::TxKind::Call(CALLEE),
            value: U256::ZERO,
            data: Bytes::new(),
            ..Default::default()
        }
    }

    fn mega_tx() -> MegaTransaction {
        let mut tx = MegaTransaction::new(tx_env());
        tx.enveloped_tx = Some(Bytes::new());
        tx
    }

    #[test]
    fn test_revm_system_call_with_caller_works() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));
        let system_call: ExecutionResult<MegaHaltReason> =
            SystemCallEvm::transact_system_call_with_caller(&mut evm, CALLER, CALLEE, Bytes::new())
                .unwrap();
        assert!(system_call.is_success());
    }

    #[test]
    fn test_transact_system_call_with_gas_limit_uses_passed_value() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));

        let result = evm
            .transact_system_call_with_gas_limit(CALLER, CALLEE, Bytes::new(), 123_456_789)
            .unwrap();
        assert!(result.result.is_success());
        // The custom gas limit must be applied to the underlying tx.
        assert_eq!(evm.inner.ctx.tx.base.gas_limit, 123_456_789);
    }

    #[test]
    fn test_default_system_call_keeps_upstream_30m_gas_limit() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context.block.gas_limit = 100_000_000;
        let mut evm = MegaEvm::new(context);

        // The default system-call entry point must NOT be widened by REX5 — only the
        // explicit `transact_system_call_with_gas_limit` path should pick up the live
        // block budget. This preserves byte-level behavior of EIP-2935 / EIP-4788
        // pre-block calls across all specs.
        SystemCallEvm::transact_system_call_with_caller(&mut evm, CALLER, CALLEE, Bytes::new())
            .unwrap();
        // Literal, not `SYSTEM_CALL_GAS_LIMIT_FLOOR`: this assertion verifies revm's
        // upstream hardcoded default. If upstream ever drifts from our floor, this
        // test should fail loudly rather than be auto-aligned by our constant.
        assert_eq!(evm.inner.ctx.tx.base.gas_limit, 30_000_000);
    }

    #[test]
    fn test_mega_evm_exposes_state_wrapper_block_hashes() {
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(7, B256::from([7_u8; 32]));

        let evm = MegaEvm::new(configure_context(&mut state));
        assert_eq!(evm.get_accessed_block_hashes().get(&7), Some(&B256::from([7_u8; 32])));
    }

    #[test]
    fn test_convenience_execution_methods_work() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db)).with_inspector(NoOpInspector);

        let executed = evm.execute_transaction(mega_tx()).unwrap();
        assert!(executed.result.is_success());

        #[allow(deprecated)]
        let inspected = evm.inspect_transaction(mega_tx()).unwrap();
        assert!(inspected.result.is_success());
    }

}
