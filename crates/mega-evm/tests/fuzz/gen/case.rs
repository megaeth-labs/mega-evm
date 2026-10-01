//! A case: a world, a transaction into it and the programs of its contracts, and how it runs.

use alloy_primitives::{Address, Bytes, B256, U256};
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE},
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
    },
    test_utils::{zero_fee_l1_block_info, MemoryDatabase},
    ExternalEnvs, MegaContext, MegaEvm, MegaTransaction, MegaTransactionOutcome, SaltEnv,
    TestExternalEnvs, MIN_BUCKET_SIZE,
};
use proptest::prelude::*;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    state::{AccountInfo, Bytecode},
    Inspector,
};

use super::{
    program::{program, Program},
    tx::{tx, Shape, Tx},
    world::{world, AccountShape, World},
    Flavor, Who, CHAIN_ID, SYSTEM_ADDRESS,
};

/// The external environments a case runs with: the SALT capacities and the Oracle's answers,
/// with an error type so a bucket can fail.
pub(crate) type Envs = TestExternalEnvs<String>;

/// A case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Case {
    pub(crate) world: World,
    pub(crate) tx: Tx,
    /// The program of the contract the transaction calls, or its init code.
    pub(crate) main: Program,
    /// `A`'s program.
    pub(crate) a: Program,
    /// `B`'s program.
    pub(crate) b: Program,
}

pub(crate) fn case(flavor: Flavor) -> impl Strategy<Value = Case> {
    (world(), tx(flavor), program(flavor), program(flavor), program(flavor))
        .prop_map(|(world, tx, main, a, b)| Case { world, tx, main, a, b })
}

/// How a case ran: the outcome, or the error the engine refused the transaction with.
pub(crate) type Execution = Result<MegaTransactionOutcome, String>;

impl Case {
    /// The pre-state: the system contracts, the registry naming the system address, the fixed
    /// accounts with their programs, the delegation, and the keyless signer's funds.
    pub(crate) fn database(&self) -> MemoryDatabase {
        let mut db = MemoryDatabase::default()
            .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
            .account_code(
                HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
                HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
            )
            .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
            .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
            .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
            .sequencer_registry(SYSTEM_ADDRESS);
        let world = &self.world;
        install(&mut db, Who::Caller.address(), &world.caller, None);
        install(&mut db, Who::Contract.address(), &world.contract, Some(self.main.assemble()));
        install(&mut db, Who::A.address(), &world.a, Some(self.a.assemble()));
        install(&mut db, Who::B.address(), &world.b, Some(self.b.assemble()));
        db.set_account_balance(Who::Beneficiary.address(), world.beneficiary_balance.wei());
        if let Some(delegation) = world.delegation {
            let address = delegation.delegator.address();
            let balance = db.basic_ref_balance(address);
            let nonce = world.nonce_of(address);
            let code = Bytecode::new_eip7702(delegation.delegate.address());
            db.insert_account_info(
                address,
                AccountInfo {
                    balance,
                    nonce,
                    code_hash: code.hash_slow(),
                    code: Some(code),
                    ..Default::default()
                },
            );
        }
        if world.system_nonce > 0 {
            db.set_account_nonce(SYSTEM_ADDRESS, world.system_nonce as u64);
        }
        if let (Some(deployment), Shape::Keyless { signer_funded: true, .. }) =
            (self.tx.deployment(), &self.tx.shape)
        {
            db.set_account_balance(deployment.signer, U256::from(10u64.pow(18)));
        }
        db
    }

    /// The SALT capacities and the Oracle's answers.
    pub(crate) fn envs(&self) -> Envs {
        let salt = &self.world.salt;
        let mut envs = Envs::new()
            .with_default_bucket_capacity(salt.default_multiplier as u64 * MIN_BUCKET_SIZE as u64);
        for (who, m) in &salt.crowded_accounts {
            envs = envs.with_bucket_capacity(
                Envs::bucket_id_for_account(who.address()),
                *m as u64 * MIN_BUCKET_SIZE as u64,
            );
        }
        for (who, slot, m) in &salt.crowded_slots {
            envs = envs.with_bucket_capacity(
                Envs::bucket_id_for_slot(who.address(), U256::from(*slot)),
                *m as u64 * MIN_BUCKET_SIZE as u64,
            );
        }
        if let Some(who) = salt.failing_account {
            envs = envs.with_failing_bucket(
                Envs::bucket_id_for_account(who.address()),
                "the bucket cannot be read".to_string(),
            );
        }
        for (slot, value) in &self.world.oracle {
            envs = envs.with_oracle_storage(U256::from(*slot), value.u256());
        }
        envs
    }

    /// The block.
    pub(crate) fn block(&self) -> BlockEnv {
        let shape = self.world.block;
        BlockEnv {
            number: U256::from(300),
            beneficiary: self.world.beneficiary_address(),
            timestamp: U256::from(1_700_000_000u64),
            gas_limit: 10_000_000_000,
            basefee: shape.basefee,
            prevrandao: Some(B256::repeat_byte(7)),
            blob_excess_gas_and_price: shape
                .blob
                .then_some(BlobExcessGasAndPrice { excess_blob_gas: 0, blob_gasprice: 3 }),
            slot_num: shape.slot_num,
            ..Default::default()
        }
    }

