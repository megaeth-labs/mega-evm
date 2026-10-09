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
//!
//! # Prices
//!
//! Gas detention needs a precompile call's price before the call runs, to tell a call its
//! allowance can pay from one that crosses the limit ([`PricedPrecompiles::price`]). A price
//! belongs to the precompile that runs: every entry revm defines carries one
//! ([`Precompile::required_gas`](revm::precompile::Precompile::required_gas)), and so does the
//! KZG entry here. So does every one of op-revm's size-limited wrappers — here the BN254 pairing
//! and the BLS12-381 G1 MSM, G2 MSM and pairing — which prices an input within its size limit as
//! the run it wraps does, and one past it at nothing, since it refuses that input before any gas
//! check. Every entry of the table is priced.
//!
//! The map erases what it holds once a node changes it, so the engine prices from its own table,
//! [`satin_precompiles`], and only an address whose dispatched entry is still the table's. These
//! calls are not priced, and gas detention runs them on the allowance:
//!
//! - a call to a node's own precompile, at a new address;
//! - a call to an address a node replaced: through
//!   [`MegaEvm::with_dyn_precompiles`](crate::MegaEvm), which records it, or through the map's own
//!   API with an entry of another id;
//! - every call, once the whole set was replaced by one that is not the Satin set: the neutral
//!   configuration's, the fixture fork's own.
//!
//! A node changes the set through `with_dyn_precompiles`, or the factory's builder, which calls
//! it. The engine cannot see a change made around it, through the mutable reference to the map
//! that revm's `EvmTr::all_mut` and alloy-evm's `Evm::components_mut` hand out:
//!
//! - an entry replaced through the map's mutable accessors under the id of the entry it replaces;
//! - the whole map replaced by another set: an address whose dispatched entry carries the id of the
//!   Satin table's entry there is then priced from the Satin table, whatever the other set charges
//!   for it.
//!
//! Neither accessor is a sign of a change: revm reaches the context and the frame stack through
//! `all_mut` on every frame, and alloy-evm's transaction tracer calls `components_mut`, so a
//! trace would stop pricing where block execution priced.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{string::String, sync::Arc};

use alloy_evm::{
    precompiles::{DynPrecompile, Precompile as _, PrecompilesMap},
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

/// Which precompile calls gas detention may price from the Satin table: what the engine knows of
/// the changes made to the set it dispatches from.
#[derive(Clone, Debug, Default)]
pub(crate) struct PricedPrecompiles {
    /// The Satin addresses a node replaced with a precompile of its own
    /// ([`MegaEvm::with_dyn_precompiles`](crate::MegaEvm)).
    replaced: AddressSet,
    /// Whether the whole set was replaced, through the engine, by one that is not the Satin set:
    /// the neutral configuration's, the fixture fork's own. A set replaced through a mutable
    /// reference to the map is not recorded.
    foreign: bool,
}

impl PricedPrecompiles {
    /// Records that a node installed a precompile of its own at `address`. An address outside the
    /// Satin set replaces nothing the table prices.
    pub(crate) fn record_replaced(&mut self, address: Address) {
        if satin_precompiles().contains(&address) {
            self.replaced.insert(address);
        }
    }

    /// Records that the whole set was replaced by one that is not the Satin set: nothing is
    /// priced from the Satin table from then on.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) const fn record_foreign(&mut self) {
        self.foreign = true;
    }

    /// The gas a call of `input` to the precompile `dispatched` runs at `address` needs, read
    /// before the call runs, or `None` when the engine cannot price it.
    ///
    /// The price is the Satin table's, and only for an address whose dispatched entry is still
    /// the table's: the set is the Satin one, no node replaced the address through the engine
    /// ([`record_replaced`](Self::record_replaced)), and the entry carries the table entry's id,
    /// so a replacement made through the map's own API under another id is not priced either.
    /// Every entry of the table carries a price function; one built without it would answer
    /// `None`.
    pub(crate) fn price(
        &self,
        dispatched: &PrecompilesMap,
        address: &Address,
        input: &[u8],
    ) -> Option<u64> {
        if self.foreign || self.replaced.contains(address) {
            return None;
        }
        let builtin = satin_precompiles().get(address)?;
        if dispatched.get(address)?.precompile_id() != builtin.id() {
            return None;
        }
        builtin.required_gas(input)
    }
}

