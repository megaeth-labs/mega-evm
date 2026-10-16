//! This module provides utility functions to generate EVM bytecode.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, Bytes, U256};
use revm::bytecode::opcode::{
    CALL, CALLCODE, CREATE, CREATE2, DUP1, EQ, GAS, INVALID, JUMPDEST, JUMPI, LOG3, MSTORE, PUSH0,
    RETURN, RETURNDATACOPY, RETURNDATASIZE, REVERT, SELFDESTRUCT, SSTORE, STOP,
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

    /// Append a `CALL` to `target` carrying `value` wei with all the gas left, no calldata and no
    /// return area, leaving the success flag on the stack.
    pub fn call(self, target: Address, value: U256) -> Self {
        self.call_by(CALL, target, value)
    }

    /// Append a `CALLCODE` of `target`'s code carrying `value` wei with all the gas left, no
    /// calldata and no return area, leaving the success flag on the stack.
    pub fn callcode(self, target: Address, value: U256) -> Self {
        self.call_by(CALLCODE, target, value)
    }

    fn call_by(self, opcode: u8, target: Address, value: U256) -> Self {
        self.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_u256(value)
            .push_address(target)
            .append(GAS)
            .append(opcode)
    }

    /// Append a `CREATE` of `init_code`, stored at memory offset 0, endowing the account it
    /// creates with `value` wei, and leaving its address — zero when the creation failed.
    pub fn create(self, value: U256, init_code: impl AsRef<[u8]>) -> Self {
        let len = init_code.as_ref().len() as u64;
        self.mstore(0, init_code)
            .push_number(len)
            .push_number(0_u64)
            .push_u256(value)
            .append(CREATE)
    }

    /// Append a `CREATE2` of `init_code` with `salt`, as [`create`](Self::create) does.
    pub fn create2(self, value: U256, init_code: impl AsRef<[u8]>, salt: U256) -> Self {
        let len = init_code.as_ref().len() as u64;
        self.mstore(0, init_code)
            .push_u256(salt)
            .push_number(len)
            .push_number(0_u64)
            .push_u256(value)
            .append(CREATE2)
    }

    /// Append a `SELFDESTRUCT` to `beneficiary`.
    pub fn selfdestruct(self, beneficiary: Address) -> Self {
        self.push_address(beneficiary).append(SELFDESTRUCT)
    }

    /// Append a `LOG3` of one word of memory: the shape of an EIP-7708 transfer log, and what the
    /// data size counts one at.
    pub fn log3_word(self) -> Self {
        self.append_many([PUSH0, PUSH0, PUSH0]).push_number(32_u8).append_many([PUSH0, LOG3])
    }

    /// Append a copy of the last call's return data to memory offset 0 and a `RETURN` of it.
    pub fn return_returndata(self) -> Self {
        self.copy_returndata().append_many([RETURNDATASIZE, PUSH0, RETURN])
    }

    /// Append a copy of the last call's return data to memory offset 0 and a `REVERT` with it.
    pub fn revert_with_returndata(self) -> Self {
        self.copy_returndata().append_many([RETURNDATASIZE, PUSH0, REVERT])
    }

    fn copy_returndata(self) -> Self {
        self.append_many([RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY])
    }

    /// Append a `RETURN` of the word on top of the stack, stored at memory offset 0.
    pub fn return_top(self) -> Self {
        self.append_many([PUSH0, MSTORE]).push_number(32_u8).append_many([PUSH0, RETURN])
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use alloy_primitives::{address, B256};
    use revm::{
        bytecode::opcode::POP,
        context::result::{EVMError, ResultAndState},
    };

    use crate::{
        test_utils::{transact, MemoryDatabase},
        MegaHaltReason, MegaSpecId, MegaTransactionError,
    };

    use super::*;

    fn execute_bytecode(
        bytecode: Bytes,
    ) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
        let caller = address!("0000000000000000000000000000000000100000");
        let contract = address!("0000000000000000000000000000000000100001");
        let mut db = MemoryDatabase::default();
        db.set_account_code(contract, bytecode);
        transact(
            MegaSpecId::SATIN,
            &mut db,
            caller,
            Some(contract),
            Bytes::new(),
            U256::ZERO,
            1_000_000,
        )
    }

    #[test]
    fn test_assert_stack_value_success() {
        let mut builder = BytecodeBuilder::default().push_number(0x2333u64);
        builder = builder.assert_stack_value(0, U256::from(0x2333u64));
        let bytecode = builder.build();
        let result = execute_bytecode(bytecode);
        assert!(result.unwrap().result.is_success(), "Transaction should succeed");
    }

    /// The value helpers move what they say: a call, a `CALLCODE`, the two creations' endowments
    /// and a destruction's balance, each leaving its transfer log in order; the return helpers
    /// return the top word and the last call's return data.
    #[test]
    fn test_the_value_helpers_move_value() {
        use crate::test_utils::transfer_log;
        let caller = address!("0000000000000000000000000000000000100000");
        let contract = address!("0000000000000000000000000000000000100001");
        let receiver = address!("0000000000000000000000000000000000100002");
        let one = U256::from(1);
        let code = BytecodeBuilder::default()
            .call(receiver, one)
            .callcode(receiver, one)
            .create(one, [])
            .create2(one, [], U256::ZERO)
            .append_many([POP, POP, POP, POP])
            .selfdestruct(receiver)
            .build();
        let mut db = MemoryDatabase::default().account_balance(contract, U256::from(100));
        db.set_account_code(contract, code);
        let result = transact(
            MegaSpecId::SATIN,
            &mut db,
            caller,
            Some(contract),
            Bytes::new(),
            U256::ZERO,
            10_000_000,
        )
        .unwrap();
        assert!(result.result.is_success(), "{:?}", result.result);
        let nonce = result.state[&contract].info.nonce - 2;
        let created = contract.create(nonce);
        let created2 = contract.create2(B256::ZERO, alloy_primitives::keccak256([]));
        assert_eq!(
            result.result.logs(),
            [
                transfer_log(contract, receiver, one),
                transfer_log(contract, created, one),
                transfer_log(contract, created2, one),
                transfer_log(contract, receiver, U256::from(97)),
            ],
            "the call's, the two endowments' and the destruction's; the `CALLCODE` moves nothing",
        );
        assert_eq!(result.state[&receiver].info.balance, U256::from(98));

        let top = BytecodeBuilder::default().push_number(42_u8).return_top().build();
        let output = execute_bytecode(top).unwrap().result.into_output().unwrap();
        assert_eq!(U256::from_be_slice(&output), U256::from(42));
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
