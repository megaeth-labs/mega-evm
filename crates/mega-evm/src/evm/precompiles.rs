//! The precompile set of the Satin engine.
//!
//! Satin runs op-revm's Karst set — the Osaka `ModExp` schedule, `P256VERIFY` at 6,900, the BN254
//! pairing bounded at 57,600 bytes of input — with one entry replaced: KZG point evaluation costs
//! [`kzg_point_evaluation::GAS_COST`] instead of upstream's 50,000, because verifying a proof is
//! far more work than that price buys on a `MegaETH` sequencer.
//!
//! The set is handed to the EVM as an alloy-evm [`PrecompilesMap`], so a node can add or replace
//! an address at runtime ([`DynPrecompilesBuilder`]). A map built from the static set looks the
//! address up in the same table op-revm would, so carrying it costs no allocation per call.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{string::String, sync::Arc};

use alloy_evm::{
    precompiles::{DynPrecompile, PrecompilesMap},
    Database,
};
use revm::{
    handler::PrecompileProvider,
    interpreter::{CallInputs, InterpreterResult},
    precompile::Precompiles,
    primitives::{Address, AddressSet, HashMap, OnceLock},
};

use crate::{ExternalEnvTypes, MegaContext, MegaInnerContext, MegaSpecId};

/// The Satin precompile set: op-revm's Karst set with `MegaETH`'s KZG price.
pub fn satin_precompiles() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = op_revm::precompiles::karst().clone();
        precompiles.extend([kzg_point_evaluation::KZG_POINT_EVALUATION]);
        precompiles
    })
}

/// A precompile map carrying the Satin set, ready for a node to add its own entries to.
pub fn satin_precompiles_map() -> PrecompilesMap {
    PrecompilesMap::from_static(satin_precompiles())
}

/// Builds the dynamic precompiles an EVM runs on top of the Satin set.
///
/// The spec is passed so a builder can key on it; Satin is a single spec, so it is always
/// [`MegaSpecId::SATIN`] today.
pub type DynPrecompilesBuilder =
    Arc<dyn Fn(MegaSpecId) -> HashMap<Address, DynPrecompile> + Send + Sync>;

/// KZG point evaluation at `MegaETH`'s price.
pub mod kzg_point_evaluation {
    use revm::{
        precompile::{
            Precompile, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
        },
        primitives::Address,
    };

    /// Address of the KZG point evaluation precompile, as upstream has it.
    pub const ADDRESS: Address = revm::precompile::kzg_point_evaluation::ADDRESS;

    /// What a KZG point evaluation costs on `MegaETH`: a flat price, as upstream's is, twice
    /// upstream's 50,000.
    pub const GAS_COST: u64 = 100_000;

    /// Runs upstream's KZG point evaluation and reports [`GAS_COST`] for it.
    ///
    /// The gas check comes first, so a call that cannot pay the `MegaETH` price is out of gas
    /// before any verification runs, exactly as upstream is out of gas below its own price.
    fn run_with_fixed_cost(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
        if gas_limit < GAS_COST {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
        }
        Ok(match revm::precompile::kzg_point_evaluation::run(input, gas_limit) {
            Ok(output) => PrecompileOutput::new(GAS_COST, output.bytes, reservoir),
            Err(halt) => PrecompileOutput::halt(halt, reservoir),
        })
    }

    /// KZG point evaluation, priced at [`GAS_COST`].
    pub const KZG_POINT_EVALUATION: Precompile =
        Precompile::new(PrecompileId::KzgPointEvaluation, ADDRESS, run_with_fixed_cost);
}

