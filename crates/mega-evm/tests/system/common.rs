//! Shared setup of the system contract tests.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE},
        ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, SEQUENCER_REGISTRY_ADDRESS,
        SEQUENCER_REGISTRY_CODE,
    },
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionOutcome,
    VolatileDataAccess,
};
use revm::{
    bytecode::opcode::{ADD, MSTORE, RETURN, RETURNDATACOPY, RETURNDATASIZE},
    context::{result::ResultAndState, BlockEnv, TxEnv},
    interpreter::{interpreter::EthInterpreter, CallInputs, CallOutcome, Interpreter},
    Database, Inspector,
};

/// The sender of every transaction the tests run.
pub(crate) const CALLER: Address = address!("0x0000000000000000000000000000000000300000");

/// The contract the tests deploy when a call has to come from code.
pub(crate) const CONTRACT: Address = address!("0x0000000000000000000000000000000000300001");

/// A gas limit below the execution cap, so a transaction has no state-gas reservoir.
pub(crate) const GAS_LIMIT: u64 = 100_000_000;

/// A database holding the code of every system contract, as the chain holds it once the fork
/// that deploys them has activated.
pub(crate) fn system_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64))
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .account_code(HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, HIGH_PRECISION_TIMESTAMP_ORACLE_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE)
}

/// A database as [`system_db`], with `code` deployed at [`CONTRACT`] and a balance to send
/// from, so a test that makes a value-bearing call fails on the policy and not on the funds.
pub(crate) fn with_contract(code: Bytes) -> MemoryDatabase {
    system_db().account_code(CONTRACT, code).account_balance(CONTRACT, U256::from(1_000_000))
}

/// A block with room for any transaction the tests run.
pub(crate) fn block() -> BlockEnv {
    BlockEnv { number: U256::from(1), gas_limit: 10_000_000_000, ..Default::default() }
}

/// A Satin context over `db` with zero L1 fees.
pub(crate) fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

/// A transaction from [`CALLER`] calling `to` with `data` and `value`.
pub(crate) fn call_tx(to: Address, data: impl AsRef<[u8]>, value: U256) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data: Bytes::copy_from_slice(data.as_ref()),
        value,
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }))
}

/// [`call_tx`] carrying `gas_limit`.
pub(crate) fn call_tx_with_gas(
    to: Address,
    data: impl AsRef<[u8]>,
    value: U256,
    gas_limit: u64,
) -> MegaTransaction {
    let mut tx = call_tx(to, data, value);
    tx.0.base.gas_limit = gas_limit;
    tx
}

/// A `keylessDeploy` transaction carrying `data`, from [`CALLER`], at `gas_limit`.
pub(crate) fn keyless_tx(data: impl AsRef<[u8]>, gas_limit: u64) -> MegaTransaction {
    call_tx_with_gas(KEYLESS_DEPLOY_ADDRESS, data, U256::ZERO, gas_limit)
}

/// Runs `tx` on a fresh Satin EVM over `db`, without committing.
pub(crate) fn run<DB: alloy_evm::Database>(
    db: DB,
    tx: MegaTransaction,
) -> ResultAndState<MegaHaltReason> {
    MegaEvm::new(context(db)).transact_raw(tx).expect("the transaction is valid")
}

/// Code that calls `target` with `data` through the `scheme` opcode, forwarding all the gas it
/// has, and returns the call's status followed by the callee's return data.
///
/// `value` is passed by the two schemes that carry one (`CALL`, `CALLCODE`) and ignored by the
/// two that do not.
pub(crate) fn calls_with(scheme: u8, target: Address, data: &[u8], value: u64) -> Bytes {
    use revm::bytecode::opcode::{CALL, CALLCODE, GAS};

    let mut code = BytecodeBuilder::default().mstore(0x0, data);
    code = code
        .push_number(0_u64) // retSize: the return data is read with RETURNDATACOPY
        .push_number(0_u64) // retOffset
        .push_number(data.len() as u64) // argsSize
        .push_number(0_u64); // argsOffset
    if scheme == CALL || scheme == CALLCODE {
        code = code.push_number(value);
    }
    code.push_address(target)
        .append(GAS)
        .append(scheme)
        // memory[0..32] = the call's status
        .push_number(0_u64)
        .append(MSTORE)
        // memory[32..32 + returndatasize] = the callee's return data
        .append(RETURNDATASIZE)
        .push_number(0_u64)
        .push_number(32_u64)
        .append(RETURNDATACOPY)
        // return both
        .append(RETURNDATASIZE)
        .push_number(32_u64)
        .append(ADD)
        .push_number(0_u64)
        .append(RETURN)
        .build()
}

/// Appends a `CALL` of `selector` on the access control contract, discarding its status.
pub(crate) fn steer(code: BytecodeBuilder, selector: [u8; 4]) -> BytecodeBuilder {
    use revm::bytecode::opcode::{CALL, POP};
    code.mstore(0x0, selector)
        .push_number(0_u8) // retSize
        .push_number(0_u8) // retOffset
        .push_number(4_u8) // argsSize
        .push_number(0_u8) // argsOffset
        .push_number(0_u8) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u32)
        .append_many([CALL, POP])
}

