//! A blockchain test's chain, imported block by block through Satin's block executor.
//!
//! Each block runs through [`MegaBlockExecutor`] — the pre-block system calls, the system-contract
//! deploys, every transaction and the end of the block — on a `MegaEvm` in the neutral
//! configuration of the Osaka fixtures (`Mode::Equivalence`), over a copy of the state the chain
//! holds. A block the executor accepts becomes the chain's head and its state the chain's; a block
//! it refuses is dropped with its copy, so the chain stays at the previous block.
//!
//! The executor needs the Satin fork's parameters; [`chain_spec`] supplies them with values that
//! bind nothing on an Osaka fixture, and says why for each.
//!
//! A Satin block adds accounts to the state an Ethereum block leaves: the system contracts the
//! executor deploys before every block, and the fee vaults op-revm routes fees to.
//! [`SATIN_ACCOUNTS`] lists them, each with the rule it is taken out of the post-state by before
//! the root is computed; any other account that differs is a failure.

use std::collections::{BTreeMap, BTreeSet};

use mega_evm::{
    alloy_consensus::transaction::{Recovered, SignerRecoverable},
    alloy_evm::block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder,
    alloy_primitives::Bloom,
    op_revm::constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    revm::{
        bytecode::Bytecode,
        context::BlockEnv,
        database::{CacheState, PlainAccount, State},
        primitives::{keccak256, Address, B256, KECCAK_EMPTY, U256},
        state::AccountInfo,
    },
    system::{
        keyless::KEYLESS_DEPLOY_ADDRESS, system_contract_specs, SequencerRegistryConfig,
        SystemContractSpec, ACCESS_CONTROL_ADDRESS, CREATE2_FACTORY_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, LIMIT_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaHardforkConfig, ProtocolLimits,
};

use super::fixture::DecodedBlock;
use crate::{
    exceptions,
    roots::{logs_bloom, receipts_root, state_root},
    types::blockchain::Account,
    Fork, Mode,
};

/// The fork whose neutral configuration the blocks run under.
pub const FORK: Fork = Fork::Osaka;

/// How many of the most recent block hashes `BLOCKHASH` can read.
const BLOCK_HASH_WINDOW: u64 = 256;

/// Why an account a Satin block adds is in the post-state, and so what it must hold to be taken
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// A contract the executor deploys before every block. It is taken out when it holds exactly
    /// what the deploy writes: the contract's code and nonce, its seeded storage, and no balance.
    Predeploy,
    /// The vault op-revm credits the base fee to, where Ethereum burns it. It is taken out when it
    /// holds exactly the base fees of the blocks the chain accepted, and nothing else.
    BaseFeeVault,
    /// A vault op-revm credits a fee the neutral configuration prices at nothing — the L1 data fee
    /// and the operator fee. It is taken out when it holds nothing; EIP-161 has then already
    /// removed it, since crediting nothing touches it.
    UnpaidFeeVault,
}

/// An account a Satin block adds to the state an Ethereum block leaves.
#[derive(Clone, Copy, Debug)]
pub struct SatinAccount {
    /// Its address.
    pub address: Address,
    /// Why it is there.
    pub added: Added,
    /// What puts it there.
    pub reason: &'static str,
}

/// The accounts a Satin block adds to the state an Ethereum block leaves.
///
/// One is taken out of the post-state before its root is computed only when the fixture's
/// pre-state does not hold it — an account the fixture put there is Ethereum's to compare — and
/// only when it holds exactly what Satin put there ([`Added`]). Anything else stays, and the root
/// it moves is a failure.
pub const SATIN_ACCOUNTS: [SatinAccount; 10] = [
    SatinAccount {
        address: ORACLE_CONTRACT_ADDRESS,
        added: Added::Predeploy,
        reason: "the Oracle, deployed before every block",
    },
    SatinAccount {
        address: HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        added: Added::Predeploy,
        reason: "the high-precision timestamp oracle, deployed before every block",
    },
    SatinAccount {
        address: KEYLESS_DEPLOY_ADDRESS,
        added: Added::Predeploy,
        reason: "KeylessDeploy, deployed before every block",
    },
    SatinAccount {
        address: ACCESS_CONTROL_ADDRESS,
        added: Added::Predeploy,
        reason: "MegaAccessControl, deployed before every block",
    },
    SatinAccount {
        address: LIMIT_CONTROL_ADDRESS,
        added: Added::Predeploy,
        reason: "MegaLimitControl, deployed before every block",
    },
    SatinAccount {
        address: SEQUENCER_REGISTRY_ADDRESS,
        added: Added::Predeploy,
        reason: "the SequencerRegistry, deployed with its seeded roles before every block",
    },
    SatinAccount {
        address: CREATE2_FACTORY_ADDRESS,
        added: Added::Predeploy,
        reason: "the EIP-7997 factory, deployed before every block",
    },
    SatinAccount {
        address: BASE_FEE_RECIPIENT,
        added: Added::BaseFeeVault,
        reason: "OP's base-fee vault: op-revm credits it the base fee Ethereum burns",
    },
    SatinAccount {
        address: L1_FEE_RECIPIENT,
        added: Added::UnpaidFeeVault,
        reason: "OP's L1-fee vault: op-revm credits it the L1 data fee, nothing here",
    },
    SatinAccount {
        address: OPERATOR_FEE_RECIPIENT,
        added: Added::UnpaidFeeVault,
        reason: "OP's operator-fee vault: op-revm credits it the operator fee, nothing here",
    },
];

