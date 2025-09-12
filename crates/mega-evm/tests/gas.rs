// TODO: tests for doubled gas cost for sstore and call when bucket capacity doubles
// TODO: test per-byte gas cost for created bytecode.
// TODO: test additional gas cost per-byte for calldata.
// TODO: test increased log topic and data gas cost.
// TODO: test additional gas cost for code change.
// TODO: test 31/32 rule

use alloy_primitives::U256;
use mega_evm::test_utils::{opcode_gen::BytecodeBuilder, MemoryDatabase};
use revm::{
    context::ContextTr,
    interpreter::{interpreter::EthInterpreter, Interpreter},
    Inspector,
};

struct StepInspector<CTX: ContextTr> {
    inspector: Box<dyn Fn(&mut Interpreter<EthInterpreter>, &mut CTX) + Send + Sync>,
}

impl<CTX: ContextTr> Inspector<CTX> for StepInspector<CTX> {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut CTX) {
        (self.inspector)(interp, context);
    }
}

#[test]
fn test_sstore_gas_cost_doubled_by_bucket_capacity() {}
