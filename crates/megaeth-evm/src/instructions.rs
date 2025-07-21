use crate::{constants, HostExt, MegaethContext, MegaethSpecId};
use alloy_evm::Database;
use alloy_primitives::{Bytes, Log, LogData, B256};
use revm::{
    bytecode::opcode::{LOG0, LOG1, LOG2, LOG3, LOG4},
    handler::instructions::{EthInstructions, InstructionProvider},
    interpreter::{
        as_usize_or_fail,
        gas::{LOG, LOGDATA},
        gas_or_fail,
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, LoopControl, MemoryTr, RuntimeFlag, StackTr},
        popn, require_non_staticcall, resize_memory, tri, InstructionResult, InstructionTable,
        Interpreter, InterpreterTypes,
    },
};

/// `MegaethInstructions` is the instruction table for `MegaETH`.
#[derive(Clone)]
pub struct MegaethInstructions<DB: Database> {
    spec: MegaethSpecId,
    inner: EthInstructions<EthInterpreter, MegaethContext<DB>>,
}

impl<DB: Database> std::fmt::Debug for MegaethInstructions<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MegaethInstructions").field("spec", &self.spec).finish_non_exhaustive()
    }
}

impl<DB: Database> MegaethInstructions<DB> {
    /// Create a new `MegaethInstructions` with the given spec id.
    pub fn new(spec: MegaethSpecId) -> Self {
        let this = Self { spec, inner: EthInstructions::new_mainnet() };
        this.with_spec(spec)
    }

    fn with_spec(mut self, spec: MegaethSpecId) -> Self {
        if spec.is_enabled_in(MegaethSpecId::MINI_RAX) {
            // Override the LOG instructions and use our own implementation with quadratic data cost
            self.inner.insert_instruction(LOG0, log_with_quadratic_data_cost::<0, _>);
            self.inner.insert_instruction(LOG1, log_with_quadratic_data_cost::<1, _>);
            self.inner.insert_instruction(LOG2, log_with_quadratic_data_cost::<2, _>);
            self.inner.insert_instruction(LOG3, log_with_quadratic_data_cost::<3, _>);
            self.inner.insert_instruction(LOG4, log_with_quadratic_data_cost::<4, _>);
        }
        self
    }
}

impl<DB: Database> InstructionProvider for MegaethInstructions<DB> {
    type Context = MegaethContext<DB>;
    type InterpreterTypes = EthInterpreter;

    fn instruction_table(&self) -> &InstructionTable<Self::InterpreterTypes, Self::Context> {
        self.inner.instruction_table()
    }
}

/// `LOG` opcode implementation modified from `revm` to support quadratic data cost.
pub fn log_with_quadratic_data_cost<const N: usize, H: HostExt + ?Sized>(
    interpreter: &mut Interpreter<impl InterpreterTypes>,
    host: &mut H,
) {
    require_non_staticcall!(interpreter);

    popn!([offset, len], interpreter);
    let len = as_usize_or_fail!(interpreter, len);
    let previous_total_log_data_size = host.log_data_size();
    gas_or_fail!(
        interpreter,
        quadratic_log_cost(N as u8, len as u64, previous_total_log_data_size)
    );
    let data = if len == 0 {
        Bytes::new()
    } else {
        let offset = as_usize_or_fail!(interpreter, offset);
        resize_memory!(interpreter, offset, len);
        Bytes::copy_from_slice(interpreter.memory.slice_len(offset, len).as_ref())
    };
    if interpreter.stack.len() < N {
        interpreter.control.set_instruction_result(InstructionResult::StackUnderflow);
        return;
    }
    let Some(topics) = interpreter.stack.popn::<N>() else {
        interpreter.control.set_instruction_result(InstructionResult::StackUnderflow);
        return;
    };

    let log = Log {
        address: interpreter.input.target_address(),
        data: LogData::new(topics.into_iter().map(B256::from).collect(), data)
            .expect("LogData should have <=4 topics"),
    };

    host.log(log);
}

/// `LOG` opcode cost calculation.
///
/// # Parameters
///
/// - `n`: Number of topics
/// - `len`: Length of the data of current opcode
/// - `previous_total_data_size`: Total size of all previous log data, excluding current opcode
#[inline]
#[allow(unused_variables)]
pub const fn quadratic_log_cost(n: u8, len: u64, previous_total_data_size: u64) -> Option<u64> {
    // cost for opcode and topics
    let base_cost = tri!(LOG.checked_add(constants::mini_rax::LOG_TOPIC_COST * n as u64));

    let data_cost = {
        // cost for log data
        // the total cost for the whole transaction (summing up all the logs) would be:
        // - data_len * LOGDATA, if data_len <= 4096
        // - 4096 * LOGDATA + (data_len - 4096) ^ 2, if data_len > 4096
        // Here we calculate the cost for the current log.
        let total_data_size = previous_total_data_size + len;
        if total_data_size <= 4096 {
            // Less than 4KB, linear cost. The cost of current log is linear to its length.
            tri!(LOGDATA.checked_mul(len))
        } else if previous_total_data_size <= 4096 {
            // The previous total log length is less than 4KB, but the current log length is greater
            // than 4KB. The cost of current log is a combination of linear and
            // quadratic cost: (4096 - previous_total_data_size) * LOGDATA + (len -
            // (4096 - previous_total_data_size)) ^ 2
            let linear_cost_len = 4096 - previous_total_data_size;
            let linear_cost = tri!(LOGDATA.checked_mul(linear_cost_len));
            let quadratic_cost_len = len - linear_cost_len;
            let quadratic_cost = tri!(quadratic_cost_len.checked_pow(2));
            tri!(linear_cost.checked_add(quadratic_cost))
        } else {
            // The previous total log length is greater than 4KB, and the current log length is also
            // greater than 4KB. The cost of current log is quadratic to its length:
            // total_data_size ** 2 - previous_total_data_size ** 2 === (total_data_size +
            // previous_total_data_size) * len
            tri!(tri!(total_data_size.checked_add(previous_total_data_size)).checked_mul(len))
        }
    };

    base_cost.checked_add(data_cost)
}
