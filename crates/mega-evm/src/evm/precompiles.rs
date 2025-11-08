//! Custom precompiles for `MegaETH` EVM.
//!
//! This module provides custom precompile implementations with `MegaETH`-specific
//! gas cost overrides.

use crate::{ExternalEnvs, MegaContext, MegaSpecId};
use alloy_evm::{precompiles::PrecompilesMap, Database};
use delegate::delegate;
use once_cell::race::OnceBox;
use op_revm::{OpContext, OpSpecId};
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{InputsImpl, InterpreterResult},
    precompile::Precompiles,
    primitives::Address,
};

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;

/// `MegaETH` precompile provider with custom gas cost overrides.
#[derive(Debug, Clone)]
pub struct MegaPrecompiles {
    /// Inner precompile provider from op-revm.
    inner: EthPrecompiles,
    /// The `MegaETH` specification ID.
    spec: MegaSpecId,
}

impl MegaPrecompiles {
    /// Create a new precompile provider with the given `MegaETH` spec.
    #[inline]
    pub fn new_with_spec(spec: MegaSpecId) -> Self {
        // Get base precompiles from op-revm
        let inner = match spec {
            MegaSpecId::EQUIVALENCE => op_revm::precompiles::isthmus(),
            MegaSpecId::MINI_REX => mini_rex(),
        };

        Self { inner: EthPrecompiles { precompiles: inner, spec: spec.into_eth_spec() }, spec }
    }

    /// Get the precompiles for the current spec.
    ///
    /// This method returns precompiles with custom gas cost overrides for `MINI_REX` spec.
    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        // For now, just use the inner precompiles
        // Custom gas costs will be applied in the run() method
        self.inner.precompiles
    }
}

/// Precompiles for the `MINI_REX` spec.
pub fn mini_rex() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = op_revm::precompiles::isthmus().clone();
        // Use the OSAKA modexp precompile for MINI_REX
        precompiles
            .extend([revm::precompile::modexp::OSAKA, kzg_point_evaluation::KZG_POINT_EVALUATION]);
        Box::new(precompiles)
    })
}

/// Customized KZG point evaluation precompile module.
pub mod kzg_point_evaluation {
    use revm::precompile::{PrecompileError, PrecompileWithAddress};

    /// Gas cost for the KZG point evaluation precompile.
    pub const GAS_COST: u64 = 100_000;

    /// KZG point evaluation precompile. This is the modified version of the original precompile
    /// with a custom gas cost.
    pub const KZG_POINT_EVALUATION: PrecompileWithAddress = PrecompileWithAddress(
        revm::precompile::kzg_point_evaluation::ADDRESS,
        |input, gas_limit| {
            if gas_limit < GAS_COST {
                return Err(PrecompileError::OutOfGas);
            }
            let mut output = revm::precompile::kzg_point_evaluation::run(input, gas_limit)?;
            output.gas_used = GAS_COST;
            Ok(output)
        },
    );
}

impl<CTX> PrecompileProvider<CTX> for MegaPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = MegaSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }

        *self = Self::new_with_spec(spec);
        true
    }

    delegate! {
        to self.inner {
            fn run(
                &mut self,
                context: &mut CTX,
                address: &Address,
                inputs: &InputsImpl,
                is_static: bool,
                gas_limit: u64,
            ) -> Result<Option<Self::Output>, String>;
            fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>>;
            fn contains(&self, address: &Address) -> bool;
        }
    }
}

impl Default for MegaPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(MegaSpecId::EQUIVALENCE)
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvs> PrecompileProvider<MegaContext<DB, ExtEnvs>>
    for PrecompilesMap
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: OpSpecId) -> bool {
        PrecompileProvider::<OpContext<DB>>::set_spec(self, spec)
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut MegaContext<DB, ExtEnvs>,
        address: &Address,
        inputs: &InputsImpl,
        is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
        let maybe_output = PrecompileProvider::<OpContext<DB>>::run(
            self, context, address, inputs, is_static, gas_limit,
        )?;
        // Record the compute gas cost
        Ok(maybe_output.inspect(|output| {
            if context.spec.is_enabled(MegaSpecId::MINI_REX) {
                context.additional_limit.borrow_mut().record_compute_gas(output.gas.spent());
            }
        }))
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        PrecompileProvider::<OpContext<DB>>::warm_addresses(self)
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<OpContext<DB>>::contains(self, address)
    }
}

#[cfg(test)]
mod tests {
    use super::{kzg_point_evaluation::GAS_COST, MegaPrecompiles};
    use crate::{test_utils::MemoryDatabase, DefaultExternalEnvs, MegaContext, MegaSpecId};
    use alloy_evm::precompiles::PrecompilesMap;
    use alloy_primitives::Bytes;
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

    #[test]
    fn test_kzg_precompile_sufficient_gas() {
        let mut db = MemoryDatabase::default();
        let ext_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &ext_envs);
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
        let ext_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &ext_envs);
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
    fn test_kzg_precompile_insufficient_gas() {
        let mut db = MemoryDatabase::default();
        let ext_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &ext_envs);
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
        println!("interpreter_result: {:?}", interpreter_result);
        assert!(
            matches!(interpreter_result.result, InstructionResult::PrecompileOOG),
            "Result should be PrecompileOOG"
        );
    }

    #[test]
    fn test_kzg_precompile_one_below_gas_limit() {
        let mut db = MemoryDatabase::default();
        let ext_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &ext_envs);
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
        let ext_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
        let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &ext_envs);
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
}
