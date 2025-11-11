//! This module provides utility functions to generate EVM bytecode.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use alloy_primitives::{Address, Bytes, U256};
use revm::bytecode::opcode::{
    DUP1, EQ, INVALID, JUMPDEST, JUMPI, MSTORE, PUSH0, RETURN, REVERT, SSTORE, STOP,
};

use crate::test_utils::right_pad_bytes;

/// A builder for assembling EVM bytecode.
#[derive(Debug, Default)]
pub struct BytecodeBuilder {
    code: Vec<u8>,
}

impl BytecodeBuilder {
    /// Build the bytecode.
    pub fn build(self) -> Bytes {
        self.code.into()
    }

    /// Build the bytecode as a vector.
    pub fn build_vec(self) -> Vec<u8> {
        self.code
    }

    /// Get the length of the bytecode.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Check if the bytecode is empty.
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Append a single opcode or byte.
    pub fn append(mut self, opcode: u8) -> Self {
        self.code.push(opcode);
        self
    }

    /// Append a series of opcodes or bytes.
    pub fn append_many(mut self, items: impl IntoIterator<Item = u8>) -> Self {
        self.code.extend(items);
        self
    }

    /// Append a PUSH opcode and the bytes to push.
    pub fn push_bytes(mut self, bytes: impl AsRef<[u8]>) -> Self {
        let bytes: &[u8] = bytes.as_ref();
        assert!(bytes.len() <= 32);
        self.code.push(PUSH0 + bytes.len() as u8);
        self.code.extend(bytes.to_vec());
        self
    }

    /// Append a PUSH opcode and the number to push.
    pub fn push_number<T: Into<u128> + Copy>(self, number: T) -> Self {
        let num = number.into();
        let bytes = match core::mem::size_of::<T>() {
            1 => (num as u8).to_be_bytes().to_vec(),
            2 => (num as u16).to_be_bytes().to_vec(),
            4 => (num as u32).to_be_bytes().to_vec(),
            8 => (num as u64).to_be_bytes().to_vec(),
            16 => num.to_be_bytes().to_vec(),
            _ => panic!("Unsupported integer size"),
        };
        self.push_bytes(bytes)
    }

    /// Append a PUSH opcode and the address to push.
    pub fn push_address(self, address: Address) -> Self {
        self.push_bytes(address)
    }

    /// Append a PUSH opcode and the u256 value to push.
    pub fn push_u256(self, value: U256) -> Self {
        self.push_bytes(value.to_be_bytes_vec())
    }

    /// Append a series of MSTORE opcodes to store the given bytes at the given offset.
    pub fn mstore(self, offset: usize, bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref().to_vec();
        let padded_bytes = right_pad_bytes(bytes, 32);
        let mut this = self;
        for (i, chunk) in padded_bytes.chunks(32).enumerate() {
            this = this.push_bytes(chunk);
            this = this.push_number((offset + i * 32) as u64);
            this.code.push(MSTORE);
        }
        this
    }

    /// Append a SSTORE opcode to store the given value at the given slot.
    pub fn sstore(mut self, slot: U256, value: U256) -> Self {
        self = self.push_u256(value);
        self = self.push_u256(slot);
        self.code.push(SSTORE);
        self
    }

    /// Append a REVERT opcode with empty return data.
    pub fn revert(self) -> Self {
        self.append_many([PUSH0, PUSH0, REVERT])
    }

    /// Append a REVERT opcode with the given return data.
    pub fn revert_with_data(mut self, data: impl AsRef<[u8]>) -> Self {
        let data_len = data.as_ref().len();
        self = self.mstore(0x0, data);
        self = self.push_number(data_len as u64);
        self = self.push_number(0x0_u64);
        self = self.append(REVERT);
        self
    }

    /// Append a RETURN opcode with empty return data.
    pub fn return_empty(self) -> Self {
        self.append_many([PUSH0, PUSH0, RETURN])
    }

    /// Append a RETURN opcode with the given return data.
    pub fn return_with_data(mut self, data: impl AsRef<[u8]>) -> Self {
        let data_len = data.as_ref().len();
        self = self.mstore(0x0, data);
        self = self.push_number(data_len as u64);
        self = self.push_number(0x0_u64);
        self = self.append(RETURN);
        self
    }

    /// Append an assmembly snippet that checks whether the value at the given stack position is
    /// equal to the given value.
    ///
    /// If not, call INVALID opcode.
    ///
    /// This snippet will left the stack unchanged after execution.
    pub fn assert_stack_value(mut self, stack_position: usize, value: U256) -> Self {
        self = self
            .append(DUP1 + stack_position as u8) // duplicate the value at the given stack position to check
            .push_u256(value)
            .append(EQ);
        let code_len = self.len();
        self =
            self.push_number(code_len as u64 + 1 + 8 + 2).append_many([JUMPI, INVALID, JUMPDEST]);
        self
    }

    /// Append a STOP opcode.
    pub fn stop(mut self) -> Self {
        self = self.append(STOP);
        self
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use alloy_primitives::address;
    use revm::context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
    };

    use crate::{
        test_utils::MemoryDatabase, DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason,
        MegaSpecId, MegaTransaction, MegaTransactionError,
    };

    use super::*;

    fn execute_bytecode(
        bytecode: Bytes,
    ) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
        let contract = address!("0000000000000000000000000000000000100001");
        let mut db = MemoryDatabase::default();
        db.set_account_code(contract, bytecode);
        let mut context =
            MegaContext::new(&mut db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default());
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });
        let mut evm = MegaEvm::new_with_accelerated_precompiles(context, None);
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
