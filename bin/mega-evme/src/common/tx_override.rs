//! Transaction override support for mega-evme replay command.
//!
//! This module provides the ability to override transaction fields when replaying
//! transactions from RPC.

use alloy_eips::{Encodable2718, Typed2718};
use alloy_primitives::{Address, Bytes, TxHash, U256};
use clap::Args;
use mega_evm::{
    alloy_evm::{IntoTxEnv, RecoveredTx},
    MegaTransaction, MegaTransactionExt,
};

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

    /// Parse the overrides: read the input override (from `--override.input`
    /// or `--override.input-file`) and the value override.
    pub fn parse(&self) -> Result<TxOverrides> {
        // The input is read first, so a run with two bad overrides reports the
        // input's failure.
        let input = load_hex(self.input.clone(), self.input_file.clone())?;
        Ok(TxOverrides {
            gas_limit: self.gas_limit,
            value: self.value.as_deref().map(parse_ether_value).transpose()?,
            input,
        })
    }
}

/// Parsed transaction overrides.
///
/// Owns every override, the input bytes included, so each set of overrides is
/// independent of any other: the [`OverriddenTx`] wrappers it hands out borrow
/// it, which is what lets them stay `Copy` (as the block executor's
/// `run_transaction` requires) while carrying non-`Copy` input.
#[derive(Debug, Clone, Default)]
pub struct TxOverrides {
    /// Override for gas limit.
    gas_limit: Option<u64>,
    /// Override for value.
    value: Option<U256>,
    /// Override for input data.
    input: Option<Bytes>,
}

impl TxOverrides {
    /// Wraps a transaction so it converts to a [`MegaTransaction`] with these
    /// overrides applied.
    pub const fn wrap<T: Copy>(&self, tx: T) -> OverriddenTx<'_, T> {
        OverriddenTx { inner: tx, overrides: self }
    }

    /// Apply overrides to a [`MegaTransaction`].
    fn apply(&self, tx: &mut MegaTransaction) {
        if let Some(gas_limit) = self.gas_limit {
            tx.base.gas_limit = gas_limit;
        }
        if let Some(value) = self.value {
            tx.base.value = value;
        }
        if let Some(input) = &self.input {
            tx.base.data = input.clone();
        }
    }
}

/// A wrapper that applies overrides when converting to `TxEnv`.
///
/// This wrapper implements all the required traits by delegating to the inner
/// transaction, but intercepts `IntoTxEnv` to apply overrides.
#[derive(Debug, Clone, Copy)]
pub struct OverriddenTx<'a, T: Copy> {
    inner: T,
    overrides: &'a TxOverrides,
}

// Implement IntoTxEnv - this is where we apply the overrides
impl<T: IntoTxEnv<MegaTransaction> + Copy> IntoTxEnv<MegaTransaction> for OverriddenTx<'_, T> {
    fn into_tx_env(self) -> MegaTransaction {
        let mut tx = self.inner.into_tx_env();
        self.overrides.apply(&mut tx);
        tx
    }
}

// Delegate RecoveredTx to inner
impl<Tx, T: RecoveredTx<Tx> + Copy> RecoveredTx<Tx> for OverriddenTx<'_, T> {
    fn tx(&self) -> &Tx {
        self.inner.tx()
    }

    fn signer(&self) -> &Address {
        self.inner.signer()
    }
}

// Delegate Typed2718 to inner (required as the `Encodable2718` supertrait below).
impl<T: Typed2718 + Copy> Typed2718 for OverriddenTx<'_, T> {
    fn ty(&self) -> u8 {
        self.inner.ty()
    }
}

// Delegate Encodable2718 to inner. Overrides only affect the `TxEnv` produced by `IntoTxEnv`, not
// the EIP-2718 encoding, so the encoded size reflects the original transaction — matching the
// `tx_size`/`da_size` the executor charged before override support existed.
impl<T: Encodable2718 + Copy> Encodable2718 for OverriddenTx<'_, T> {
    fn type_flag(&self) -> Option<u8> {
        self.inner.type_flag()
    }

    fn encode_2718_len(&self) -> usize {
        self.inner.encode_2718_len()
    }

    fn encode_2718(&self, out: &mut dyn alloy_primitives::bytes::BufMut) {
        self.inner.encode_2718(out)
    }
}

// Delegate MegaTransactionExt to inner so `OverriddenTx` is accepted by `run_transaction`.
// `tx_size`/`estimated_da_size` fall back to the trait defaults (recomputed from the delegated
// encoding above); only `tx_hash` needs explicit forwarding.
impl<T: MegaTransactionExt + Copy> MegaTransactionExt for OverriddenTx<'_, T> {
    fn tx_hash(&self) -> TxHash {
        self.inner.tx_hash()
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
    use alloy_primitives::{Signature, B256};
    use mega_evm::MegaTxEnvelope;

    use super::*;

    /// A legacy call carrying `input`, as the endpoint would serve it.
    fn envelope(input: &[u8]) -> MegaTxEnvelope {
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                gas_limit: 21_000,
                input: Bytes::copy_from_slice(input),
                ..Default::default()
            },
            Signature::new(U256::ONE, U256::ONE, false),
            B256::ZERO,
        ))
    }

    /// Overrides parsed from `input` alone.
    fn input_override(input: Option<&str>) -> TxOverrides {
        TxOverrideArgs { input: input.map(str::to_string), ..Default::default() }
            .parse()
            .expect("the overrides parse")
    }

    /// Each wrapper carries its own overrides: wrappers made from different
    /// override sets, held at once and converted in any order, each apply their
    /// own input, and a wrapper without an input override keeps the original.
    #[test]
    fn test_interleaved_wrappers_keep_their_own_overrides() {
        let tx = envelope(&[0x01]);
        let recovered = Recovered::new_unchecked(&tx, Address::ZERO);
        let first = input_override(Some("0xaa"));
        let second = input_override(Some("0xbb"));
        let none = input_override(None);

        let wrapped_first = first.wrap(recovered);
        let wrapped_second = second.wrap(recovered);
        let wrapped_none = none.wrap(recovered);

        assert_eq!(wrapped_none.into_tx_env().base.data, Bytes::from_static(&[0x01]));
        assert_eq!(wrapped_first.into_tx_env().base.data, Bytes::from_static(&[0xaa]));
        assert_eq!(wrapped_second.into_tx_env().base.data, Bytes::from_static(&[0xbb]));
        assert_eq!(wrapped_first.into_tx_env().base.data, Bytes::from_static(&[0xaa]));
    }

    /// Gas limit and value overrides apply alongside the input.
    #[test]
    fn test_overrides_apply_every_field() {
        let tx = envelope(&[0x01]);
        let overrides = TxOverrideArgs {
            gas_limit: Some(50_000),
            value: Some("7".to_string()),
            input: Some("0xcc".to_string()),
            input_file: None,
        }
        .parse()
        .expect("the overrides parse");

        let converted = overrides.wrap(Recovered::new_unchecked(&tx, Address::ZERO)).into_tx_env();
        assert_eq!(converted.base.gas_limit, 50_000);
        assert_eq!(converted.base.value, U256::from(7));
        assert_eq!(converted.base.data, Bytes::from_static(&[0xcc]));
    }
}
