//! A blockchain test's chain, imported block by block through Satin's block executor.
//!
//! Each block runs through [`MegaBlockExecutor`] — the pre-block system calls, every transaction
//! and the end of the block — on a `MegaEvm` in the neutral configuration of the Osaka fixtures
//! (`Mode::Equivalence`), over a copy of the state the chain holds. A block the executor accepts
//! becomes the chain's head and its state the chain's; a block it refuses is dropped with its copy,
//! so the chain stays at the previous block.
//!
//! Satin runs on a chain that ran the legacy forks, which holds the six system contracts, and OP's
//! EIP-7997 factory, and deploys none of them; the runner seeds each of the seven the fixture's
//! pre-state lacks into the chain's genesis state, once, as such a chain holds them
//! ([`rex6_predeploys`]), the `SequencerRegistry` with roles that bind nothing on an Osaka fixture
//! ([`registry_roles`], which says why).
//!
//! Each block's context tells the executor whether the block admits only deposits and whether
//! Satin activates in it, worked out as a node works them out: from the schedule and the timestamp
//! of the chain's head, the block's parent. Under [`chain_spec`], which activates every fork at
//! genesis, no block the runner imports is either, so no block checks the predeploys.
//!
//! A Satin chain holds accounts an Ethereum chain does not: the seven predeploys, which Satin
//! requires and does not create, so the chain's pre-state holds them; and the fee vaults op-revm
//! routes fees to. [`SATIN_ACCOUNTS`] lists them, each with the rule it is taken out of the
//! post-state by before the root is computed; any other account that differs is a failure.

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
        keyless::KEYLESS_DEPLOY_ADDRESS, ACCESS_CONTROL_ADDRESS, CREATE2_FACTORY_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS, LIMIT_CONTROL_ADDRESS, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
    },
    test_utils::{rex6_predeploys, SequencerRegistryRoles},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaHardforkConfig, MegaHardforks,
    ProtocolLimits,
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

/// Why an account a Satin chain holds is in the post-state, and so what it must hold to be taken
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    /// A predeploy, which Satin requires and neither creates nor upgrades: the chain's pre-state
    /// holds it as a chain that ran Rex6 holds it ([`rex6_predeploys`]), the `SequencerRegistry`
    /// with the roles of [`registry_roles`]. It is taken out when it holds exactly that: the code,
    /// the nonce, the storage, and no balance.
    PreState,
    /// The vault op-revm credits the base fee to, where Ethereum burns it. It is taken out when it
    /// holds exactly the base fees of the blocks the chain accepted, and nothing else.
    BaseFeeVault,
    /// A vault op-revm credits a fee the neutral configuration prices at nothing — the L1 data fee
    /// and the operator fee. It is taken out when it holds nothing; EIP-161 has then already
    /// removed it, since crediting nothing touches it.
    UnpaidFeeVault,
}

/// An account a Satin chain holds that an Ethereum chain does not.
#[derive(Clone, Copy, Debug)]
pub struct SatinAccount {
    /// Its address.
    pub address: Address,
    /// Why it is there.
    pub added: Added,
    /// What puts it there.
    pub reason: &'static str,
}

