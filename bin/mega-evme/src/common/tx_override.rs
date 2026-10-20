//! Transaction override support for mega-evme replay command.
//!
//! This module provides the ability to override transaction fields when replaying
//! transactions from RPC.

use alloy_primitives::{Bytes, TxHash, U256};
use clap::Args;
use mega_evm::{alloy_evm::block::ExecutableTxParts, MegaTransaction, MegaTransactionExt};

use super::{load_hex, parse_ether_value, Result};

/// Transaction override arguments for the replay command.
#[derive(Args, Debug, Clone, Default)]
#[command(next_help_heading = "Transaction Override Options")]
pub struct TxOverrideArgs {
    /// Override transaction gas limit
    #[arg(long = "override.gas-limit", visible_aliases = ["override.gaslimit"], value_name = "GAS")]
    pub gas_limit: Option<u64>,

    /// Override transaction value.
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    /// Examples: `--override.value 1ether`, `--override.value 100gwei`
    #[arg(long = "override.value", value_name = "VALUE")]
    pub value: Option<String>,

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
    pub fn wrap<T>(&self, tx: T) -> Result<OverriddenTx<T>> {
        let input = load_hex(self.input.clone(), self.input_file.clone())?;
        Ok(OverriddenTx {
            inner: tx,
            overrides: TxOverrides {
                gas_limit: self.gas_limit,
                value: self.value.as_deref().map(parse_ether_value).transpose()?,
                input,
            },
        })
    }
}

/// Parsed transaction overrides.
///
/// The input bytes live on this value. Two overrides on one thread keep their own calldata.
#[derive(Debug, Clone, Default)]
pub struct TxOverrides {
    /// Override for gas limit.
    pub gas_limit: Option<u64>,
    /// Override for value.
    pub value: Option<U256>,
    /// Override for input data.
    pub input: Option<Bytes>,
}

impl TxOverrides {
    /// Apply overrides to a [`MegaTransaction`].
    pub fn apply(self, tx: &mut MegaTransaction) {
        if let Some(gas_limit) = self.gas_limit {
            tx.base.gas_limit = gas_limit;
        }
        if let Some(value) = self.value {
            tx.base.value = value;
        }
        if let Some(input) = self.input {
            tx.base.data = input;
        }
    }
}

/// A wrapper that applies overrides to the transaction environment the block executor builds.
///
/// Only the environment changes: the recovered transaction, its encoding and so its hash and
/// sizes stay the original's, which is what the executor charges the data-availability size of.
#[derive(Debug, Clone)]
pub struct OverriddenTx<T> {
    inner: T,
    overrides: TxOverrides,
}

impl<T> OverriddenTx<T> {
    /// Get a reference to the inner transaction.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

// The block executor builds the environment through `into_parts`; this is where the overrides
// apply.
impl<T, Tx> ExecutableTxParts<MegaTransaction, Tx> for OverriddenTx<T>
where
    T: ExecutableTxParts<MegaTransaction, Tx>,
{
    type Recovered = T::Recovered;

    fn into_parts(self) -> (MegaTransaction, Self::Recovered) {
        let (mut tx, recovered) = self.inner.into_parts();
        self.overrides.apply(&mut tx);
        (tx, recovered)
    }
}

// Delegate MegaTransactionExt to inner so `OverriddenTx` is accepted by `run_transaction`: the
// hash and the two sizes are the original transaction's.
impl<T: MegaTransactionExt> MegaTransactionExt for OverriddenTx<T> {
    fn tx_hash(&self) -> TxHash {
        self.inner.tx_hash()
    }

    fn tx_size(&self) -> u64 {
        self.inner.tx_size()
    }

    fn estimated_da_size(&self) -> u64 {
        self.inner.estimated_da_size()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes};
    use mega_evm::{
        alloy_evm::{block::ExecutableTxParts, tx::RecoveredTx},
        alloy_op_evm::OpTx,
        op_revm::OpTransaction,
        revm::context::tx::TxEnvBuilder,
    };

    use super::*;

    struct EnvOnly(MegaTransaction);

    struct NoRecovered;

    impl ExecutableTxParts<MegaTransaction, ()> for EnvOnly {
        type Recovered = NoRecovered;

        fn into_parts(self) -> (MegaTransaction, NoRecovered) {
            (self.0, NoRecovered)
        }
    }

    impl RecoveredTx<()> for NoRecovered {
        fn tx(&self) -> &() {
            &()
        }

        fn signer(&self) -> &Address {
            &Address::ZERO
        }
    }

    fn blank() -> EnvOnly {
        let tx_env = TxEnvBuilder::new().tx_type(Some(0)).build().expect("legacy transaction");
        EnvOnly(OpTx(OpTransaction::new(tx_env)))
    }

    #[test]
    fn test_input_overrides_stay_on_their_wrapper() {
        let first = TxOverrideArgs { input: Some("0x11".to_string()), ..TxOverrideArgs::default() };
        let second =
            TxOverrideArgs { input: Some("0x22".to_string()), ..TxOverrideArgs::default() };
        let wrapped_first = first.wrap(blank()).expect("first override");
        let wrapped_second = second.wrap(blank()).expect("second override");
        let (tx_first, _) = ExecutableTxParts::<MegaTransaction, ()>::into_parts(wrapped_first);
        let (tx_second, _) = ExecutableTxParts::<MegaTransaction, ()>::into_parts(wrapped_second);
        assert_eq!(tx_first.base.data, Bytes::from_static(&[0x11]));
        assert_eq!(tx_second.base.data, Bytes::from_static(&[0x22]));
    }
}
