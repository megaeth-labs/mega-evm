# Encapsulated EVM for MegaETH based on revm

This repo contains the customized version of revm tailored for MegaETH specifications.

## Architecture

This repo contains a specialized `revm::context::Evm`, tailored for MegaETH.
The specialization comes from the following aspects:
- Revm handler: in [`mega_evm::Handler`](crates/mega-evm/src/handler.rs), EVM handler is inherited from `op_revm::OpHandler` by directly wrapping it with some modifications.
- Instruction table: The default `revm::handler::instructions::EthInstructions` are customized by replacing the logic of some instructions.
- Revm context and host: in [`mega_evm::Context`](crates/mega-evm/src/context.rs), EVM context is inherited from `op_revm::OpContext` by directly wrapping it with some modifications.
- Revm spec id: in [`mega_evm::Spec`](crates/mega-evm/src/spec.rs), a new set of EVM spec is defined for MegaETH and mapped to optimism and mainnet EVM specs.
