//! Gas Limit Enforcement Inspector
//!
//! This inspector detects beneficiary access and enforces gas limits during execution.

use crate::{constants, Context};
use alloy_evm::Database;
use delegate::delegate;
use revm::{
    inspector::Inspector,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult, Interpreter,
    },
};

/// Inspector that detects beneficiary access and enforces gas limits
#[derive(Debug, Clone)]
pub struct GasLimitEnforcementInspector<I>(pub I);

impl<DB: Database, I: Inspector<Context<DB>>> Inspector<Context<DB>>
    for GasLimitEnforcementInspector<I>
{
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut Context<DB>) {
        // Execute instruction
        self.0.step(interp, context);

        // Enforce gas limit if beneficiary was accessed
        if context.has_accessed_beneficiary() {
            let current_spent = interp.gas.spent();
            if current_spent >= constants::mini_rex::BENEFICIARY_GAS_LIMIT {
                interp.gas.set_spent(constants::mini_rex::BENEFICIARY_GAS_LIMIT);
                interp.halt(InstructionResult::OutOfGas);
            }
        }
    }

    // Delegate all other methods to inner inspector
    delegate! {
        to self.0 {
            fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut Context<DB>);
            fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut Context<DB>);
            fn call(&mut self, context: &mut Context<DB>, inputs: &mut CallInputs) -> Option<CallOutcome>;
            fn call_end(&mut self, context: &mut Context<DB>, inputs: &CallInputs, outcome: &mut CallOutcome);
            fn create(&mut self, context: &mut Context<DB>, inputs: &mut CreateInputs) -> Option<CreateOutcome>;
            fn create_end(&mut self, context: &mut Context<DB>, inputs: &CreateInputs, outcome: &mut CreateOutcome);
        }
    }
}