/// Builds the dynamic precompiles an EVM runs on top of the Satin set.
///
/// The spec is passed so a builder can key on it; Satin is a single spec, so it is always
/// [`MegaSpecId::SATIN`] today.
///
/// Logs a precompile added here writes itself are not counted in the data size, which counts a
/// log where a `LOG` opcode completes.
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

    /// The price of a KZG point evaluation, read before it runs: [`GAS_COST`] for every input.
    /// A call below it is out of gas, and one at or above it gives the same result on every gas
    /// limit, a malformed input's failure included, since upstream's checks all come after the
    /// gas check.
    const fn required_gas(_input: &[u8]) -> u64 {
        GAS_COST
    }

    /// KZG point evaluation, priced at [`GAS_COST`].
    pub const KZG_POINT_EVALUATION: Precompile =
        Precompile::new(PrecompileId::KzgPointEvaluation, ADDRESS, run_with_fixed_cost)
            .with_required_gas(required_gas);
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
        precompile::{secp256r1, PrecompileId, PrecompileOutput},
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

    /// `contains` answers for the set the provider carries: every Satin address is a precompile
    /// and an address outside the set is not. Nothing in revm's execution path asks, so only a
    /// direct call covers it; a node's RPC is the caller that does.
    #[test]
    fn test_contains_answers_for_the_satin_set() {
        let map = satin_precompiles_map();
        let contains = |address: &Address| {
            PrecompileProvider::<MegaContext<&mut MemoryDatabase>>::contains(&map, address)
        };
        for address in satin_precompiles().addresses() {
            assert!(contains(address), "{address} is a precompile");
        }
        assert!(contains(&kzg_point_evaluation::ADDRESS));
        for address in [Address::ZERO, Address::repeat_byte(0xee), Address::with_last_byte(0x7f)] {
            assert!(!contains(&address), "{address} is not a precompile");
        }
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

    /// The BN254 pairing takes input up to the Karst bound, 57,600 bytes or 300 pairs of 192, and
    /// refuses a whole pair more.
    ///
    /// Both inputs are whole pairs of zeros, which pair to the identity, so the run itself accepts
    /// either: the 301st pair is refused by the bound alone. One stray byte past the bound would
    /// prove nothing, since a length that is not a whole number of pairs fails whatever the bound.
    ///
    /// Rule [S2.4]. Expected values `independent`: 57,600 bytes and 300 × 192 by hand, and the
    /// EIP-1108 price of 300 pairs, 45,000 + 300 × 34,000.
    #[test]
    fn test_bn254_pairing_is_bounded_at_the_karst_size() {
        assert_eq!(KARST_MAX_INPUT_SIZE, 57_600);
        let address = *op_revm::precompiles::bn254_pair::KARST.address();
        let pairs = |n: usize| Bytes::from(std::vec![0u8; n * 192]);
        let at_bound = run(address, pairs(300), 50_000_000);
        assert_eq!(at_bound.result, InstructionResult::Return);
        assert_eq!(at_bound.gas.total_gas_spent(), 45_000 + 300 * 34_000);

        let over = run(address, pairs(301), 50_000_000);
        assert_eq!(over.result, InstructionResult::PrecompileError, "a whole pair past the bound");
    }

    /// Every entry of the Satin set carries a price, op-revm's size-limited wrappers of the BN254
    /// pairing and the BLS12-381 G1 MSM, G2 MSM and pairing included, so gas detention decides a
    /// call to any of them from its price.
    ///
    /// Whether an entry has a price is the entry's, not the input's: `required_gas` answers `Some`
    /// exactly when the entry carries a price function, and that function answers a number for
    /// any input. So the empty input decides the cell, and the inputs beyond it — every length a
    /// precompile here reads in words or pairs, around those boundaries, of zeros and of `0xff`
    /// bytes, up to past the BN254 pairing's size limit — check that each price function answers
    /// them rather than failing on one.
    ///
    /// Rule [S12.47]. Expected values `independent`: a price is present, whatever the input.
    #[test]
    fn test_every_satin_entry_is_priced() {
        let lengths =
            [0, 1, 31, 32, 33, 64, 96, 128, 160, 192, 193, 256, 288, 384, 1_024, 57_600, 57_601];
        for precompile in satin_precompiles().inner().values() {
            for len in lengths {
                for byte in [0x00, 0xff] {
                    let input = std::vec![byte; len];
                    assert!(
                        precompile.required_gas(&input).is_some(),
                        "{:?} is priced at {len} bytes of {byte:#04x}",
                        precompile.id()
                    );
                }
            }
        }
    }

    /// op-revm's size-limited entries of the Satin set are priced by their EIPs up to their size
    /// limits, and at nothing past them, where the wrapper refuses the input before any gas check:
    ///
    /// - the BN254 pairing (EIP-1108): 45,000, and 34,000 per whole 192-byte pair, a stray byte
    ///   included, since the run checks the length after its gas;
    /// - the BLS12-381 G1 and G2 MSMs (EIP-2537): 12,000 and 22,500 per pair of 160 and 288 bytes,
    ///   discounted by the EIP's tables — per mille, 1,000 and 949 for one and two G1 pairs, 1,000
    ///   for one and two G2 pairs, and from 128 pairs on 519 and 524 — and nothing for a length
    ///   that is not a positive multiple of a pair, which the run refuses before its gas check;
    /// - the BLS12-381 pairing (EIP-2537): 37,700, and 32,600 per 384-byte pair, and nothing for a
    ///   length that is not a positive multiple of a pair.
    #[test]
    fn test_the_size_limited_entries_are_priced_by_their_eips() {
        use op_revm::precompiles::{bls12_381, bn254_pair};
        let price = |precompile: &revm::precompile::Precompile, len: usize| {
            let entry = satin_precompiles().get(precompile.address()).unwrap();
            assert_eq!(entry.id(), precompile.id(), "the Satin set dispatches the wrapper");
            entry.required_gas(&std::vec![0; len]).unwrap()
        };

        // The BN254 pairing: its 57,600 bytes are 300 pairs.
        let pairing = &bn254_pair::KARST;
        assert_eq!(bn254_pair::KARST_MAX_INPUT_SIZE, 300 * 192);
        for pairs in [0, 1, 2, 300] {
            let at = 45_000 + 34_000 * pairs as u64;
            assert_eq!(price(pairing, pairs * 192), at, "{pairs} pairs");
            if pairs < 300 {
                assert_eq!(price(pairing, pairs * 192 + 1), at, "{pairs} pairs and a byte");
            }
        }
        for len in [300 * 192 + 1, 301 * 192] {
            assert_eq!(price(pairing, len), 0, "{len} bytes: past the limit");
        }

        // The MSMs: the G1 limit is 1,806 pairs, the G2 limit 968.
        assert_eq!(bls12_381::JOVIAN_G1_MSM_MAX_INPUT_SIZE, 1_806 * 160);
        assert_eq!(bls12_381::JOVIAN_G2_MSM_MAX_INPUT_SIZE, 968 * 288);
        let msms = [
            (&bls12_381::JOVIAN_G1_MSM, 160, 12_000, [1_000, 949, 519], 1_806),
            (&bls12_381::JOVIAN_G2_MSM, 288, 22_500, [1_000, 1_000, 524], 968),
        ];
        for (msm, pair, base, [one, two, most], limit) in msms {
            let id = msm.id();
            for (pairs, discount) in [(1, one), (2, two), (128, most), (129, most), (limit, most)] {
                let at = pairs as u64 * base * discount / 1_000;
                assert_eq!(price(msm, pairs * pair), at, "{id:?}: {pairs} pairs");
            }
            for len in [0, pair - 1, pair + 1, limit * pair + 1, (limit + 1) * pair] {
                assert_eq!(price(msm, len), 0, "{id:?}: {len} bytes");
            }
        }

        // The BLS12-381 pairing: its 156,672 bytes are 408 pairs.
        let pairing = &bls12_381::JOVIAN_PAIRING;
        assert_eq!(bls12_381::JOVIAN_PAIRING_MAX_INPUT_SIZE, 408 * 384);
        for pairs in [1, 2, 408] {
            assert_eq!(price(pairing, pairs * 384), 37_700 + 32_600 * pairs as u64, "{pairs}");
        }
        for len in [0, 383, 385, 408 * 384 + 1, 409 * 384] {
            assert_eq!(price(pairing, len), 0, "{len} bytes");
        }
    }

    /// The KZG entry's price is its run's: below [`GAS_COST`](kzg_point_evaluation::GAS_COST) the
    /// run is out of gas, and from it on the run gives one result that uses exactly the price, a
    /// valid proof, a malformed input and an empty one alike.
    #[test]
    fn test_the_kzg_price_is_its_runs() {
        let kzg = &kzg_point_evaluation::KZG_POINT_EVALUATION;
        let mut wrong_proof = kzg_input().to_vec();
        wrong_proof[191] ^= 1;
        for input in
            [kzg_input(), Bytes::from(wrong_proof), Bytes::from_static(&[1; 100]), Bytes::new()]
        {
            let price = kzg.required_gas(&input);
            assert_eq!(price, Some(kzg_point_evaluation::GAS_COST));
            let price = kzg_point_evaluation::GAS_COST;
            for gas_limit in [0, price / 2, price - 1] {
                let result = run(kzg_point_evaluation::ADDRESS, input.clone(), gas_limit);
                assert_eq!(result.result, InstructionResult::PrecompileOOG, "{gas_limit}");
            }
            let at_price = run(kzg_point_evaluation::ADDRESS, input.clone(), price);
            assert_ne!(at_price.result, InstructionResult::PrecompileOOG);
            for gas_limit in [price + 1, 2 * price + 7, 30_000_000] {
                let result = run(kzg_point_evaluation::ADDRESS, input.clone(), gas_limit);
                assert_eq!(result.result, at_price.result, "{gas_limit}");
                assert_eq!(result.output, at_price.output, "{gas_limit}");
                if result.result.is_ok_or_revert() {
                    assert_eq!(result.gas.total_gas_spent(), price, "{gas_limit}");
                }
            }
        }
    }

    /// A Satin address is priced from the Satin table only while the entry the map dispatches
    /// there is still the table's: one a node replaced through the engine is not, whatever id its
    /// precompile carries, nor is one replaced through the map's own API under another id. An
    /// address outside the table is never priced; op-revm's wrapper of the BN254 pairing is, from
    /// its own entry.
    #[test]
    fn test_a_replaced_address_is_not_priced_from_the_satin_table() {
        let modexp = *revm::precompile::modexp::OSAKA.address();
        let input = [0_u8; 96];
        let own = |id: PrecompileId| {
            DynPrecompile::new(id, |input| {
                Ok(PrecompileOutput::new(1, Bytes::new(), input.reservoir))
            })
        };
        let untouched = satin_precompiles_map();
        let none = PricedPrecompiles::default();
        assert_eq!(none.price(&untouched, &modexp, &input), Some(500), "the Osaka minimum");
        assert_eq!(
            none.price(&untouched, &kzg_point_evaluation::ADDRESS, &input),
            Some(kzg_point_evaluation::GAS_COST)
        );

        // Replaced through the engine, even under the built-in's own id.
        let mut replaced = PricedPrecompiles::default();
        replaced.record_replaced(modexp);
        let mut map = satin_precompiles_map();
        map.apply_precompile(&modexp, |_| Some(own(PrecompileId::ModExp)));
        assert_eq!(replaced.price(&map, &modexp, &input), None);
        assert_eq!(
            replaced.price(&map, &kzg_point_evaluation::ADDRESS, &input),
            Some(kzg_point_evaluation::GAS_COST),
            "the other addresses are still the table's"
        );

        // Replaced through the map's own API, under another id.
        let mut map = satin_precompiles_map();
        map.apply_precompile(&kzg_point_evaluation::ADDRESS, |_| {
            Some(own(PrecompileId::Custom("own".into())))
        });
        assert_eq!(none.price(&map, &kzg_point_evaluation::ADDRESS, &input), None);
        assert_eq!(none.price(&map, &modexp, &input), Some(500));

        // A node's own address, and an address outside the table recorded as replaced.
        let own_address = Address::repeat_byte(0xd1);
        let mut map = satin_precompiles_map();
        map.apply_precompile(&own_address, |_| Some(own(PrecompileId::Custom("own".into()))));
        let mut recorded = PricedPrecompiles::default();
        recorded.record_replaced(own_address);
        assert_eq!(recorded.price(&map, &own_address, &input), None);
        assert_eq!(recorded.price(&map, &modexp, &input), Some(500));

        // op-revm's wrapper of the BN254 pairing: one pair, 45,000 and 34,000 per pair (EIP-1108).
        let pairing = *op_revm::precompiles::bn254_pair::KARST.address();
        assert!(untouched.get(&pairing).is_some());
        assert_eq!(none.price(&untouched, &pairing, &[0; 192]), Some(45_000 + 34_000));
    }

    /// A `ModExp` header of `base_len`, `exp_len` and `mod_len`, followed by the operands.
    fn modexp_input(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Bytes {
        let mut input = std::vec![0u8; 96];
        input[31] = base.len() as u8;
        input[63] = exponent.len() as u8;
        input[95] = modulus.len() as u8;
        input.extend_from_slice(base);
        input.extend_from_slice(exponent);
        input.extend_from_slice(modulus);
        Bytes::from(input)
    }

    /// `ModExp` is the Osaka entry: a one-byte exponentiation costs the Osaka 500-gas minimum,
    /// not the 200 of the Berlin entry Karst removed, and an exponentiation large enough for the
    /// EIP-7883 formula to bite is priced by the formula rather than by the minimum.
    #[test]
    fn test_modexp_is_priced_on_the_osaka_formula() {
        let address = *revm::precompile::modexp::OSAKA.address();

        // base = 3, exponent = 2, modulus = 5, one byte each: 3^2 mod 5 = 4.
        let small = run(address, modexp_input(&[3], &[2], &[5]), 100_000);
        assert_eq!(small.result, InstructionResult::Return);
        assert_eq!(small.output.as_ref(), &[4]);
        assert_eq!(small.gas.total_gas_spent(), 500, "the Osaka minimum");

        // 32-byte operands with a 32-byte exponent: the formula's cost is above the minimum.
        let thirty_two = |last: u8| {
            let mut operand = [0u8; 32];
            operand[31] = last;
            operand
        };
        let large =
            run(address, modexp_input(&thirty_two(3), &[0xffu8; 32], &thirty_two(7)), 1_000_000);
        assert_eq!(large.result, InstructionResult::Return);
        // EIP-7883 prices this as multiplication complexity (16, for operands of 32 bytes or
        // fewer) times the iteration count (255, one below the exponent's 256 bits), with no
        // divisor: 4,080. The Berlin entry Karst removed divided that by three.
        assert_eq!(large.gas.total_gas_spent(), 16 * 255);
        assert_eq!(large.gas.total_gas_spent(), 4_080);
    }
}