/// The `SequencerRegistry` roles the executor seeds the registry with.
///
/// They bind nothing on a fixture: the registry's system address is the only one whose
/// transactions are promoted, and only a legacy call to one of `MegaETH`'s whitelisted contracts
/// is, which no Ethereum fixture makes; no role change is ever pending, so no block makes the
/// `applyPendingChanges()` call.
pub fn registry_config() -> SequencerRegistryConfig {
    SequencerRegistryConfig::placeholder()
}

/// The schedule the blocks run on: every fork active from genesis, with the Satin fork's two
/// parameters the executor requires.
///
/// - The `SequencerRegistry` roles are [`registry_config`]'s.
/// - The protocol limits are the loosest a chain may carry ([`ProtocolLimits::loosest`]). The
///   data-size, KV and state-gas limits, per transaction, per frame and per block, and the block's
///   execution-gas budget are unlimited. Gas detention's two caps are one below the most compute a
///   transaction can spend under Satin's own 200,000,000 execution cap; under the neutral Osaka
///   configuration EIP-7825 caps a transaction's gas at 2^24, so no transaction's compute comes
///   near a cap, and a detained transaction runs exactly as it would undetained.
///
/// The executor installs those per-transaction limits before every transaction, whatever the EVM
/// carried; the limits the gate's state-test runner installs (`no_limits()`) are not ones a chain
/// may carry.
pub fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with_all_activated()
        .with_params(registry_config())
        .with_params(ProtocolLimits::loosest())
}

/// The fixture's `account` as the state holds it: its code hash computed from its code, and its
/// code served by hash, as a node's state serves it. An `AccountInfo` that carried no code would
/// read as an account without any (`AccountInfo::default` carries the empty code). A nonce wider
/// than 64 bits is a fixture error rather than a value clamped to fit.
pub(crate) fn plain_account(account: &Account) -> Result<PlainAccount, String> {
    let nonce = u64::try_from(account.nonce)
        .map_err(|_| format!("nonce {} does not fit a u64", account.nonce))?;
    let code_hash = if account.code.is_empty() { KECCAK_EMPTY } else { keccak256(&account.code) };
    Ok(PlainAccount {
        info: AccountInfo {
            balance: account.balance,
            nonce,
            code_hash,
            code: None,
            ..Default::default()
        },
        storage: account.storage.iter().map(|(slot, value)| (*slot, *value)).collect(),
    })
}

/// The root of a state the fixture writes out, such as its post-state.
pub(crate) fn fixture_state_root(accounts: &BTreeMap<Address, Account>) -> Result<B256, String> {
    let plain = accounts
        .iter()
        .map(|(address, account)| Ok((*address, plain_account(account)?)))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(state_root(plain.iter().map(|(address, account)| (*address, account))))
}

/// What a block the executor accepted produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockOutput {
    /// The gas the block used.
    pub(crate) gas_used: u64,
    /// The root of its receipts.
    pub(crate) receipts_root: B256,
    /// The bloom of its logs.
    pub(crate) logs_bloom: Bloom,
    /// The root of the state it left, the accounts Satin added taken out.
    pub(crate) state_root: B256,
}

