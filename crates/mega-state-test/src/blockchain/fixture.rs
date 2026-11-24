//! A blockchain test as the execution-spec fixtures write it, read only as far as the runner
//! judges it.
//!
//! A fixture file maps test names to tests; a test carries its pre-state, its genesis block, its
//! blocks, the post-state its last valid block leaves and the hash of that block. Every block is
//! carried as its RLP, which is what the runner executes: an invalid block has no decoded header
//! or transactions beside it, only `rlp_decoded`, which may itself be absent when the RLP does not
//! decode. The decoded transactions and withdrawals are read only to decide whether a test is
//! skipped, before anything runs.

use std::collections::BTreeMap;

use mega_evm::{
    alloy_consensus::{Block, Header},
    revm::primitives::{Address, Bytes, B256, U256},
    MegaTxEnvelope,
};
use serde::{
    de::{Error as _, IgnoredAny, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::value::RawValue;

use crate::types::{blockchain::Account, deserialize_maybe_empty};

/// The network name of the entries the runner executes.
pub const NETWORK: &str = "Osaka";

/// A fixture file's tests, each still unparsed, so that only the entries of [`NETWORK`] are read
/// in full. A name that appears twice is an error rather than one test silently replacing the
/// other.
pub(crate) struct Suite<'a>(pub(crate) BTreeMap<String, &'a RawValue>);

impl<'de: 'a, 'a> Deserialize<'de> for Suite<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Tests<'a>(core::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for Tests<'a> {
            type Value = Suite<'a>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a map of test names to tests")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut tests = BTreeMap::new();
                while let Some(name) = map.next_key::<String>()? {
                    let test: &'de RawValue = map.next_value()?;
                    if tests.insert(name.clone(), test).is_some() {
                        return Err(A::Error::custom(format!("test {name:?} appears twice")));
                    }
                }
                Ok(Suite(tests))
            }
        }
        deserializer.deserialize_map(Tests(core::marker::PhantomData))
    }
}

/// The network a test is filled for, read without parsing the rest of it.
#[derive(Deserialize)]
pub(crate) struct Network {
    pub(crate) network: String,
}

/// A blockchain test.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Test {
    /// The RLP of the genesis block, whose state root is the pre-state's.
    #[serde(rename = "genesisRLP")]
    pub(crate) genesis_rlp: Bytes,
    /// The blocks, in the order they are imported.
    pub(crate) blocks: Vec<FixtureBlock>,
    /// The state the genesis block holds.
    pub(crate) pre: BTreeMap<Address, Account>,
    /// The state the last valid block leaves.
    pub(crate) post_state: BTreeMap<Address, Account>,
    /// The hash of the last valid block.
    pub(crate) lastblockhash: B256,
    /// The chain the test runs on.
    pub(crate) config: ChainConfig,
}

/// The chain configuration a test carries.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChainConfig {
    /// The chain id.
    pub(crate) chainid: U256,
    /// The blob fee parameters of each fork, by fork name.
    pub(crate) blob_schedule: BTreeMap<String, BlobParams>,
}

/// One fork's blob fee parameters.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BlobParams {
    /// The fraction the blob base fee is updated by.
    pub(crate) base_fee_update_fraction: U256,
}

/// A block of a test.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FixtureBlock {
    /// The block's RLP.
    pub(crate) rlp: Bytes,
    /// The exception a node raises importing the block, as one name or alternatives joined by
    /// `|`; `None` for a valid block.
    pub(crate) expect_exception: Option<String>,
    /// The block's transactions, decoded, for a valid block.
    #[serde(default)]
    pub(crate) transactions: Vec<FixtureTx>,
    /// The block's withdrawals, for a valid block.
    #[serde(default)]
    pub(crate) withdrawals: Vec<IgnoredAny>,
    /// The block decoded from its RLP, for an invalid block whose RLP decodes.
    #[serde(rename = "rlp_decoded")]
    pub(crate) rlp_decoded: Option<Decoded>,
}

/// An invalid block's body, decoded.
#[derive(Debug, Deserialize)]
pub(crate) struct Decoded {
    /// The block's transactions.
    #[serde(default)]
    pub(crate) transactions: Vec<FixtureTx>,
    /// The block's withdrawals.
    #[serde(default)]
    pub(crate) withdrawals: Vec<IgnoredAny>,
}

/// A decoded transaction, read for what decides whether its test is skipped.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FixtureTx {
    /// The transaction's EIP-2718 type; `None` for a legacy transaction written without one.
    #[serde(rename = "type")]
    pub(crate) ty: Option<U256>,
    /// The recipient; `None` for a creation.
    #[serde(default, deserialize_with = "deserialize_maybe_empty")]
    pub(crate) to: Option<Address>,
    /// The gas price of a legacy or EIP-2930 transaction.
    pub(crate) gas_price: Option<U256>,
    /// The fee cap of a dynamic-fee transaction.
    pub(crate) max_fee_per_gas: Option<U256>,
    /// The priority fee cap of a dynamic-fee transaction.
    pub(crate) max_priority_fee_per_gas: Option<U256>,
}

impl FixtureBlock {
    /// The block's transactions as the fixture decodes them, the invalid block's included.
    pub(crate) fn transactions(&self) -> impl Iterator<Item = &FixtureTx> {
        self.transactions
            .iter()
            .chain(self.rlp_decoded.iter().flat_map(|decoded| decoded.transactions.iter()))
    }

    /// Whether the block carries a withdrawal, valid or not.
    pub(crate) fn has_withdrawals(&self) -> bool {
        !self.withdrawals.is_empty() ||
            self.rlp_decoded.as_ref().is_some_and(|decoded| !decoded.withdrawals.is_empty())
    }

    /// The names the block's expected exception gives, without their alternatives' separator;
    /// empty for a valid block.
    pub(crate) fn exception_names(&self) -> impl Iterator<Item = &str> {
        self.expect_exception.iter().flat_map(|names| names.split('|').map(str::trim))
    }
}

/// A block decoded from its RLP: its header and its transactions as an OP chain carries them.
pub(crate) type DecodedBlock = Block<MegaTxEnvelope, Header>;

/// Decodes the block `rlp` holds, which must be exactly one block.
pub(crate) fn decode_block(rlp: &[u8]) -> Result<DecodedBlock, String> {
    let mut buf = rlp;
    let block = <DecodedBlock as alloy_rlp::Decodable>::decode(&mut buf)
        .map_err(|error| format!("the block's RLP does not decode: {error}"))?;
    if !buf.is_empty() {
        return Err(format!("{} bytes follow the block's RLP", buf.len()));
    }
    Ok(block)
}
