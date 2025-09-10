use revm::{
    bytecode::opcode::OpCode,
    context::ContextTr,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome, Interpreter,
    },
    Database, Inspector,
};

#[derive(Debug, Default)]
pub struct TraceInspector {}

impl<CTX: ContextTr> Inspector<CTX> for TraceInspector {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        println!("call: {:?}", inputs);
        let target = inputs.target_address;
        let target = context.db_mut().basic(target).unwrap();
        println!("target: {:?}", target);
        None
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut CTX) {
        let opcode = OpCode::new(interp.bytecode.opcode());
        println!("step: {:?}", opcode.unwrap().as_str());
    }
}