/// The accounts a Satin chain holds that an Ethereum chain does not: the one its pre-state holds
/// for Satin, and the ones a Satin block adds to the state an Ethereum block leaves.
///
/// One is taken out of the post-state before its root is computed only when the fixture's
/// pre-state does not hold it — an account the fixture put there is Ethereum's to compare — and
/// only when it holds exactly what Satin put there ([`Added`]). Anything else stays, and the root
/// it moves is a failure.
pub const SATIN_ACCOUNTS: [SatinAccount; 10] = [
    SatinAccount {
        address: ORACLE_CONTRACT_ADDRESS,
        added: Added::PreState,
        reason: "the Oracle, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        added: Added::PreState,
        reason: "the high-precision timestamp oracle, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: KEYLESS_DEPLOY_ADDRESS,
        added: Added::PreState,
        reason: "KeylessDeploy, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: ACCESS_CONTROL_ADDRESS,
        added: Added::PreState,
        reason: "MegaAccessControl, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: LIMIT_CONTROL_ADDRESS,
        added: Added::PreState,
        reason: "MegaLimitControl, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: SEQUENCER_REGISTRY_ADDRESS,
        added: Added::PreState,
        reason: "the SequencerRegistry, a predeploy the chain's pre-state holds",
    },
    SatinAccount {
        address: CREATE2_FACTORY_ADDRESS,
        added: Added::PreState,
        reason: "the EIP-7997 factory, a predeploy the chain's pre-state holds",
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

/// The roles of the `SequencerRegistry` the chain's pre-state holds: [`MEGA_SYSTEM_ADDRESS`] in
/// every one.
///
/// They bind nothing on a fixture: the registry's system address is the only one whose
/// transactions are promoted, and only a legacy call to one of `MegaETH`'s whitelisted contracts
/// is, which no Ethereum fixture makes; no role change is ever pending, so no block makes the
/// `applyPendingChanges()` call.
pub fn registry_roles() -> SequencerRegistryRoles {
    SequencerRegistryRoles::from(MEGA_SYSTEM_ADDRESS)
}

/// The schedule the blocks run on: every fork active from genesis.
pub fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default().with_all_activated()
}

/// The limits the blocks are held to in place of Satin's: none ([`ProtocolLimits::no_limits`]).
///
/// Equivalence mode takes out what `MegaETH` sets and Ethereum does not, its limits among them,
/// as the gate's state-test runner does (`EvmTxRuntimeLimits::no_limits()`): no data-size limit,
/// per transaction, per frame or per block, and no gas detention. The executor installs the
/// per-transaction half before every transaction, whatever the EVM carried.
pub(crate) fn limits() -> ProtocolLimits {
    ProtocolLimits::no_limits()
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
    /// The head block's timestamp: the parent's of the next block.
    head_timestamp: u64,
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
}

impl Chain {
    /// The chain at its genesis block, numbered and hashed `genesis` and stamped
    /// `genesis_timestamp`, holding `pre` and every predeploy Satin requires whose address `pre`
    /// does not hold ([`Added::PreState`]).
    pub(crate) fn new(
        pre: &BTreeMap<Address, Account>,
        genesis: (u64, B256),
        genesis_timestamp: u64,
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
        for (address, info, storage) in rex6_predeploys(registry_roles()) {
            if pre.contains_key(&address) {
                continue;
            }
            if let Some(code) = &info.code {
                state.contracts.insert(info.code_hash, code.clone());
            }
            let storage = storage.into_iter().filter(|(_, value)| !value.is_zero()).collect();
            state.insert_account_with_storage(address, info, storage);
        }
        Ok(Self {
            state,
            hashes: BTreeMap::from([genesis]),
            head: genesis.1,
            head_timestamp: genesis_timestamp,
            routed_base_fee: U256::ZERO,
            pre: pre.keys().copied().collect(),
            chain_id,
            blob_base_fee_update_fraction,
            spec: chain_spec(),
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
            Added::PreState => {
                rex6_predeploys(registry_roles()).into_iter().any(|(predeploy, held, slots)| {
                    let held_storage: BTreeMap<_, _> = slots
                        .iter()
                        .filter(|(_, value)| !value.is_zero())
                        .map(|(slot, value)| (slot, value))
                        .collect();
                    predeploy == address &&
                        info.nonce == held.nonce &&
                        info.balance == held.balance &&
                        info.code_hash == held.code_hash &&
                        storage == held_storage
                })
            }
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
        let only_deposits = self.spec.admits_only_deposits(self.head_timestamp, header.timestamp);
        let activates_satin =
            self.spec.is_satin_activation_block(self.head_timestamp, header.timestamp);
        let ctx = MegaBlockExecutionCtx::new(
            header.parent_hash,
            header.parent_beacon_block_root,
            header.extra_data.clone(),
            BlockLimits::no_limits(),
        )
        .with_no_user_tx_activation_block(only_deposits)
        .with_satin_activation_block(activates_satin);
        let mut executor =
            MegaBlockExecutor::new(evm, ctx, self.spec.clone(), OpAlloyReceiptBuilder::default())
                .with_protocol_limits(limits());
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
        self.head_timestamp = header.timestamp;
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

    /// The removal list names every predeploy the pre-state holds and the three fee vaults, each
    /// once.
    #[test]
    fn test_the_list_names_every_account_a_satin_chain_adds() {
        let listed: BTreeSet<_> = SATIN_ACCOUNTS.iter().map(|a| a.address).collect();
        assert_eq!(listed.len(), SATIN_ACCOUNTS.len(), "an address listed twice");
        let listed_as = |added: Added| -> BTreeSet<_> {
            SATIN_ACCOUNTS.iter().filter(|a| a.added == added).map(|a| a.address).collect()
        };
        let predeploys: BTreeSet<_> =
            rex6_predeploys(registry_roles()).into_iter().map(|(address, ..)| address).collect();
        assert_eq!(listed_as(Added::PreState), predeploys);
        let vaults: BTreeSet<_> = SATIN_ACCOUNTS
            .iter()
            .filter(|a| a.added != Added::PreState)
            .map(|a| a.address)
            .collect();
        assert_eq!(
            vaults,
            BTreeSet::from([BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT])
        );
    }

    /// [S10] The schedule the chain runs on loads, and the blocks are held to no limit:
    /// equivalence mode takes out every one `MegaETH` sets.
    ///
    /// Expected values: constants (no limit at all).
    #[test]
    fn test_the_chain_spec_loads_and_the_blocks_are_held_to_no_limit() {
        let spec = chain_spec();
        assert!(spec.validate_schedule().is_ok());
        assert_eq!(limits(), ProtocolLimits::no_limits());
        assert_eq!(limits().tx_runtime_limits, mega_evm::EvmTxRuntimeLimits::no_limits());
        assert_eq!(limits().block_txs_data_limit, u64::MAX);
    }

    /// [S1.8] [S21.12] [S1] A block's context says whether it admits only deposits, from the
    /// schedule and the head's timestamp, as a node works it out. Under a schedule that activates
    /// Satin at the block's timestamp, with the genesis a second earlier, the block is Satin's
    /// activation block: a user transaction in it is refused, and the block without it is
    /// imported. The next block, whose parent is that block, admits the transaction. Under the
    /// runner's own schedule every fork is active from genesis, so no block it imports is an
    /// activation block.
    ///
    /// Expected values: independent (the schedule's own timestamps and the rule).
    #[test]
    fn test_an_imported_block_is_told_whether_it_admits_only_deposits() {
        use mega_evm::{
            alloy_consensus::{BlockBody, Header, Signed, TxLegacy},
            alloy_hardforks::ForkCondition,
            alloy_primitives::{Bytes, Signature, TxKind},
            MegaHardfork, MegaTxEnvelope,
        };

        const ACTIVATION: u64 = 1_000;
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 10,
            gas_limit: 100_000,
            to: TxKind::Call(Address::repeat_byte(0xc0)),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature =
            Signature::new(U256::from(0x23f5_c4da_u64), U256::from(0x42f9_035f_u64), false);
        let tx = MegaTxEnvelope::Legacy(Signed::new_unhashed(tx, signature));
        let sender = tx.recover_signer().expect("the signature recovers a sender");
        let funded = Account { balance: U256::from(10).pow(U256::from(18)), ..Default::default() };
        let pre = BTreeMap::from([(sender, funded)]);
        let block = |number: u64, timestamp: u64, transactions: Vec<MegaTxEnvelope>| {
            let header = Header {
                number,
                timestamp,
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(7),
                parent_beacon_block_root: Some(B256::ZERO),
                excess_blob_gas: Some(0),
                ..Default::default()
            };
            DecodedBlock::new(header, BlockBody { transactions, ommers: vec![], withdrawals: None })
        };
        let satin_at_activation = MegaHardforkConfig::default()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(ACTIVATION));

        let mut chain = Chain::new(&pre, (0, B256::ZERO), ACTIVATION - 1, 1, 5_007_716).unwrap();
        chain.spec = satin_at_activation;
        let refusal = chain
            .import(&block(1, ACTIVATION, vec![tx.clone()]))
            .expect_err("a user transaction in Satin's activation block");
        assert!(
            refusal.detail.contains("non-deposit transaction in fork activation block"),
            "{}",
            refusal.detail
        );
        chain.import(&block(1, ACTIVATION, vec![])).expect("the activation block, without it");
        chain.import(&block(2, ACTIVATION + 1, vec![tx])).expect("the block after admits it");

        assert!(
            !chain_spec().admits_only_deposits(0, u64::MAX),
            "under the runner's schedule no parent and no block make an activation block"
        );
    }
}