    /// The configuration: the spec's, on the chain every case runs on.
    pub(crate) fn cfg(&self) -> CfgEnv<mega_evm::MegaSpecId> {
        let mut cfg = CfgEnv::new_with_spec(mega_evm::MegaSpecId::SATIN);
        cfg.chain_id = CHAIN_ID;
        cfg
    }

    /// The transaction.
    pub(crate) fn transaction(&self) -> MegaTransaction {
        self.tx.build(&self.world, &self.main.assemble())
    }

    /// A Satin context over `db` with the case's environments, block, chain and limits.
    pub(crate) fn context(&self, db: MemoryDatabase) -> MegaContext<MemoryDatabase, Envs> {
        let envs = self.envs();
        MegaContext::new_with_external_envs(
            db,
            mega_evm::MegaSpecId::SATIN,
            ExternalEnvs { salt_env: envs.clone(), oracle_env: envs },
        )
        .with_cfg(self.cfg())
        .with_block(self.block())
        .with_chain(zero_fee_l1_block_info())
        .with_tx_runtime_limits(self.world.limits.runtime())
    }

    /// A fresh EVM over the case's pre-state.
    pub(crate) fn evm(&self) -> MegaEvm<MemoryDatabase, NoOpInspector, Envs> {
        MegaEvm::new(self.context(self.database()))
    }

    /// Runs the transaction on a fresh EVM, without committing.
    pub(crate) fn execute(&self) -> Execution {
        Self::execute_on(&mut self.evm(), self.transaction())
    }

    /// Runs the transaction on a fresh EVM and hands the EVM back, for what its context and
    /// environments recorded.
    pub(crate) fn execute_with_evm(
        &self,
    ) -> (Execution, MegaEvm<MemoryDatabase, NoOpInspector, Envs>) {
        let mut evm = self.evm();
        let execution = Self::execute_on(&mut evm, self.transaction());
        (execution, evm)
    }

    /// Runs the transaction on a fresh EVM under `inspector`, and hands the EVM back with what
    /// the inspector recorded.
    pub(crate) fn execute_inspected<I>(
        &self,
        inspector: I,
    ) -> (Execution, MegaEvm<MemoryDatabase, I, Envs>)
    where
        I: Inspector<MegaContext<MemoryDatabase, Envs>, EthInterpreter>,
    {
        let mut evm = self.evm().with_inspector(inspector);
        let execution = Self::execute_on(&mut evm, self.transaction());
        (execution, evm)
    }

    fn execute_on<I>(evm: &mut MegaEvm<MemoryDatabase, I, Envs>, tx: MegaTransaction) -> Execution
    where
        I: Inspector<MegaContext<MemoryDatabase, Envs>, EthInterpreter>,
    {
        evm.execute_transaction(tx).map_err(|error| format!("{error:?}"))
    }

    /// The case with no self-destruction left inside init code: the init code of a creation
    /// transaction, of a keyless deployment and of every creation the programs make can no longer
    /// destroy the account it is creating, in its own code or in code it borrows through
    /// `CALLCODE` or `DELEGATECALL` (see [`Program::as_init_without_destruction`]). A contract
    /// that existed before the transaction still destroys itself.
    ///
    /// For the comparison with Ethereum on Amsterdam, where a contract destroyed in the
    /// transaction that created it is settled by EIP-8246, which Satin's Osaka base does not have.
    pub(crate) fn without_destruction_in_creation(&self) -> Self {
        let is_creation =
            matches!(self.tx.shape, Shape::Create | Shape::Deposit { create: true, .. });
        let main = self.main.clone();
        let mut tx = self.tx.clone();
        if let Shape::Keyless { init, .. } = &mut tx.shape {
            *init = init.clone().without_destruction();
        }
        Self {
            world: self.world.clone(),
            tx,
            main: if is_creation {
                main.as_init_without_destruction()
            } else {
                main.without_destruction_in_creations()
            },
            a: self.a.clone().without_destruction_in_creations(),
            b: self.b.clone().without_destruction_in_creations(),
        }
    }

    /// Whether any program of the case, or any init code in them, destroys an account.
    pub(crate) fn destroys(&self) -> bool {
        self.main.destroys() ||
            self.a.destroys() ||
            self.b.destroys() ||
            matches!(&self.tx.shape, Shape::Keyless { init, .. } if init_destroys(init))
    }
}

fn init_destroys(init: &super::program::InitCode) -> bool {
    match init {
        super::program::InitCode::Selfdestructs { .. } => true,
        super::program::InitCode::Runs(program) => program.destroys(),
        _ => false,
    }
}

/// Installs `account` at `address`, with `code` when it has any.
fn install(
    db: &mut MemoryDatabase,
    address: Address,
    account: &AccountShape,
    code: Option<Vec<u8>>,
) {
    if let Some(code) = code.filter(|code| !code.is_empty()) {
        db.set_account_code(address, Bytes::from(code));
    }
    db.set_account_balance(address, account.balance.wei());
    db.set_account_nonce(address, account.nonce as u64);
    for (slot, value) in &account.slots {
        db.set_account_storage(address, U256::from(*slot), value.u256());
    }
}

/// Reads a balance out of the cache database without loading it as a read.
trait BalanceRef {
    fn basic_ref_balance(&mut self, address: Address) -> U256;
}

impl BalanceRef for MemoryDatabase {
    fn basic_ref_balance(&mut self, address: Address) -> U256 {
        revm::Database::basic(self, address).unwrap_or_default().map_or(U256::ZERO, |a| a.balance)
    }
}
