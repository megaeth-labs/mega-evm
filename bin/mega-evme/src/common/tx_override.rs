//! Transaction override support for mega-evme replay command.
//!
//! This module provides the ability to override transaction fields when replaying
//! transactions from RPC.

use std::cell::RefCell;

use alloy_primitives::{Address, Bytes, U256};
use clap::Args;
use mega_evm::{
    alloy_evm::{IntoTxEnv, RecoveredTx},
    MegaTransaction,
};

use super::{load_hex, Result};

// Thread-local storage for input override (Bytes is not Copy, so we can't store it in TxOverrides)
thread_local! {
    static INPUT_OVERRIDE: RefCell<Option<Bytes>> = const { RefCell::new(None) };
}

/// Transaction override arguments for the replay command.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Transaction Override Options")]
pub struct TxOverrideArgs {
    /// Override transaction gas limit
    #[arg(long = "override.gas-limit", visible_aliases = ["override.gaslimit"], value_name = "GAS")]
    pub gas_limit: Option<u64>,

    /// Override transaction value (in wei)
    #[arg(long = "override.value", value_name = "WEI")]
    pub value: Option<U256>,

    /// Override transaction input data (hex string)
    #[arg(long = "override.input", visible_aliases = ["override.data"], value_name = "HEX")]
    pub input: Option<String>,

    /// Override transaction input data from file (hex content)
    #[arg(long = "override.input-file", visible_aliases = ["override.data-file"], value_name = "FILE")]
    pub input_file: Option<String>,
}

impl TxOverrideArgs {
    /// Returns true if any override is set.
    pub fn has_overrides(&self) -> bool {
        self.gas_limit.is_some() ||
            self.value.is_some() ||
            self.input.is_some() ||
            self.input_file.is_some()
    }

    /// Wraps a transaction with overrides.
    pub fn wrap<T: Copy>(&self, tx: T) -> Result<OverriddenTx<T>> {
        // Parse and store input override in thread-local if present
        let has_input_override =
            if let Some(bytes) = load_hex(self.input.clone(), self.input_file.clone())? {
                INPUT_OVERRIDE.with(|cell| cell.borrow_mut().replace(bytes));
                true
            } else {
                INPUT_OVERRIDE.with(|cell| cell.borrow_mut().take());
                false
            };

        Ok(OverriddenTx {
            inner: tx,
            overrides: TxOverrides {
                gas_limit: self.gas_limit,
                value: self.value,
                has_input_override,
            },
        })
    }
}

/// Parsed transaction overrides.
///
/// All fields must be `Copy` because `OverriddenTx<T>` must implement `Copy`
/// (required by block executor's `run_transaction`). The input override is stored
/// in a thread-local (`INPUT_OVERRIDE`) since `Bytes` is not `Copy`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TxOverrides {
    /// Override for gas limit.
    pub gas_limit: Option<u64>,
    /// Override for value.
    pub value: Option<U256>,
    /// Whether input data should be overridden (actual data in thread-local).
    pub has_input_override: bool,
}

impl TxOverrides {
    /// Apply overrides to a [`MegaTransaction`].
    pub fn apply(&self, tx: &mut MegaTransaction) {
        if let Some(gas_limit) = self.gas_limit {
            tx.base.gas_limit = gas_limit;
        }
        if let Some(value) = self.value {
            tx.base.value = value;
        }
        if self.has_input_override {
            if let Some(input) = INPUT_OVERRIDE.with(|cell| cell.borrow().clone()) {
                tx.base.data = input;
            }
        }
    }
}

/// A wrapper that applies overrides when converting to `TxEnv`.
///
/// This wrapper implements all the required traits by delegating to the inner
/// transaction, but intercepts `IntoTxEnv` to apply overrides.
#[derive(Debug, Clone, Copy)]
pub struct OverriddenTx<T: Copy> {
    inner: T,
    overrides: TxOverrides,
}

impl<T: Copy> OverriddenTx<T> {
    /// Get a reference to the inner transaction.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

// Implement IntoTxEnv - this is where we apply the overrides
impl<T: IntoTxEnv<MegaTransaction> + Copy> IntoTxEnv<MegaTransaction> for OverriddenTx<T> {
    fn into_tx_env(self) -> MegaTransaction {
        let mut tx = self.inner.into_tx_env();
        self.overrides.apply(&mut tx);
        tx
    }
}

// Delegate RecoveredTx to inner
impl<Tx, T: RecoveredTx<Tx> + Copy> RecoveredTx<Tx> for OverriddenTx<T> {
    fn tx(&self) -> &Tx {
        self.inner.tx()
    }

    fn signer(&self) -> &Address {
        self.inner.signer()
    }
}
