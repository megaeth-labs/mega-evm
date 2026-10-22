//! Unit tests extracted from `crates/mega-evm/src/evm/precompiles.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/precompiles.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "std"))]
    use alloc as std;
    use std::{rc::Rc, vec::Vec};

    use super::{kzg_point_evaluation::GAS_COST, mini_rex, rex, MegaPrecompiles};
    use crate::{
        test_utils::MemoryDatabase, AdditionalLimit, EvmTxRuntimeLimits, MegaContext, MegaSpecId,
    };
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Bytes;
    use core::cell::RefCell;
    use revm::{
        handler::PrecompileProvider,
        interpreter::{InputsImpl, InstructionResult},
    };
    use sha2::{Digest, Sha256};

    /// Generate valid KZG Point Evaluation test data from EIP-4844 test vectors.
    fn generate_kzg_test_input() -> InputsImpl {
        let commitment = hex::decode("8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca25f26936857bc3a7c2539ea8ec3a952b7").unwrap();
        let mut versioned_hash = Sha256::digest(&commitment).to_vec();
        versioned_hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
        let z = hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000")
            .unwrap();
        let y = hex::decode("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9")
            .unwrap();
        let proof = hex::decode("a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc2160744faf0070725e00b60ad9a026a15b1a8c").unwrap();

        let mut input = Vec::new();
        input.extend_from_slice(&versioned_hash);
        input.extend_from_slice(&z);
        input.extend_from_slice(&y);
        input.extend_from_slice(&commitment);
        input.extend_from_slice(&proof);

        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        InputsImpl {
            target_address: address,
            bytecode_address: Some(address),
            caller_address: address,
            input: revm::interpreter::CallInput::Bytes(Bytes::from(input)),
            call_value: Default::default(),
        }
    }

    /// Mirror of `generate_kzg_test_input()` but with the last byte of the proof
    /// flipped — structurally valid (192 bytes, matching versioned hash) but
    /// upstream verification returns `PrecompileError::BlobVerifyKzgProofFailed`.
    fn generate_invalid_proof_kzg_test_input() -> InputsImpl {
        let mut inputs = generate_kzg_test_input();
        let bytes = match inputs.input {
            revm::interpreter::CallInput::Bytes(b) => b,
            _ => panic!("expected Bytes"),
        };
        let mut buf = bytes.to_vec();
        let last = buf.len() - 1;
        buf[last] ^= 0x01;
        inputs.input = revm::interpreter::CallInput::Bytes(Bytes::from(buf));
        inputs
    }

    fn set_spec_for_context<DB: alloy_evm::Database, ExtEnvs: crate::ExternalEnvTypes>(
        precompiles_map: &mut PrecompilesMap,
        _context: &MegaContext<DB, ExtEnvs>,
        spec: op_revm::OpSpecId,
    ) -> bool {
        <PrecompilesMap as PrecompileProvider<MegaContext<DB, ExtEnvs>>>::set_spec(
            precompiles_map,
            spec,
        )
    }

    #[test]
    fn test_kzg_precompile_sufficient_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 200_000);
        assert!(result.is_ok(), "Precompile should succeed with sufficient gas");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.spent(), GAS_COST);
    }

    #[test]
    fn test_kzg_precompile_exact_gas_limit() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, GAS_COST);
        assert!(result.is_ok(), "Precompile should succeed with exact GAS_COST");
        let output = result.unwrap().unwrap();
        assert!(matches!(output.result, InstructionResult::Return), "Result should be Return");
        assert_eq!(output.gas.spent(), GAS_COST);
    }

    #[test]
    fn test_set_spec_preserves_mega_kzg_override() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );

        let changed =
            set_spec_for_context(&mut precompiles_map, &context, op_revm::OpSpecId::ISTHMUS);
        assert!(!changed, "Mega precompile table must remain unchanged");

        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;
        let result = precompiles_map.run(&mut context, &address, &inputs, true, 200_000);
        let output = result.expect("run ok").expect("Some output");
        assert!(matches!(output.result, InstructionResult::Return));
        assert_eq!(output.gas.spent(), GAS_COST);
    }

    #[test]
    fn test_kzg_precompile_insufficient_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 50_000);
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    #[test]
    fn test_kzg_precompile_one_below_gas_limit() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, GAS_COST - 1);
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    #[test]
    fn test_kzg_precompile_zero_gas() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX);
        let mut precompiles_map = PrecompilesMap::from_static(
            MegaPrecompiles::new_with_spec(MegaSpecId::MINI_REX).precompiles(),
        );
        let inputs = generate_kzg_test_input();
        let address = revm::precompile::kzg_point_evaluation::ADDRESS;

        let result = precompiles_map.run(&mut context, &address, &inputs, true, 0);
        assert!(result.is_ok(), "Should not panic");
        let output = result.unwrap();
        assert!(output.is_some(), "Precompile should return Some(result)");
        let interpreter_result = output.unwrap();
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    /// Replaces the context's `additional_limit` with one whose `tx_compute_gas_limit`
    /// is set to `limit`, so tests can exercise the REX5 compute-gas cap path with a
    /// small remaining budget without having to pre-record large gas amounts.
    fn set_tx_compute_gas_limit<DB: alloy_evm::Database, ExtEnvs: crate::ExternalEnvTypes>(
        context: &mut MegaContext<DB, ExtEnvs>,
        spec: MegaSpecId,
        limit: u64,
    ) {
        let tx_limits = EvmTxRuntimeLimits {
            tx_compute_gas_limit: limit,
            ..EvmTxRuntimeLimits::from_spec(spec)
        };
        context.additional_limit = Rc::new(RefCell::new(AdditionalLimit::new(spec, tx_limits)));
    }

}
