//! What a transaction pool can decide about a transaction before it reads any state.
//!
//! [`validate_transaction_stateless`] runs the EVM's own validation phases on an EVM over an empty
//! database, so a pool admits and refuses by the rules block execution applies, and reads the
//! intrinsic gas the EVM charges ([`IntrinsicGas`]) rather than recomputing it.

use core::convert::Infallible;

use alloy_primitives::Address;
use revm::{
    context::{BlockEnv, CfgEnv},
    database::EmptyDB,
    handler::{EthFrame, Handler},
    interpreter::interpreter::EthInterpreter,
};

use crate::{
    system::is_mega_system_transaction_with, MegaContext, MegaEvm, MegaEvmError, MegaHandler,
    MegaSpecId, MegaTransaction,
};

/// The gas a transaction's gas limit must cover before it runs, by ledger, as the EVM charges it
/// when it validates the transaction.
///
/// It is fixed by the transaction's own fields. What the state decides — a recipient or a created
/// account that EIP-2780 finds new, an EIP-7702 authority that applies — is charged when the
/// transaction runs, and a gas limit that cannot pay it runs out of gas rather than being refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntrinsicGas {
    /// Regular gas: EIP-2780's base cost with its recipient and value charges, the calldata, the
    /// access list, the regular part of each EIP-7702 authorization and a creation's init-code
    /// words.
    pub regular: u64,
    /// EIP-8037 state gas. Under EIP-2780 it is zero: the state a transaction adds is charged when
    /// it runs, once the state says what it adds.
    pub state: u64,
    /// The history gas of the transaction's body: its envelope, the five write records every
    /// transaction makes, its calldata, access list and authorizations, at the cost per history
    /// byte. Zero for a transaction exempt from history gas: a deposit and a system-address
    /// transaction.
    pub history: u64,
    /// The EIP-7623 calldata floor. The gas limit must cover it, and a transaction pays at least
    /// it. At the spec's byte prices a transaction that pays history gas pays more than the floor
    /// for the same bytes, so the floor binds only one exempt from history; the byte prices are
    /// provisional.
    pub floor: u64,
}

impl IntrinsicGas {
    /// What the gas limit must cover before the transaction runs: the regular, state and history
    /// parts together.
    pub const fn total(&self) -> u64 {
        self.regular.saturating_add(self.state).saturating_add(self.history)
    }

    /// The smallest gas limit that covers both [`total`](Self::total) and the floor.
    ///
    /// No gas limit below it is valid. One at or above it can still be refused by the other
    /// rules: the block's gas limit, and, for a gas limit above the execution cap, a regular part
    /// or a floor that the cap does not cover.
    pub fn min_gas_limit(&self) -> u64 {
        self.total().max(self.floor)
    }
}

/// Validates `tx` as the EVM does before it reads any state, and returns the intrinsic gas the
/// EVM charges it.
///
/// It runs the Satin handler's own validation phases — `validate_env` and
/// `validate_initial_tx_gas` — on an EVM over an empty database, configured with
/// [`spec_cfg`](crate::spec_cfg) of `cfg` and running in `block`. What they check:
///
/// - the block environment: a prevrandao and a blob excess gas are set;
/// - the transaction against the configuration and the block: its chain id, its type and its fees
///   against the block's base fee, a non-empty authorization list, its gas limit against the
///   block's, the init-code size of a creation, a nonce that can still be incremented;
/// - the intrinsic gas: the gas limit covers [`IntrinsicGas::total`] and the floor, and a gas limit
///   above the execution cap leaves the regular part and the floor within the cap.
///
/// A deposit is validated as op-revm validates one: on its intrinsic gas, and on not being flagged
/// as an L1 system transaction. A system-address transaction has its chain id checked and is
/// promoted to a deposit. Neither pays history gas.
///
/// What is left to the state is left out: the sender's nonce, balance and code (EIP-3607), the
/// system address's nonce and code for a system-address transaction, and the charges the state
/// decides (see [`IntrinsicGas`]). A body over the transaction's data-size limit is not refused
/// either: the limit stops such a transaction when it runs.
///
/// `system_address` is the live system address: the one the `SequencerRegistry` names in the
/// state the pool validates against. A transaction of the system shape from it is a
/// system-address transaction; with `None`, no transaction is.
///
/// # Errors
///
/// The error the EVM reports for the same transaction in the same block: an invalid transaction,
/// or a block environment the spec cannot run in.
///
/// The EVM does not refuse a deposit for any of them, nor a system-address transaction for one
/// found once it is promoted: op-revm includes a deposit that fails validation as a failed
/// deposit, which bumps the sender's nonce, keeps the mint and uses the whole gas limit. For
/// those the error says the transaction would fail, not that it would be refused.
pub fn validate_transaction_stateless(
    cfg: CfgEnv<MegaSpecId>,
    block: BlockEnv,
    tx: MegaTransaction,
    system_address: Option<Address>,
) -> Result<IntrinsicGas, MegaEvmError<Infallible>> {
    let system_transaction =
        system_address.is_some_and(|address| is_mega_system_transaction_with(&tx, address));
    let ctx =
        MegaContext::new(EmptyDB::default(), cfg.spec).with_cfg(cfg).with_block(block).with_tx(tx);
    let mut evm = MegaEvm::new(ctx);
    let handler = MegaHandler::<_, MegaEvmError<Infallible>, EthFrame<EthInterpreter>>::new();
    handler.validate_env_as(&mut evm, system_transaction, false)?;
    let gas = handler.validate_initial_tx_gas(&mut evm)?;
    let history = evm.ctx().additional_limit.intrinsic_history_gas();
    Ok(IntrinsicGas {
        regular: gas.initial_regular_gas(),
        state: gas.initial_state_gas_final() - history,
        history,
        floor: gas.floor_gas(),
    })
}
