//! Unit tests extracted from `crates/mega-evm/src/sandbox/execution.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/sandbox/execution.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::result::Output;

    /// Test error type that lets us drive `process_sandbox_transact_result`'s
    /// `Err` arms (which map to `SandboxOutcome::Rejected`) directly without
    /// standing up a full sandbox EVM. The two arms differ only by
    /// `IsTxError::is_tx_error()`, so a single struct with a configurable flag
    /// covers both selector mappings.
    struct FakeTxErr {
        is_tx: bool,
        msg: &'static str,
    }

    impl core::fmt::Display for FakeTxErr {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl IsTxError for FakeTxErr {
        fn is_tx_error(&self) -> bool {
            self.is_tx
        }
    }

    /// `validate_signer_code` MUST be a no-op when `cfg.disable_eip3607 = true`, even
    /// when the provided `AccountInfo` has bytecode that would otherwise trip the
    /// caller-with-code check. Mirrors the canonical revm `validate_account_nonce_and_code`
    /// semantics and pins the early-return contract.
    #[test]
    fn test_validate_signer_code_disable_eip3607_short_circuits() {
        use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
        use revm::{
            bytecode::Bytecode,
            primitives::{keccak256, Bytes as PrimitivesBytes},
        };

        let code_bytes = PrimitivesBytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        let code_hash = keccak256(&code_bytes);
        let signer_info = AccountInfo {
            nonce: 0,
            balance: U256::ZERO,
            code_hash,
            code: Some(Bytecode::new_raw(code_bytes)),
        };

        let mut ctx =
            MegaContext::<_, EmptyExternalEnv>::new(MemoryDatabase::default(), MegaSpecId::REX5);
        ctx.modify_cfg(|cfg| cfg.disable_eip3607 = true);

        let out = validate_signer_code(&ctx, &signer_info);
        assert!(
            matches!(out, Ok(())),
            "disable_eip3607 must short-circuit before the code inspection; got {out:?}",
        );
    }

    /// `charge_caller_materialization_pre_sandbox` MUST surface a SALT/dynamic-gas failure
    /// as `Err(KeylessDeployError::InternalError)` without writing to `ctx.error()`.
    ///
    /// The contract is load-bearing: op-revm's `Handler::execution_result` drains
    /// `ctx.error()` and converts any non-empty `ContextError` into an EVM-level
    /// `Err(Custom(...))`. Routing a SALT failure through the
    /// `HostExt::new_account_storage_gas` helper — which stashes the error into
    /// `ctx.error()` — would promote an interceptor-level failure into a whole-transaction
    /// execution error and break the `InternalError()` selector contract. The
    /// pre-sandbox charge path must therefore call `DynamicGasCost::new_account_gas`
    /// directly and keep the error local.
    #[test]
    fn test_charge_caller_materialization_salt_failure_returns_internal_error_without_ctx_pollution(
    ) {
        use crate::{test_utils::MemoryDatabase, BucketId, OracleEnv, SaltEnv};
        use alloy_primitives::{address, B256};
        use core::fmt::{self, Display, Formatter};

        // Minimal `SaltEnv` whose `get_bucket_capacity` always fails. This drives
        // `DynamicGasCost::new_account_gas` (called from
        // `charge_caller_materialization_pre_sandbox`) to return `Err`, exercising the
        // failure-handling path under test.
        #[derive(Debug, Clone, Copy)]
        struct AlwaysFailSalt;

        #[derive(Debug, Clone)]
        struct InjectedSaltError;
        impl Display for InjectedSaltError {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("injected SALT failure")
            }
        }

        impl SaltEnv for AlwaysFailSalt {
            type Error = InjectedSaltError;
            fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
                Err(InjectedSaltError)
            }
            fn bucket_id_for_account(_account: Address) -> BucketId {
                0
            }
            fn bucket_id_for_slot(_address: Address, _key: U256) -> BucketId {
                0
            }
        }

        #[derive(Debug, Clone, Copy)]
        struct NoopOracle;
        impl OracleEnv for NoopOracle {
            fn get_oracle_storage(&self, _slot: U256) -> Option<U256> {
                None
            }
            fn on_hint(&self, _from: Address, _topic: B256, _data: Bytes) {}
        }

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let db = MemoryDatabase::default();
        let envs: crate::ExternalEnvs<(AlwaysFailSalt, NoopOracle)> =
            crate::ExternalEnvs { salt_env: AlwaysFailSalt, oracle_env: NoopOracle };
        let mut ctx = MegaContext::new(db, MegaSpecId::REX5).with_external_envs(envs);

        let mut gas = Gas::new(10_000_000);
        let return_memory_offset = 0..0;

        // Signer is unmaterialized (default `AccountInfo` is `is_empty()` == true)
        // → the materialization branch fires.
        let signer_info = AccountInfo::default();
        let out = charge_caller_materialization_pre_sandbox(
            &ctx,
            &mut gas,
            signer,
            &signer_info,
            &return_memory_offset,
        );
        assert!(
            matches!(out, Err(KeylessDeployError::InternalError)),
            "SALT failure must surface as InternalError, got {out:?}",
        );

        // The whole point of the fix: ctx.error() must NOT have been polluted, so the outer
        // transaction's `execution_result` doesn't drain it into `Err(Custom)`.
        let ctx_error = core::mem::replace(ctx.error(), Ok(()));
        assert!(
            matches!(ctx_error, Ok(())),
            "ctx.error() must remain Ok after a SALT failure in the pre-sandbox charge; \
             got {ctx_error:?}",
        );

        // record_deposit_caller_creation must NOT have fired either.
        assert_eq!(
            ctx.additional_limit.borrow().get_usage().state_growth,
            0,
            "deposit-caller state-growth must not be recorded when the storage gas computation fails",
        );
    }
}
