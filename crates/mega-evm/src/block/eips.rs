//! The pre-block and post-block calls a block makes before and after its transactions.
//!
//! Each helper runs the call and hands back the state it produced; none of them commits. A
//! node's witness generator needs to see the read and write set of every step of block
//! execution, and a helper that committed straight to the database would hide its own.
//!
//! A pre-block call is a system call on [`pre_block_call_gas_limit`], and a block whose
//! pre-block call does not succeed is refused: the state the protocol maintains before the
//! block's transactions is not optional, and a block that could not write it is not one the
//! chain can build on. A database error the database calls fatal is not a verdict on the block:
//! it is an internal error. Every pre-block call runs through [`transact_pre_block_call`], which
//! holds that rule for all of them.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{
    boxed::Box,
    format,
    string::{String, ToString},
};

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::block::{BlockExecutionError, BlockValidationError};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, Bytes, B256, U256};
use op_revm::OpHaltReason;
use revm::{
    context::{
        result::{EVMError, ResultAndState},
        Block, ContextTr, DBErrorMarker,
    },
    database::State,
    handler::SYSTEM_CALL_REGULAR_GAS_LIMIT,
    state::{Account, EvmState},
    Database,
};

use crate::{ExternalEnvTypes, MegaEvm};

/// The caller every pre-block system call runs as.
pub use alloy_eips::eip4788::SYSTEM_ADDRESS;

/// The gas limit of every pre-block system call in a block whose gas limit is `block_gas_limit`:
/// the block's gas limit, and never less than 30,000,000.
///
/// It is the legacy engine's budget, kept as it was rather than replaced by a number nobody
/// chose. The legacy engine sized it to absorb the storage gas a crowded SALT bucket multiplies,
/// which could take a pre-block write past revm's 30M default. On Satin neither half of that
/// holds: a system call prices its state at the minimum bucket, and it runs on at most
/// 30,000,000 of regular gas, the rest of its gas limit being its state-gas reservoir. So what
/// this budget adds above 30M is reservoir, which the call's writes draw first and which nobody
/// pays for when they do not, and the regular budget is 30M whatever the block. In a block whose
/// gas limit is 30M or less the call has no reservoir, and its writes are paid out of the
/// regular budget, which holds them many times over.
pub fn pre_block_call_gas_limit(block_gas_limit: u64) -> u64 {
    block_gas_limit.max(SYSTEM_CALL_REGULAR_GAS_LIMIT)
}

/// Runs the pre-block call `name`: a system call from [`SYSTEM_ADDRESS`] to `target` with `data`,
/// on the pre-block budget of the block `evm` is set up for ([`pre_block_call_gas_limit`]).
/// Answers its result and state; nothing is committed.
///
/// One rule holds for every pre-block call, and it is the rule alloy-evm's block executor holds a
/// transaction's database error to ([`BlockExecutionError::evm`]):
///
/// - a database error the database calls fatal ([`DBErrorMarker::is_fatal`]) is an internal error
///   ([`BlockExecutionError::other`]): a read the database could not serve says nothing about the
///   block, and a node must not refuse a valid block because its storage failed;
/// - any other failure refuses the block with the call's own error, which `refused` builds from a
///   message: a database error that is not fatal, an EVM error other than the database's, and an
///   outcome that is not a success, whether the call reverted or halted. Under revm's `State` the
///   one database error that is not fatal is an EIP-7928 block access list that does not cover a
///   read, which is a verdict on the block.
pub(crate) fn transact_pre_block_call<DB, INSP, ExtEnvs>(
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
    name: &str,
    target: Address,
    data: Bytes,
    refused: impl FnOnce(String) -> BlockExecutionError,
) -> Result<ResultAndState<OpHaltReason>, BlockExecutionError>
where
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
{
    let gas_limit = pre_block_call_gas_limit(evm.ctx().block().gas_limit());
    let outcome =
        match evm.transact_system_call_with_gas_limit(SYSTEM_ADDRESS, target, data, gas_limit) {
            Ok(outcome) => outcome,
            Err(EVMError::Database(error)) if error.is_fatal() => {
                return Err(BlockExecutionError::other(error))
            }
            Err(error) => return Err(refused(error.to_string())),
        };
    if !outcome.result.is_success() {
        let message = format!("the {name} pre-block call did not succeed: {:?}", outcome.result);
        return Err(refused(message));
    }
    Ok(outcome)
}