/// Why the executor, or the node around it, refused a block.
#[derive(Debug)]
pub(crate) struct Refusal {
    /// The execution-spec exception names the refusal satisfies, without their prefix; empty for
    /// one no name describes.
    pub(crate) names: &'static [&'static str],
    /// The refusal as the executor reported it.
    pub(crate) detail: String,
}

/// A test's chain: the state its head left, and the hashes `BLOCKHASH` reads.
pub(crate) struct Chain {
    /// The state the head block left, the accounts Satin added included.
    state: CacheState,
    /// The hashes of the chain's blocks, by number.
    hashes: BTreeMap<u64, B256>,
    /// The head block's hash.
    pub(crate) head: B256,
    /// The base fees of the blocks the chain accepted: what the base-fee vault holds.
    routed_base_fee: U256,
    /// The accounts the fixture's pre-state holds.
    pre: BTreeSet<Address>,
    /// The chain id.
    chain_id: u64,
    /// The fraction the blob base fee is updated by.
    blob_base_fee_update_fraction: u64,
    /// The schedule the blocks run on.
    spec: MegaHardforkConfig,
    /// What the executor deploys before every block, by address.
    predeploys: BTreeMap<Address, SystemContractSpec>,
}

impl Chain {
    /// The chain at its genesis block, numbered and hashed `genesis`, holding `pre`.
    pub(crate) fn new(
        pre: &BTreeMap<Address, Account>,
        genesis: (u64, B256),
        chain_id: u64,
        blob_base_fee_update_fraction: u64,
    ) -> Result<Self, String> {
        let mut state = CacheState::new();
        for (address, account) in pre {
            let plain = plain_account(account).map_err(|error| format!("{address}: {error}"))?;
            if !account.code.is_empty() {
                state
                    .contracts
                    .insert(plain.info.code_hash, Bytecode::new_raw(account.code.clone()));
            }
            state.insert_account_with_storage(*address, plain.info, plain.storage);
        }
        Ok(Self {
            state,
            hashes: BTreeMap::from([genesis]),
            head: genesis.1,
            routed_base_fee: U256::ZERO,
            pre: pre.keys().copied().collect(),
            chain_id,
            blob_base_fee_update_fraction,
            spec: chain_spec(),
            predeploys: system_contract_specs(&registry_config())
                .into_iter()
                .map(|spec| (spec.address, spec))
                .collect(),
        })
    }

    /// The root of the state the chain holds, the accounts Satin added taken out.
    pub(crate) fn state_root(&self) -> B256 {
        state_root(
            self.state
                .trie_account()
                .into_iter()
                .filter(|(address, account)| !self.added_by_satin(*address, account)),
        )
    }

    /// Whether the account at `address` is one a Satin block added, holding exactly what Satin put
    /// there.
    fn added_by_satin(&self, address: Address, account: &PlainAccount) -> bool {
        if self.pre.contains(&address) {
            return false;
        }
        let Some(added) = SATIN_ACCOUNTS.iter().find(|a| a.address == address).map(|a| a.added)
        else {
            return false;
        };
        let storage: BTreeMap<_, _> =
            account.storage.iter().filter(|(_, value)| !value.is_zero()).collect();
        let info = &account.info;
        match added {
            Added::Predeploy => self.predeploys.get(&address).is_some_and(|spec| {
                let seed: BTreeMap<_, _> = spec
                    .seed
                    .iter()
                    .filter(|(_, value)| !value.is_zero())
                    .map(|(slot, value)| (slot, value))
                    .collect();
                info.nonce == spec.nonce &&
                    info.balance.is_zero() &&
                    info.code_hash == spec.code_hash &&
                    storage == seed
            }),
            Added::BaseFeeVault => {
                info.balance == self.routed_base_fee &&
                    info.nonce == 0 &&
                    info.code_hash == KECCAK_EMPTY &&
                    storage.is_empty()
            }
            Added::UnpaidFeeVault => {
                info.balance.is_zero() &&
                    info.nonce == 0 &&
                    info.code_hash == KECCAK_EMPTY &&
                    storage.is_empty()
            }
        }
    }

    /// The environment of `block`, from its header.
    fn block_env(&self, block: &DecodedBlock) -> BlockEnv {
        let header = &block.header;
        let mut env = BlockEnv {
            number: U256::from(header.number),
            beneficiary: header.beneficiary,
            timestamp: U256::from(header.timestamp),
            gas_limit: header.gas_limit,
            basefee: header.base_fee_per_gas.unwrap_or_default(),
            difficulty: header.difficulty,
            prevrandao: Some(header.mix_hash),
            ..Default::default()
        };
        if let Some(excess) = header.excess_blob_gas {
            env.set_blob_excess_gas_and_price(excess, self.blob_base_fee_update_fraction);
        }
        env
    }

