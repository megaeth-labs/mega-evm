//! The `SequencerRegistry` system contract for the `MegaETH` EVM.
//!
//! This contract records the current sequencer, pending rotations, and rotation history.
//! It uses compile-time constants for the initial sequencer and admin — `address(0)` in storage
//! means "use the constant default".
//!
//! Unlike intercepted system contracts (`LimitControl`, `AccessControl`), this contract runs
//! normal on-chain bytecode. It does not have an interceptor.
//!
//! Rotation is applied via a pre-block EVM system call to `applyPendingChange()`, following
//! the same pattern as EIP-2935 and EIP-4788.

use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    Database,
};
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
    Database as RevmDatabase,
};

use crate::{MegaHardforks, MEGA_SYSTEM_ADDRESS};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The initial sequencer address, matching the Solidity `INITIAL_SEQUENCER` constant.
/// This must be identical to the value compiled into the contract bytecode.
pub const REX5_INITIAL_SEQUENCER: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The initial admin address, matching the Solidity `INITIAL_ADMIN` constant.
/// This must be identical to the value compiled into the contract bytecode.
pub const REX5_REGISTRY_ADMIN: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The code of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::ISequencerRegistry;

/// The storage slot index for `_currentSequencer` (slot 0 in the Solidity layout).
const SLOT_CURRENT_SEQUENCER: U256 = U256::ZERO;

/// The storage slot index for `_pendingSequencer` (slot 2 in the Solidity layout).
const SLOT_PENDING_SEQUENCER: U256 = U256::from_limbs([2, 0, 0, 0]);

/// The storage slot index for `_activationBlock` (slot 3 in the Solidity layout).
const SLOT_ACTIVATION_BLOCK: U256 = U256::from_limbs([3, 0, 0, 0]);

/// Ensures the `SequencerRegistry` contract is deployed at its designated address.
///
/// Uses the constant-bootstrap pattern: no storage is written during deployment.
/// The initial sequencer and admin are provided by compile-time constants in the bytecode.
/// The caller is responsible for committing the returned `EvmState` changes to the database.
pub fn transact_deploy_sequencer_registry<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_5_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    let acc = db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS)?;

    // If already deployed with the correct code hash, return early and mark as read.
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == SEQUENCER_REGISTRY_CODE_HASH {
            return Ok(Some(EvmState::from_iter([(
                SEQUENCER_REGISTRY_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Deploy: set bytecode only, no storage writes (constant bootstrap).
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = SEQUENCER_REGISTRY_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE));

    let mut revm_acc: Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, revm_acc)])))
}

/// Executes the pre-block `applyPendingChange()` system call on the `SequencerRegistry`
/// if a pending rotation is due on a REX5-active block.
///
/// This avoids an EVM system call on every block by first checking committed storage.
/// Follows the EIP-2935 / EIP-4788 pre-block system call pattern.
pub fn transact_apply_pending_sequencer_change_if_due<'a, DB, Halt>(
    hardforks: &impl MegaHardforks,
    evm: &mut impl alloy_evm::Evm<DB = &'a mut State<DB>, HaltReason = Halt>,
) -> Result<Option<EvmState>, BlockExecutionError>
where
    DB: Database + 'a,
{
    if !hardforks.is_rex_5_active_at_timestamp(evm.block().timestamp.saturating_to()) {
        return Ok(None);
    }

    let block_number = evm.block().number.to::<u64>();
    if !is_rotation_due(evm.db_mut(), block_number)? {
        return Ok(None);
    }

    transact_apply_pending_sequencer_change(evm)
}