/// A call from [`CONTRACT`] to `target` through `scheme`, run as a transaction.
pub(crate) fn through_contract(
    target: Address,
    scheme: u8,
    data: &[u8],
    value: u64,
) -> (bool, Bytes) {
    let code = calls_with(scheme, target, data, value);
    let result = run(with_contract(code), call_tx(CONTRACT, [], U256::ZERO));
    let outcome = output(&result);
    let (status, data) = split_outcome(&outcome);
    (status, Bytes::copy_from_slice(data))
}

/// Splits what [`calls_with`] returns into the inner call's status and its return data.
pub(crate) fn split_outcome(output: &[u8]) -> (bool, &[u8]) {
    let (status, data) = output.split_at(32);
    (U256::from_be_slice(status) == U256::ONE, data)
}

/// The output of a successful transaction.
pub(crate) fn output(result: &ResultAndState<MegaHaltReason>) -> Bytes {
    assert!(result.result.is_success(), "the transaction failed: {:?}", result.result);
    result.result.output().cloned().unwrap_or_default()
}

/// The revert data of a transaction that reverted.
pub(crate) fn revert_data(result: &ResultAndState<MegaHaltReason>) -> Bytes {
    match &result.result {
        revm::context::result::ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("the transaction did not revert: {other:?}"),
    }
}

/// Whether a state byte costs nothing at the prices in effect: every state-gas entry of the
/// schedule is zero.
///
/// Only a measurement build arranges that, with `MEGA_SATIN_CPSB` at 0 or at a price every entry
/// rounds to nothing. A test whose scenario is state gas — a limit to cross with it, a bucket to
/// scale it, a charge that cannot be paid — has nothing to run then, and returns early, leaving a
/// note (`note_price_guard`), which the byte-price grid counts.
pub(crate) fn state_is_free() -> bool {
    let params = mega_evm::satin_gas_params();
    if !mega_evm::STATE_GAS_REPRICED.iter().all(|&(id, _)| params.get(id()) == 0) {
        return false;
    }
    mega_evm::test_utils::note_price_guard("MEGA_SATIN_CPSB prices a state byte at nothing");
    true
}

/// The state gas one new account costs at the byte prices in effect, in the minimum bucket.
pub(crate) fn account_state_gas() -> u64 {
    mega_evm::satin_gas_params().get(revm::context_interface::cfg::GasId::new_account_state_gas())
}

/// The history gas `bytes` bytes cost at the byte prices in effect, by hand rather than through
/// the engine's pricing: the bytes times `COST_PER_HISTORY_BYTE`, 88 gas, at the spec's price.
///
/// A measurement build may price a history byte with a fraction of a gas, kept in thousandths of
/// a gas; the charge for the bytes is then rounded to the nearest gas, halves up.
pub(crate) fn history(bytes: u64) -> u64 {
    use mega_evm::constants::COST_PER_HISTORY_BYTE;
    let milli_gas = mega_evm::active_satin_prices().cphb.milli_gas();
    if milli_gas == COST_PER_HISTORY_BYTE * 1_000 {
        return bytes * COST_PER_HISTORY_BYTE;
    }
    let milli_gas = u128::from(bytes) * u128::from(milli_gas);
    u64::try_from((milli_gas + 500) / 1_000).expect("the history of the bytes fits a u64")
}

/// The history gas the body of a transaction carrying `calldata_len` bytes of calldata, and no
/// access list or authorization, pays at the byte prices in effect, by hand: `TX_BODY_SIZE`, 310
/// bytes, and one byte per byte of calldata.
pub(crate) fn body_history(calldata_len: u64) -> u64 {
    history(mega_evm::TX_BODY_SIZE + calldata_len)
}

/// What a transaction did, and what gas detention made of its reads.
pub(crate) struct Run {
    pub(crate) outcome: MegaTransactionOutcome,
    pub(crate) accessed: VolatileDataAccess,
    pub(crate) limit: Option<u64>,
}

impl Run {
    /// The output of the transaction, which must have succeeded.
    pub(crate) fn output(&self) -> Bytes {
        assert!(self.outcome.result.is_success(), "{:?}", self.outcome.result);
        self.outcome.result.output().cloned().unwrap_or_default()
    }

    /// The output as a word.
    pub(crate) fn word(&self) -> U256 {
        U256::from_be_slice(&self.output())
    }

    /// The call status the log at `index` carries.
    pub(crate) fn logged_status(&self, index: usize) -> bool {
        let logs = self.outcome.result.logs();
        assert!(logs.len() > index, "{} logs, wanted {}", logs.len(), index + 1);
        U256::from_be_slice(&logs[index].data.data) == U256::ONE
    }
}

/// Records every answer `remainingComputeGas()` returns, next to the spendable regular gas its
/// caller resumes with.
#[derive(Debug, Default)]
pub(crate) struct AnswerAndResume {
    pub(crate) pending: Option<u64>,
    pub(crate) pairs: Vec<(u64, u64)>,
}

impl<CTX> Inspector<CTX, EthInterpreter> for AnswerAndResume {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if let Some(answer) = self.pending.take() {
            self.pairs.push((answer, interp.gas.tracker().spendable()));
        }
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address == LIMIT_CONTROL_ADDRESS && outcome.result.is_ok() {
            self.pending = Some(U256::from_be_slice(&outcome.result.output).to::<u64>());
        }
    }
}