    /// Imports `block`: runs it through the executor on a copy of the chain's state, and, when the
    /// executor accepts it, makes it the head and answers what it produced. A refused block leaves
    /// the chain as it was.
    pub(crate) fn import(&mut self, block: &DecodedBlock) -> Result<BlockOutput, Refusal> {
        let header = &block.header;
        let mut state = State::builder().with_cached_prestate(self.state.clone()).build();
        let window = header.number.saturating_sub(BLOCK_HASH_WINDOW);
        state.block_hashes.extend(self.hashes.range(window..).map(|(n, hash)| (*n, *hash)));

        let evm = Mode::Equivalence.evm(FORK, &mut state, self.block_env(block), self.chain_id);
        let ctx = MegaBlockExecutionCtx::new(
            header.parent_hash,
            header.parent_beacon_block_root,
            header.extra_data.clone(),
            BlockLimits::no_limits(),
        );
        let mut executor =
            MegaBlockExecutor::new(evm, ctx, self.spec.clone(), OpAlloyReceiptBuilder::default());
        executor.apply_pre_execution_changes().map_err(refusal)?;
        for tx in &block.body.transactions {
            let signer = tx.recover_signer().map_err(|error| Refusal {
                names: &["INVALID_SIGNATURE_VRS"],
                detail: format!("the signature recovers no sender: {error}"),
            })?;
            executor.execute_transaction(&Recovered::new_unchecked(tx, signer)).map_err(refusal)?;
        }
        let (_, result) = executor.finish().map_err(refusal)?;

        let routed_base_fee = self.routed_base_fee +
            U256::from(header.base_fee_per_gas.unwrap_or_default()) * U256::from(result.gas_used);
        self.state = state.cache;
        self.routed_base_fee = routed_base_fee;
        self.head = header.hash_slow();
        self.hashes.insert(header.number, self.head);
        Ok(BlockOutput {
            gas_used: result.gas_used,
            receipts_root: receipts_root(&result.receipts),
            logs_bloom: logs_bloom(&result.receipts),
            state_root: self.state_root(),
        })
    }
}

/// The exception names a refusal of the executor satisfies.
fn refusal(error: BlockExecutionError) -> Refusal {
    let names = match &error {
        BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) => {
            error.as_invalid_tx_err().map_or(&[][..], |error| exceptions::names(FORK, error))
        }
        BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        ) => &["GAS_ALLOWANCE_EXCEEDED"],
        _ => &[],
    };
    Refusal { names, detail: error.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removal list names every predeploy the executor deploys and the three fee vaults, each
    /// once.
    #[test]
    fn test_the_list_names_every_account_a_satin_block_adds() {
        let listed: BTreeSet<_> = SATIN_ACCOUNTS.iter().map(|a| a.address).collect();
        assert_eq!(listed.len(), SATIN_ACCOUNTS.len(), "an address listed twice");
        let predeploys: BTreeSet<_> =
            system_contract_specs(&registry_config()).iter().map(|spec| spec.address).collect();
        let listed_predeploys: BTreeSet<_> = SATIN_ACCOUNTS
            .iter()
            .filter(|a| a.added == Added::Predeploy)
            .map(|a| a.address)
            .collect();
        assert_eq!(listed_predeploys, predeploys);
        let vaults: BTreeSet<_> = SATIN_ACCOUNTS
            .iter()
            .filter(|a| a.added != Added::Predeploy)
            .map(|a| a.address)
            .collect();
        assert_eq!(
            vaults,
            BTreeSet::from([BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT])
        );
    }

    /// The parameters the chain runs on are ones a chain may carry: the executor refuses a block
    /// under any other.
    #[test]
    fn test_the_chain_spec_carries_valid_parameters() {
        use mega_evm::{HardforkParams, MegaHardforks};
        let spec = chain_spec();
        let limits = spec.protocol_limits(0).expect("limits from genesis");
        assert_eq!(limits, ProtocolLimits::loosest());
        assert!(limits.validate().is_ok());
        assert!(spec.fork_params::<SequencerRegistryConfig>().is_some());
        assert!(spec.validate_schedule().is_ok());
    }
}