/// Checks whether a pending sequencer rotation is due by reading committed storage.
///
/// Returns `true` only when the account exists, `_pendingSequencer != 0`,
/// and `block_number >= _activationBlock`.
pub(crate) fn is_rotation_due<DB: Database>(
    db: &mut State<DB>,
    block_number: u64,
) -> Result<bool, BlockExecutionError> {
    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;
    if acc.account_info().is_none() {
        return Ok(false);
    }

    let pending = RevmDatabase::storage(db, SEQUENCER_REGISTRY_ADDRESS, SLOT_PENDING_SEQUENCER)
        .map_err(BlockExecutionError::other)?;
    if pending.is_zero() {
        return Ok(false);
    }

    let activation = RevmDatabase::storage(db, SEQUENCER_REGISTRY_ADDRESS, SLOT_ACTIVATION_BLOCK)
        .map_err(BlockExecutionError::other)?;
    Ok(block_number >= activation.saturating_to::<u64>())
}

/// Executes the pre-block `applyPendingChange()` system call on the `SequencerRegistry`.
fn transact_apply_pending_sequencer_change<Halt>(
    evm: &mut impl alloy_evm::Evm<HaltReason = Halt>,
) -> Result<Option<EvmState>, BlockExecutionError> {
    let calldata = ISequencerRegistry::applyPendingChangeCall {}.abi_encode();
    let res = match evm.transact_system_call(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::BlockHashContractCall {
                message: format!("SequencerRegistry applyPendingChange() system call failed: {e}"),
            }
            .into());
        }
    };

    if !res.result.is_success() {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "SequencerRegistry applyPendingChange() reverted or halted".into(),
        }
        .into());
    }

    Ok(Some(res.state))
}

