//! Unit tests extracted from `crates/mega-evm/src/sandbox/error.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/sandbox/error.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// `InternalError` and `InvalidTransaction` carry no payload on the wire, so a
    /// roundtrip MUST produce the same variant. Pinned because these selectors are the
    /// off-chain error ABI (RPC, traces, relayer decoders); a divergence between
    /// `encode_error_result` and `decode_error_result` would silently break those decoders.
    #[test]
    fn test_internal_error_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::InternalError);
        // Selector-only: the encoded form is exactly the 4-byte Solidity selector.
        assert_eq!(encoded.len(), 4, "InternalError must be selector-only");
        assert!(matches!(decode_error_result(&encoded), Some(KeylessDeployError::InternalError)));
    }

    #[test]
    fn test_invalid_transaction_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::InvalidTransaction);
        assert_eq!(encoded.len(), 4, "InvalidTransaction must be selector-only");
        assert!(matches!(
            decode_error_result(&encoded),
            Some(KeylessDeployError::InvalidTransaction)
        ));
    }

    /// `InitCodeTooLarge { size, max }` is an externally visible ABI selector. Pinning the
    /// round-trip catches any drift between the Solidity error definition and the Rust
    /// encode/decode (e.g. forgotten arm in `encode_error_result` or `decode_error_result`,
    /// or accidental field reordering).
    #[test]
    fn test_init_code_too_large_roundtrip_preserves_size_and_max() {
        let original = KeylessDeployError::InitCodeTooLarge { size: 600_000, max: 548_864 };
        let encoded = encode_error_result(original.clone());
        let decoded = decode_error_result(&encoded).expect("must decode");
        assert_eq!(decoded, original);
    }

    /// `SignerHasCode` is selector-only; pinning the round-trip catches arm drift.
    #[test]
    fn test_signer_has_code_roundtrip_is_selector_only() {
        let encoded = encode_error_result(KeylessDeployError::SignerHasCode);
        assert_eq!(encoded.len(), 4, "SignerHasCode must be selector-only");
        assert!(matches!(decode_error_result(&encoded), Some(KeylessDeployError::SignerHasCode)));
    }
}
