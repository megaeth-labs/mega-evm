//! The external environments of a block, through `ExternalEnvFactory`: the node's factory is
//! asked once for the environments of the block it executes, by that block's number, and every
//! transaction of the block prices its state gas by the SALT buckets those environments report and
//! reads the Oracle's storage through their oracle service.

use std::{cell::RefCell, rc::Rc};

use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, BlockNumber, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    satin_gas_params,
    system::{IOracle, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::BytecodeBuilder,
    BucketId, ExternalEnvFactory, ExternalEnvs, MegaBlockExecutorFactory, MegaEvmFactory, SaltEnv,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use revm::{context_interface::cfg::GasId, database::State};

use crate::common::{self, BLOCK_NUMBER};

/// The contract whose three slots the block's transactions write.
const WRITER: Address = address!("0x1000000000000000000000000000000000000abc");

/// The slot of the Oracle a transaction reads, and what the oracle service answers for it.
const ORACLE_SLOT: U256 = U256::from_limbs([5, 0, 0, 0]);
const ORACLE_VALUE: U256 = U256::from_limbs([0xabc, 0, 0, 0]);

/// A node's factory: it hands every block the same environments and records which blocks asked.
#[derive(Clone, Debug)]
struct Factory {
    envs: TestExternalEnvs,
    asked: Rc<RefCell<Vec<BlockNumber>>>,
}

impl ExternalEnvFactory for Factory {
    type EnvTypes = TestExternalEnvs;

    fn external_envs(&self, block: BlockNumber) -> ExternalEnvs<TestExternalEnvs> {
        self.asked.borrow_mut().push(block);
        ExternalEnvs { salt_env: self.envs.clone(), oracle_env: self.envs.clone() }
    }
}

/// The bucket the slot `slot` of [`WRITER`] lives in.
fn bucket(slot: u64) -> BucketId {
    <TestExternalEnvs as SaltEnv>::bucket_id_for_slot(WRITER, U256::from(slot))
}

/// Code that writes 1 into the slot named by the first byte of its calldata.
fn writer() -> alloy_primitives::Bytes {
    use revm::bytecode::opcode::{CALLDATALOAD, PUSH0, SHR, SSTORE, STOP};
    BytecodeBuilder::default()
        .push_number(1_u8)
        .append_many([PUSH0, CALLDATALOAD])
        .push_number(248_u8)
        .append_many([SHR, SSTORE, STOP])
        .build()
}

/// A block with three fresh slots written — one in a bucket at the minimum capacity, one at twice
/// it, one at eight times it — and a read of the Oracle's storage the oracle service answers:
/// every state gas charge is the schedule's entry times the bucket's multiplier, and the regular
/// gas does not move.
#[test]
fn test_a_blocks_environments_come_from_the_factory() {
    let envs = TestExternalEnvs::new()
        .with_bucket_capacity(bucket(2), MIN_BUCKET_SIZE as u64 * 2)
        .with_bucket_capacity(bucket(8), MIN_BUCKET_SIZE as u64 * 8)
        .with_oracle_storage(ORACLE_SLOT, ORACLE_VALUE);
    let asked = Rc::new(RefCell::new(vec![]));
    let factory = MegaBlockExecutorFactory::new(
        OpAlloyReceiptBuilder::default(),
        common::chain_spec(),
        MegaEvmFactory::new().with_external_env_factory(Factory { envs, asked: asked.clone() }),
    );

    let mut db = common::database();
    db.set_account_code(WRITER, writer());
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE);
    let mut state = State::builder().with_database(db).build();
    let evm = factory.evm_factory().create_evm(&mut state, common::evm_env());
    let mut executor = factory.create_executor(evm, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    assert_eq!(*asked.borrow(), vec![BLOCK_NUMBER], "one block, one set of environments");

    let entry = satin_gas_params().get(GasId::sstore_set_state_gas());
    let mut regular = vec![];
    for (nonce, m) in [(0, 1), (1, 2), (2, 8)] {
        let tx = common::recovered(common::tx(nonce, WRITER, vec![m as u8].into(), 1_000_000));
        let outcome = executor.run_transaction(&tx).expect("it executes");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.gas.state, entry * m, "the slot's bucket at m = {m}");
        regular.push(outcome.gas.regular);
        executor.commit_transaction_outcome(outcome).expect("the block has room");
    }
    assert!(
        regular.iter().all(|gas| *gas == regular[0]),
        "regular gas does not scale: {regular:?}"
    );

    let read = IOracle::getSlotCall { slot: ORACLE_SLOT }.abi_encode();
    let tx = common::recovered(common::tx(3, ORACLE_CONTRACT_ADDRESS, read.into(), 1_000_000));
    let outcome = executor.run_transaction(&tx).expect("it executes");
    assert_eq!(
        IOracle::getSlotCall::abi_decode_returns(outcome.result.output().unwrap()).unwrap(),
        B256::from(ORACLE_VALUE),
        "the oracle service answered the read",
    );
    executor.commit_transaction_outcome(outcome).expect("the block has room");
    assert_eq!(*asked.borrow(), vec![BLOCK_NUMBER], "the environments served the whole block");
}
