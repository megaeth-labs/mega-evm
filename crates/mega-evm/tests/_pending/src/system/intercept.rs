//! Unit tests extracted from `crates/mega-evm/src/system/intercept.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/system/intercept.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256};
    use alloy_sol_types::SolCall;
    use revm::{
        bytecode::opcode::{CALLCODE, MSTORE, RETURN},
        context::tx::TxEnvBuilder,
    };

    use crate::{
        test_utils::{BytecodeBuilder, MemoryDatabase},
        IMegaAccessControl, IMegaLimitControl, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
        LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
    };

    const REMAINING_COMPUTE_GAS_SELECTOR: [u8; 4] =
        IMegaLimitControl::remainingComputeGasCall::SELECTOR;

    /// Pins the `abi_decode`-accepts-trailing-junk behaviour of `alloy-sol-types`'s
    /// `decode_sequence::<()>` for parameterless system-contract methods. The
    /// interceptor dispatch's selector-only admission relies on this: if a future
    /// alloy upgrade rejects trailing bytes, historical replays would diverge from
    /// the original `abi_decode` admission and break replay determinism on stable
    /// specs.
    #[test]
    fn test_alloy_abi_decode_accepts_selector_plus_trailing_bytes_on_zero_arg_calls() {
        let selector = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;
        let exact = selector;
        let with_junk: Vec<u8> = selector.iter().copied().chain(core::iter::once(0xff)).collect();
        let with_pad: Vec<u8> = selector.iter().copied().chain([0u8; 32]).collect();

        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&exact).is_ok(),
            "exact 4-byte selector must decode",
        );
        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&with_junk).is_ok(),
            "current alloy-sol-types accepts selector+1B junk for empty-tuple decode; if this \
             fails the pre-REX5 dispatch path's admission rule has changed — re-spec the \
             selector probe before landing",
        );
        assert!(
            IMegaAccessControl::disableVolatileDataAccessCall::abi_decode(&with_pad).is_ok(),
            "current alloy-sol-types accepts selector+32B padding for empty-tuple decode; if \
             this fails the pre-REX5 dispatch path's admission rule has changed — re-spec the \
             selector probe before landing",
        );
    }

    /// Unit test for the explicit call-scheme guard in `frame_init`.
    ///
    /// A `CALLCODE` to a system contract address with a recognized selector must NOT be
    /// intercepted.
    /// The scheme guard rejects it before any interceptor sees the call.
    /// The call falls through to on-chain bytecode, which reverts with `NotIntercepted()`,
    /// leaving the `CALLCODE` success flag as 0.
    #[test]
    fn test_callcode_scheme_guard_skips_interception() {
        let code = BytecodeBuilder::default()
            .mstore(0x0, REMAINING_COMPUTE_GAS_SELECTOR)
            // CALLCODE(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
            .push_number(0_u64) // retSize
            .push_number(0_u64) // retOffset
            .push_number(4_u64) // argsSize (selector length)
            .push_number(0_u64) // argsOffset (selector at memory[0])
            .push_number(0_u64) // value
            .push_address(LIMIT_CONTROL_ADDRESS)
            .push_number(100_000_u64) // gas
            .append(CALLCODE) // success flag (0=fail, 1=success) on stack
            .push_number(0_u64)
            .append(MSTORE)
            .push_number(32_u64)
            .push_number(0_u64)
            .append(RETURN)
            .build();

        let caller = alloy_primitives::address!("0000000000000000000000000000000000300000");
        let contract = alloy_primitives::address!("0000000000000000000000000000000000300001");

        let mut db = MemoryDatabase::default()
            .account_balance(caller, U256::from(1_000_000))
            .account_code(contract, code)
            .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);

        let mut context = MegaContext::new(&mut db, MegaSpecId::REX4);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = MegaEvm::new(context);
        let tx = TxEnvBuilder::default()
            .caller(caller)
            .call(contract)
            .gas_limit(100_000_000)
            .build_fill();
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());

        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
        assert!(result.result.is_success(), "outer tx should succeed");

        let output = result.result.output().expect("should have output");
        let success_flag = U256::from_be_slice(output);
        assert_eq!(
            success_flag,
            U256::ZERO,
            "CALLCODE to system contract must not be intercepted — scheme guard must reject it"
        );
    }
}
