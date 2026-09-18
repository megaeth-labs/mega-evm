//! Unit tests extracted from `crates/mega-evm/src/test_utils/opcode_gen.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/test_utils/opcode_gen.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use alloy_primitives::address;
    use revm::context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
    };

    use crate::{
        test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
        MegaTransaction, MegaTransactionError,
    };

    use super::*;

    fn execute_bytecode(
        bytecode: Bytes,
    ) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
        let contract = address!("0000000000000000000000000000000000100001");
        let mut db = MemoryDatabase::default();
        db.set_account_code(contract, bytecode);
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });
        let mut evm = MegaEvm::new(context);
        let tx = TxEnvBuilder::default().call(contract).gas_limit(1_000_000_000).build_fill();
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());
        alloy_evm::Evm::transact_raw(&mut evm, tx)
    }

    #[test]
    fn test_assert_stack_value_success() {
        let mut builder = BytecodeBuilder::default().push_number(0x2333u64);
        builder = builder.assert_stack_value(0, U256::from(0x2333u64));
        let bytecode = builder.build();
        let result = execute_bytecode(bytecode);
        assert!(result.unwrap().result.is_success(), "Transaction should succeed");
    }

    #[test]
    fn test_assert_stack_value_failure() {
        let mut builder = BytecodeBuilder::default().push_number(0x2333u64);
        builder = builder.assert_stack_value(0, U256::from(0x9999u64));
        let bytecode = builder.build();
        let result = execute_bytecode(bytecode);
        assert!(result.unwrap().result.is_halt(), "Transaction should fail");
    }
}
