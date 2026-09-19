//! Unit tests extracted from `crates/mega-evm/src/block/helpers.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/helpers.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
    use alloy_primitives::{address, bytes::BufMut, Signature};
    use revm::context::TxEnv;

    const CALLER: Address = address!("2000000000000000000000000000000000000001");
    const CONTRACT: Address = address!("3000000000000000000000000000000000000001");

    #[derive(Debug, Clone)]
    struct MockTx {
        nonce: u64,
        gas_limit: u64,
        value: U256,
        input: Bytes,
    }

    impl Typed2718 for MockTx {
        fn ty(&self) -> u8 {
            0
        }
    }

    impl Encodable2718 for MockTx {
        fn encode_2718_len(&self) -> usize {
            self.input.len()
        }

        fn encode_2718(&self, out: &mut dyn BufMut) {
            out.put_slice(&self.input);
        }
    }

    impl Transaction for MockTx {
        fn chain_id(&self) -> Option<ChainId> {
            Some(1)
        }

        fn nonce(&self) -> u64 {
            self.nonce
        }

        fn gas_limit(&self) -> u64 {
            self.gas_limit
        }

        fn gas_price(&self) -> Option<u128> {
            Some(9)
        }

        fn max_fee_per_gas(&self) -> u128 {
            9
        }

        fn max_priority_fee_per_gas(&self) -> Option<u128> {
            None
        }

        fn max_fee_per_blob_gas(&self) -> Option<u128> {
            None
        }

        fn priority_fee_or_price(&self) -> u128 {
            9
        }

        fn effective_gas_price(&self, _base_fee: Option<u64>) -> u128 {
            9
        }

        fn is_dynamic_fee(&self) -> bool {
            false
        }

        fn kind(&self) -> TxKind {
            TxKind::Call(CONTRACT)
        }

        fn is_create(&self) -> bool {
            false
        }

        fn value(&self) -> U256 {
            self.value
        }

        fn input(&self) -> &Bytes {
            &self.input
        }

        fn access_list(&self) -> Option<&AccessList> {
            None
        }

        fn blob_versioned_hashes(&self) -> Option<&[B256]> {
            None
        }

        fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
            None
        }
    }

    #[derive(Debug, Clone)]
    struct MockRecoveredTx {
        tx: TxEnv,
        signer: Address,
    }

    impl RecoveredTx<TxEnv> for MockRecoveredTx {
        fn tx(&self) -> &TxEnv {
            &self.tx
        }

        fn signer(&self) -> &Address {
            &self.signer
        }
    }

    impl IntoTxEnv<TxEnv> for MockRecoveredTx {
        fn into_tx_env(self) -> TxEnv {
            self.tx
        }
    }

    fn legacy_tx() -> TxLegacy {
        TxLegacy {
            chain_id: Some(1),
            nonce: 7,
            gas_price: 9,
            gas_limit: 21_000,
            to: TxKind::Call(CONTRACT),
            value: U256::from(11),
            input: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        }
    }

    fn legacy_envelope() -> MegaTxEnvelope {
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            legacy_tx(),
            Signature::test_signature(),
            Default::default(),
        ))
    }

    #[test]
    fn test_mega_transaction_ext_works_for_envelope_and_recovered_types() {
        let tx = legacy_envelope();
        let recovered = Recovered::new_unchecked(tx.clone(), CALLER);

        assert_eq!(MegaTransactionExt::tx_hash(&tx), tx.tx_hash());
        assert_eq!(MegaTransactionExt::tx_hash(&recovered), tx.tx_hash());
        assert!(MegaTransactionExt::estimated_da_size(&tx) > 0);
        assert!(MegaTransactionExt::tx_size(&tx) > 0);
    }

    #[test]
    fn test_enriched_mega_tx_new_slow_computes_hash_and_sizes() {
        let tx = MockTx {
            nonce: 7,
            gas_limit: 21_000,
            value: U256::from(11),
            input: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]),
        };
        let expected_hash = tx.trie_hash();
        let expected_da_size =
            op_alloy_flz::tx_estimated_size_fjord_bytes(tx.encoded_2718().as_slice());
        let expected_tx_size = tx.encode_2718_len() as u64;

        let enriched = EnrichedMegaTx::new_slow(tx);

        assert_eq!(MegaTransactionExt::tx_hash(&enriched), expected_hash);
        assert_eq!(enriched.da_size, expected_da_size);
        assert_eq!(enriched.tx_size, expected_tx_size);
        assert_eq!(enriched.nonce(), 7);
        assert_eq!(enriched.gas_limit(), 21_000);
        assert_eq!(enriched.value(), U256::from(11));
        assert_eq!(enriched.kind(), TxKind::Call(CONTRACT));
    }

    #[test]
    fn test_enriched_mega_tx_delegates_recovered_transaction_methods() {
        let tx_env = TxEnv {
            caller: CALLER,
            gas_limit: 21_000,
            kind: TxKind::Call(CONTRACT),
            value: U256::from(11),
            data: Bytes::from_static(&[0xaa, 0xbb]),
            ..Default::default()
        };
        let recovered = MockRecoveredTx { tx: tx_env.clone(), signer: CALLER };
        let enriched = EnrichedMegaTx::new(recovered, TxHash::ZERO, 1, 2);

        assert_eq!(enriched.tx(), &tx_env);
        assert_eq!(*enriched.signer(), CALLER);

        let converted: TxEnv = enriched.into_tx_env();
        assert_eq!(converted, tx_env);
    }
}