/// Runs the pre-block call to the [EIP-2935] block hashes contract.
///
/// Answers `None`, having run nothing, when Prague is not active or the block is the genesis
/// block, where EIP-2935 makes no call. A call that does not succeed refuses the block with
/// [`BlockValidationError::BlockHashContractCall`], and its state is handed to nobody; a fatal
/// database error is an internal error, as for every pre-block call. The state is not committed.
///
/// [EIP-2935]: https://eips.ethereum.org/EIPS/eip-2935
pub fn transact_blockhashes_contract_call<H, DB, INSP, ExtEnvs>(
    hardforks: H,
    parent_block_hash: B256,
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<Option<ResultAndState<OpHaltReason>>, BlockExecutionError>
where
    H: EthereumHardforks,
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
{
    let block = evm.ctx().block();
    if !hardforks.is_prague_active_at_timestamp(block.timestamp().saturating_to()) {
        return Ok(None);
    }

    // The genesis block has no parent to record.
    if block.number().is_zero() {
        return Ok(None);
    }

    let refused = |message| BlockValidationError::BlockHashContractCall { message }.into();
    let data = parent_block_hash.0.into();
    transact_pre_block_call(evm, "EIP-2935", HISTORY_STORAGE_ADDRESS, data, refused).map(Some)
}

/// Runs the pre-block call to the [EIP-4788] beacon block root contract.
///
/// Answers `None`, having run nothing, when Cancun is not active or the block is the genesis
/// block, where EIP-4788 makes no call and requires a zero root. A call that does not succeed
/// refuses the block with [`BlockValidationError::BeaconRootContractCall`], and its state is
/// handed to nobody; a fatal database error is an internal error, as for every pre-block call.
/// The state is not committed.
///
/// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
pub fn transact_beacon_root_contract_call<H, DB, INSP, ExtEnvs>(
    hardforks: H,
    parent_beacon_block_root: Option<B256>,
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<Option<ResultAndState<OpHaltReason>>, BlockExecutionError>
where
    H: EthereumHardforks,
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
{
    let block = evm.ctx().block();
    if !hardforks.is_cancun_active_at_timestamp(block.timestamp().saturating_to()) {
        return Ok(None);
    }

    let parent_beacon_block_root =
        parent_beacon_block_root.ok_or(BlockValidationError::MissingParentBeaconBlockRoot)?;

    if block.number().is_zero() {
        if !parent_beacon_block_root.is_zero() {
            return Err(BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                parent_beacon_block_root,
            }
            .into());
        }
        return Ok(None);
    }

    let refused = |message| {
        BlockValidationError::BeaconRootContractCall {
            parent_beacon_block_root: Box::new(parent_beacon_block_root),
            message,
        }
        .into()
    };
    let data = parent_beacon_block_root.0.into();
    transact_pre_block_call(evm, "EIP-4788", BEACON_ROOTS_ADDRESS, data, refused).map(Some)
}

/// The state that applying `balances` to `db` would produce. Nothing is committed.
///
/// Equivalent to revm's own `increment_balances`, in state rather than in place: committing what
/// this returns leaves the same accounts, balances and account statuses behind, and a witness
/// generator gets to see the delta in between.
pub fn transact_balance_increments<DB: Database>(
    balances: impl IntoIterator<Item = (Address, u128)>,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    let mut state = EvmState::default();

    for (address, balance_increment) in balances {
        if balance_increment == 0 {
            continue;
        }
        let cache_account = db.load_cache_account(address)?;
        let account_info = cache_account.account_info().unwrap_or_default();
        let mut account = Account::default().with_info(account_info);
        account.info.balance += U256::from(balance_increment);
        account.mark_touch();
        state.insert(address, account);
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
        MegaContext, MegaSpecId,
    };
    use alloy_primitives::address;
    use revm::{
        context::BlockEnv,
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
        DatabaseCommit,
    };
    use std::vec;

    #[test]
    fn test_balance_increment_commit_equivalence() {
        // The property that matters: this is equivalent to revm's `increment_balances`, which
        // the block executor would otherwise call in place.

        let addr1 = address!("0x1000000000000000000000000000000000000001");
        let addr2 = address!("0x2000000000000000000000000000000000000002");
        let addr3 = address!("0x3000000000000000000000000000000000000003");

        // Fund only addr1 and addr2, but leave addr3 empty (non-existent)
        let setup_db = |db: &mut InMemoryDB| {
            for (addr, balance, nonce) in [(addr1, 1000u64, 5u64), (addr2, 2000u64, 10u64)] {
                db.insert_account_info(
                    addr,
                    AccountInfo { balance: U256::from(balance), nonce, ..Default::default() },
                );
            }
        };

        let mut db1 = InMemoryDB::default();
        setup_db(&mut db1);
        let mut state1 = State::builder().with_database(&mut db1).build();

        let mut db2 = InMemoryDB::default();
        setup_db(&mut db2);
        let mut state2 = State::builder().with_database(&mut db2).build();

        let balance_increments = vec![(addr1, 100u128), (addr2, 200u128), (addr3, 300u128)];

        // In place, as revm does it.
        revm::database_interface::DatabaseCommitExt::increment_balances(
            &mut state1,
            balance_increments.clone(),
        )
        .expect("increment_balances should succeed");

        // As state, then committed: the commit is what applies the status transitions, so the
        // two paths must land on the same accounts.
        let result_state = transact_balance_increments(balance_increments.clone(), &mut state2)
            .expect("transact_balance_increments should succeed")
            .expect("Should return state");
        state2.commit(result_state);

        for (addr, _expected_increment) in balance_increments {
            let account1 = state1.load_cache_account(addr).expect("Should load from state1");
            let account2 = state2.load_cache_account(addr).expect("Should load from state2");

            let info1 = account1.account_info().expect("Should have account info");
            let info2 = account2.account_info().expect("Should have account info");

            assert_eq!(
                info1.balance, info2.balance,
                "Balance for {addr:?} should be identical after both methods"
            );
            assert_eq!(info1.nonce, info2.nonce, "Nonce for {addr:?} should be identical");
            assert_eq!(
                info1.code_hash, info2.code_hash,
                "Code hash for {addr:?} should be identical"
            );
            assert_eq!(
                account1.status, account2.status,
                "Account status for {addr:?} should be identical after both methods"
            );
        }
    }

    /// A zero increment is not a touch: it leaves no account in the state at all.
    #[test]
    fn test_zero_increments_leave_no_account_behind() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let produced = transact_balance_increments(
            [(address!("0x1000000000000000000000000000000000000001"), 0u128)],
            &mut state,
        )
        .expect("no database error")
        .expect("Should return state");

        assert!(produced.is_empty());
    }

    /// A database error that says it is not fatal, as a block access list that does not cover a
    /// read does.
    #[derive(Debug)]
    struct NotFatal(InjectedDbError);

    impl core::fmt::Display for NotFatal {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl core::error::Error for NotFatal {}

    impl DBErrorMarker for NotFatal {
        fn is_fatal(&self) -> bool {
            false
        }
    }

    /// An [`ErrorInjectingDatabase`] whose errors are not fatal.
    #[derive(Debug)]
    struct NotFatalDb(ErrorInjectingDatabase);

    impl Database for NotFatalDb {
        type Error = NotFatal;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, NotFatal> {
            self.0.basic(address).map_err(NotFatal)
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, NotFatal> {
            self.0.code_by_hash(code_hash).map_err(NotFatal)
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, NotFatal> {
            self.0.storage(address, index).map_err(NotFatal)
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, NotFatal> {
            self.0.block_hash(number).map_err(NotFatal)
        }
    }

    /// A pre-block call's database error is an internal error only when the database calls it
    /// fatal; one it does not refuses the block with the call's own error, as a transaction's
    /// does.
    #[test]
    fn test_a_database_error_is_internal_only_when_it_is_fatal() {
        let unreadable = address!("0x7000000000000000000000000000000000000007");
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        db.fail_on_account = Some(unreadable);
        let block =
            BlockEnv { number: U256::from(1_000), gas_limit: 30_000_000, ..Default::default() };
        let refused = |message| BlockValidationError::BlockHashContractCall { message }.into();

        let mut fatal = db.clone();
        let mut evm =
            MegaEvm::new(MegaContext::new(&mut fatal, MegaSpecId::SATIN).with_block(block.clone()));
        let err = transact_pre_block_call(&mut evm, "test", unreadable, Bytes::new(), refused)
            .expect_err("the read fails");
        assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");

        let mut not_fatal = NotFatalDb(db);
        let mut evm =
            MegaEvm::new(MegaContext::new(&mut not_fatal, MegaSpecId::SATIN).with_block(block));
        let err = transact_pre_block_call(&mut evm, "test", unreadable, Bytes::new(), refused)
            .expect_err("the read fails");
        assert!(
            matches!(
                &err,
                BlockExecutionError::Validation(BlockValidationError::BlockHashContractCall {
                    message
                }) if message.contains("injected basic() error")
            ),
            "{err:?}"
        );
    }
}