/// Resolves the current system address from the `SequencerRegistry` committed state.
///
/// This function must be called **after** `commit_system_call_outcomes()` so that both
/// the bootstrap deploy and any `applyPendingChange()` result are visible in the DB.
///
/// # Behavior
///
/// - Pre-REX5: returns `MEGA_SYSTEM_ADDRESS` (the legacy hardcoded constant).
/// - REX5 with `_currentSequencer == 0`: returns `REX5_INITIAL_SEQUENCER` (no rotation yet).
/// - REX5 with `_currentSequencer != 0`: returns the stored address.
/// - REX5 but registry missing or code hash mismatch: returns `BlockExecutionError`.
pub fn resolve_system_address<DB: Database>(
    spec: crate::MegaSpecId,
    db: &mut State<DB>,
) -> Result<Address, BlockExecutionError> {
    if !spec.is_enabled(crate::MegaSpecId::REX5) {
        return Ok(MEGA_SYSTEM_ADDRESS);
    }

    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;

    let Some(info) = acc.account_info() else {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "SequencerRegistry not deployed on REX5-active block".into(),
        }
        .into());
    };

    if info.code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Err(BlockValidationError::BlockHashContractCall {
            message: format!(
                "SequencerRegistry code hash mismatch: expected {}, got {}",
                SEQUENCER_REGISTRY_CODE_HASH, info.code_hash
            ),
        }
        .into());
    }

    let value = RevmDatabase::storage(db, SEQUENCER_REGISTRY_ADDRESS, SLOT_CURRENT_SEQUENCER)
        .map_err(BlockExecutionError::other)?;

    if value.is_zero() {
        return Ok(REX5_INITIAL_SEQUENCER);
    }

    Ok(Address::from_word(value.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, B256};
    use revm::{context::BlockEnv, database::InMemoryDB, state::AccountInfo};

    use crate::{MegaHardfork, MegaHardforkConfig, MegaSpecId};
    use alloy_hardforks::ForkCondition;
    use revm::{context::ContextTr as _, handler::EvmTr as _};

    #[test]
    fn test_sequencer_registry_code_hash_matches() {
        let computed_hash = keccak256(&SEQUENCER_REGISTRY_CODE);
        assert_eq!(computed_hash, SEQUENCER_REGISTRY_CODE_HASH);
    }

    /// Verifies that the Rust-side bootstrap constants match the Solidity constants compiled
    /// into the `SequencerRegistry` bytecode by deploying the contract in a test EVM and calling
    /// `currentSequencer()` and `admin()` on a fresh (zero-storage) deploy.
    ///
    /// On fresh deploy, `currentSequencer()` returns the bytecode `INITIAL_SEQUENCER` constant
    /// and `admin()` returns the bytecode `INITIAL_ADMIN` constant. Comparing these against
    /// the Rust constants is a definitive check through actual EVM execution.
    #[test]
    fn test_rust_constants_match_bytecode_constants() {
        use alloy_sol_types::SolCall;
        use revm::context::result::ExecutionResult;

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let caller = alloy_primitives::address!("0x0000000000000000000000000000000000000001");

        // Call currentSequencer() on fresh deploy → returns INITIAL_SEQUENCER constant
        let mut ctx = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_tx(
            crate::MegaTransaction::new(
                revm::context::tx::TxEnvBuilder::default()
                    .caller(caller)
                    .call(SEQUENCER_REGISTRY_ADDRESS)
                    .gas_limit(100_000)
                    .data(alloy_primitives::Bytes::from(
                        ISequencerRegistry::currentSequencerCall {}.abi_encode(),
                    ))
                    .build_fill(),
            ),
        );
        ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(alloy_primitives::U256::from(0));
            chain.operator_fee_constant = Some(alloy_primitives::U256::from(0));
        });
        let mut evm = crate::MegaEvm::new(ctx);
        let mut tx = evm.ctx().tx().clone();
        tx.enveloped_tx = Some(alloy_primitives::Bytes::new());
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, tx).expect("currentSequencer() call failed");
        let ExecutionResult::Success { output, .. } = result.result else {
            panic!("currentSequencer() call did not succeed");
        };
        let returned_addr = Address::from_word(alloy_primitives::B256::from_slice(output.data()));
        assert_eq!(
            returned_addr, REX5_INITIAL_SEQUENCER,
            "Solidity INITIAL_SEQUENCER ({returned_addr}) != Rust REX5_INITIAL_SEQUENCER ({}). \
             Constants are out of sync.",
            REX5_INITIAL_SEQUENCER,
        );

        // Call admin() on fresh deploy → returns INITIAL_ADMIN constant
        let mut ctx = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_tx(
            crate::MegaTransaction::new(
                revm::context::tx::TxEnvBuilder::default()
                    .caller(caller)
                    .call(SEQUENCER_REGISTRY_ADDRESS)
                    .gas_limit(100_000)
                    .data(alloy_primitives::Bytes::from(
                        ISequencerRegistry::adminCall {}.abi_encode(),
                    ))
                    .build_fill(),
            ),
        );
        ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(alloy_primitives::U256::from(0));
            chain.operator_fee_constant = Some(alloy_primitives::U256::from(0));
        });
        let mut evm = crate::MegaEvm::new(ctx);
        let mut tx = evm.ctx().tx().clone();
        tx.enveloped_tx = Some(alloy_primitives::Bytes::new());
        let result = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("admin() call failed");
        let ExecutionResult::Success { output, .. } = result.result else {
            panic!("admin() call did not succeed");
        };
        let returned_addr = Address::from_word(alloy_primitives::B256::from_slice(output.data()));
        assert_eq!(
            returned_addr, REX5_REGISTRY_ADMIN,
            "Solidity INITIAL_ADMIN ({returned_addr}) != Rust REX5_REGISTRY_ADMIN ({}). \
             Constants are out of sync.",
            REX5_REGISTRY_ADMIN,
        );

        // Verify resolve_system_address agrees with Solidity currentSequencer() on fresh deploy.
        let mut state = State::builder().with_database(&mut db).build();
        // Ensure the account is loaded into State cache for resolve_system_address.
        state.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).expect("should load account");
        let resolved =
            resolve_system_address(MegaSpecId::REX5, &mut state).expect("resolve should succeed");
        assert_eq!(
            resolved, REX5_INITIAL_SEQUENCER,
            "resolve_system_address() ({resolved}) disagrees with Solidity currentSequencer(). \
             Rust fallback logic and Solidity fallback logic are out of sync.",
        );
    }

    #[test]
    fn test_deploy_requires_rex5() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result =
            transact_deploy_sequencer_registry(MegaHardforkConfig::default(), 0, &mut state)
                .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_deploy_on_fresh_db() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_sequencer_registry(&hardforks, 0, &mut state).unwrap();

        let evm_state = result.unwrap();
        let account = evm_state.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(account.is_touched());
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        // No storage writes — constant bootstrap pattern.
        assert!(account.storage.is_empty(), "deploy should not write storage");
    }

    #[test]
    fn test_deploy_is_idempotent() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                balance: Default::default(),
                nonce: 0,
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_sequencer_registry(&hardforks, 0, &mut state).unwrap();

        let evm_state = result.unwrap();
        let account = evm_state.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(!account.is_created(), "idempotent deploy should not re-create");
    }

    #[test]
    fn test_deploy_at_later_timestamp() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks =
            MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(100));

        let result = transact_deploy_sequencer_registry(&hardforks, 99, &mut state).unwrap();
        assert_eq!(result, None, "should not deploy before Rex5 activation");

        let result = transact_deploy_sequencer_registry(&hardforks, 100, &mut state).unwrap();
        let account = result.unwrap().get(&SEQUENCER_REGISTRY_ADDRESS).unwrap().clone();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
    }

    #[test]
    fn test_deploy_overwrites_wrong_code_hash() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                balance: Default::default(),
                nonce: 0,
                code_hash: B256::ZERO,
                code: Some(Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00]))),
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result = transact_deploy_sequencer_registry(&hardforks, 0, &mut state).unwrap();

        let evm_state = result.unwrap();
        let account = evm_state.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert!(account.is_created());
    }

    #[test]
    fn test_transact_apply_pending_sequencer_change_if_due_errors_on_revert() {
        let mut db = InMemoryDB::default();
        let reverting_code = Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[
            0x60, 0x00, 0x60, 0x00, 0xfd,
        ]));
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: reverting_code.hash_slow(),
                code: Some(reverting_code),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SLOT_PENDING_SEQUENCER,
            address!("0x1111111111111111111111111111111111111111").into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(SEQUENCER_REGISTRY_ADDRESS, SLOT_ACTIVATION_BLOCK, U256::from(7))
            .unwrap();

        let mut state = State::builder().with_database(&mut db).build();
        let mut ctx = crate::MegaContext::new(&mut state, MegaSpecId::REX5).with_block(BlockEnv {
            number: U256::from(7),
            timestamp: U256::from(1),
            ..Default::default()
        });
        ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(ctx);
        let hardforks =
            MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));

        let result = transact_apply_pending_sequencer_change_if_due(&hardforks, &mut evm);

        assert!(result.is_err(), "reverted applyPendingChange() should surface as an error");
    }

    #[test]
    fn test_resolve_pre_rex5_returns_legacy() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let addr = resolve_system_address(MegaSpecId::REX4, &mut state).unwrap();
        assert_eq!(addr, crate::MEGA_SYSTEM_ADDRESS);
    }

    #[test]
    fn test_resolve_rex5_zero_slot_returns_initial() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let addr = resolve_system_address(MegaSpecId::REX5, &mut state).unwrap();
        assert_eq!(addr, REX5_INITIAL_SEQUENCER);
    }

    #[test]
    fn test_resolve_rex5_nonzero_slot_returns_stored() {
        let rotated_addr = address!("0x1111111111111111111111111111111111111111");

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SLOT_CURRENT_SEQUENCER,
            rotated_addr.into_word().into(),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let addr = resolve_system_address(MegaSpecId::REX5, &mut state).unwrap();
        assert_eq!(addr, rotated_addr);
    }

    #[test]
    fn test_resolve_rex5_missing_registry_errors() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result = resolve_system_address(MegaSpecId::REX5, &mut state);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_rex5_wrong_code_hash_errors() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: B256::ZERO,
                code: Some(Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[0x60, 0x00]))),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let result = resolve_system_address(MegaSpecId::REX5, &mut state);
        assert!(result.is_err());
    }
}