/// Runs the Satin precompile set on a [`MegaContext`].
///
/// alloy-evm implements the provider for revm's plain context; every method here hands it the
/// context [`MegaContext`] wraps, which is that context.
impl<DB: Database, ExtEnvs: ExternalEnvTypes> PrecompileProvider<MegaContext<DB, ExtEnvs>>
    for PrecompilesMap
{
    type Output = InterpreterResult;

    /// The table is already the Satin one; the engine has a single spec, so nothing can change
    /// it. Rebuilding from the base spec would drop the `MegaETH` entries and a node's own.
    #[inline]
    fn set_spec(&mut self, _spec: op_revm::OpSpecId) -> bool {
        false
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        PrecompileProvider::<MegaInnerContext<DB>>::run(self, &mut context.inner, inputs)
    }

    #[inline]
    fn warm_addresses(&self) -> &AddressSet {
        PrecompileProvider::<MegaInnerContext<DB>>::warm_addresses(self)
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<MegaInnerContext<DB>>::contains(self, address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MemoryDatabase;
    use alloy_primitives::{hex, Bytes};
    use op_revm::precompiles::bn254_pair::KARST_MAX_INPUT_SIZE;
    use revm::{
        interpreter::{CallInput, CallScheme, CallValue, InstructionResult},
        precompile::secp256r1,
        primitives::U256,
    };

    /// The c-kzg test vector `verify_kzg_proof_case_correct_proof_4_4`, as the precompile takes
    /// it: `versioned_hash ++ z ++ y ++ commitment ++ proof`.
    fn kzg_input() -> Bytes {
        let commitment = hex!(
            "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
             5f26936857bc3a7c2539ea8ec3a952b7"
        );
        let mut versioned_hash =
            revm::precompile::kzg_point_evaluation::kzg_to_versioned_hash(commitment.as_slice())
                .to_vec();
        let z = hex!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000").to_vec();
        let y = hex!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9").to_vec();
        let proof = hex!(
            "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
             4faf0070725e00b60ad9a026a15b1a8c"
        );
        versioned_hash.extend_from_slice(&z);
        versioned_hash.extend_from_slice(&y);
        versioned_hash.extend_from_slice(&commitment);
        versioned_hash.extend_from_slice(&proof);
        Bytes::from(versioned_hash)
    }

    /// What upstream returns from a successful evaluation: the field parameters.
    const KZG_RETURN_VALUE: &[u8; 64] = revm::precompile::kzg_point_evaluation::RETURN_VALUE;

    fn inputs(to: Address, input: Bytes, gas_limit: u64) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(input),
            return_memory_offset: 0..0,
            gas_limit,
            reservoir: 0,
            bytecode_address: to,
            known_bytecode: Default::default(),
            target_address: to,
            caller: Address::ZERO,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            charged_new_account_state_gas: false,
        }
    }

    /// Runs `input` against the precompile at `to` and returns its result.
    fn run(to: Address, input: Bytes, gas_limit: u64) -> InterpreterResult {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::SATIN);
        satin_precompiles_map()
            .run(&mut context, &inputs(to, input, gas_limit))
            .expect("the precompile does not fail fatally")
            .expect("the address is a precompile")
    }

    /// A valid proof costs the `MegaETH` price and returns the field parameters.
    #[test]
    fn test_kzg_precompile_sufficient_gas() {
        let result = run(kzg_point_evaluation::ADDRESS, kzg_input(), 200_000);
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.total_gas_spent(), kzg_point_evaluation::GAS_COST);
        assert_eq!(result.output.as_ref(), KZG_RETURN_VALUE.as_slice());
    }

    /// The price is exactly payable at the price itself.
    #[test]
    fn test_kzg_precompile_exact_gas_limit() {
        let result =
            run(kzg_point_evaluation::ADDRESS, kzg_input(), kzg_point_evaluation::GAS_COST);
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.total_gas_spent(), kzg_point_evaluation::GAS_COST);
        assert_eq!(result.output.as_ref(), KZG_RETURN_VALUE.as_slice());
    }

    /// Below the `MegaETH` price the call is out of gas, including at upstream's price, which
    /// would have been enough before the override.
    #[test]
    fn test_kzg_precompile_insufficient_gas() {
        for gas_limit in [0, 50_000, kzg_point_evaluation::GAS_COST - 1] {
            let result = run(kzg_point_evaluation::ADDRESS, kzg_input(), gas_limit);
            assert_eq!(result.result, InstructionResult::PrecompileOOG, "at {gas_limit} gas");
        }
    }

    /// Nothing can put upstream's KZG price back: the engine has one spec, so `set_spec` has
    /// nothing to switch to and leaves the table — and a node's own entries — alone.
    #[test]
    fn test_set_spec_keeps_the_mega_kzg_override() {
        let mut db = MemoryDatabase::default();
        let mut context = MegaContext::new(&mut db, MegaSpecId::SATIN);
        let mut map = satin_precompiles_map();

        for spec in [op_revm::OpSpecId::ISTHMUS, op_revm::OpSpecId::KARST] {
            let changed =
                PrecompileProvider::<MegaContext<&mut MemoryDatabase>>::set_spec(&mut map, spec);
            assert!(!changed, "the table must stay the Satin one");
        }

        let result = map
            .run(&mut context, &inputs(kzg_point_evaluation::ADDRESS, kzg_input(), 200_000))
            .unwrap()
            .unwrap();
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.total_gas_spent(), kzg_point_evaluation::GAS_COST);
    }

    /// The `MegaETH` entry replaces upstream's rather than sitting beside it: the set holds one
    /// KZG precompile, at the same address, and the price it charges is `MegaETH`'s.
    #[test]
    fn test_the_kzg_entry_replaces_upstreams() {
        let satin = satin_precompiles();
        let karst = op_revm::precompiles::karst();
        assert_eq!(satin.len(), karst.len(), "no address was added");
        assert!(satin.contains(&kzg_point_evaluation::ADDRESS));
        assert_ne!(
            kzg_point_evaluation::GAS_COST,
            revm::precompile::kzg_point_evaluation::GAS_COST,
            "the price is not upstream's"
        );
    }

    /// Every other Karst entry is op-revm's, at the address op-revm has it.
    #[test]
    fn test_every_other_entry_is_the_karst_one() {
        let satin = satin_precompiles();
        for address in op_revm::precompiles::karst().addresses() {
            assert!(satin.contains(address), "{address} is missing");
        }
    }

    /// `P256VERIFY` is priced at the Osaka fee, which is what Karst put in the set.
    #[test]
    fn test_p256_verify_is_priced_at_the_osaka_fee() {
        assert_eq!(secp256r1::P256VERIFY_BASE_GAS_FEE_OSAKA, 6_900);
        let address = *secp256r1::P256VERIFY_OSAKA.address();
        // An all-zero input is a well-formed 160-byte call whose signature does not verify: it
        // returns empty output and still pays the base fee.
        let result = run(address, Bytes::from(std::vec![0u8; 160]), 100_000);
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.total_gas_spent(), 6_900);
        assert!(result.output.is_empty(), "the signature does not verify");
    }

    /// The BN254 pairing takes input up to the Karst bound and refuses one byte more, without
    /// charging for the work it did not do.
    #[test]
    fn test_bn254_pairing_is_bounded_at_the_karst_size() {
        assert_eq!(KARST_MAX_INPUT_SIZE, 57_600);
        let address = *op_revm::precompiles::bn254_pair::KARST.address();
        // At the bound the input is a whole number of pair elements of zeros, which pair to the
        // identity: the call succeeds and pays the Istanbul pairing price.
        let at_bound = run(address, Bytes::from(std::vec![0u8; KARST_MAX_INPUT_SIZE]), 50_000_000);
        assert_eq!(at_bound.result, InstructionResult::Return);
        assert!(at_bound.gas.total_gas_spent() > 0);

        let over = run(address, Bytes::from(std::vec![0u8; KARST_MAX_INPUT_SIZE + 1]), 50_000_000);
        assert_eq!(over.result, InstructionResult::PrecompileError);
    }

    /// `ModExp` runs on the Osaka formula, which prices a small exponentiation well above the
    /// 200-gas Berlin minimum the earlier entry charged.
    #[test]
    fn test_modexp_is_priced_on_the_osaka_formula() {
        let address = *revm::precompile::modexp::OSAKA.address();
        // base = 3 (1 byte), exponent = 2 (1 byte), modulus = 5 (1 byte): 3^2 mod 5 = 4.
        let mut input = std::vec![0u8; 96];
        input[31] = 1;
        input[63] = 1;
        input[95] = 1;
        input.extend_from_slice(&[3, 2, 5]);
        let result = run(address, Bytes::from(input), 100_000);
        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.output.as_ref(), &[4]);
        assert_eq!(result.gas.total_gas_spent(), 500, "the Osaka minimum");
    }
}
