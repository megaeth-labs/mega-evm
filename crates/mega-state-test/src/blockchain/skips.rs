//! The blockchain tests the runner does not execute, and why.
//!
//! Every skip is decided from the test's own content before anything runs — its name, its blocks'
//! withdrawals and transactions as the fixture decodes them, and the exceptions its blocks expect
//! — never from how it fares. Each class is a part of Ethereum's block an OP chain does not have,
//! or a check a node makes on a block before it hands the block to an executor.
//!
//! The pre-block system calls are not skipped: the executor makes the EIP-2935 and EIP-4788 calls
//! before every block, and what they write is compared with everything else.

use mega_evm::revm::primitives::U256;
use serde::Serialize;

use super::fixture::{FixtureBlock, FixtureTx, Test};

/// Why a blockchain test is not executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// A block carries withdrawals: an OP chain has no beacon-chain withdrawals, and its blocks
    /// process none.
    Withdrawals,
    /// A block carries a blob transaction (type 3), or expects an exception only a blob
    /// transaction raises: an OP chain has no blob transactions.
    BlobTransactions,
    /// The test is about the execution-layer requests of EIP-7685 — EIP-7002 withdrawal requests
    /// and EIP-7251 consolidation requests, which Ethereum's post-block system calls dequeue — or
    /// a block expects an exception about requests: an OP chain makes no requests and no
    /// post-block system call.
    Requests,
    /// A block expects a `BlockException` for its header or body — its gas limit, base fee, blob
    /// gas fields, size, encoding, withdrawals root or hash: a consensus check a node makes before
    /// it executes the block, not one the block executor makes.
    HeaderOrBody,
    /// A block the fixture expects to be invalid carries a transaction an OP block cannot encode —
    /// an EIP-7702 transaction without a recipient, or a fee wider than 128 bits — so its RLP does
    /// not decode and no executor sees it. The state-test gate skips the same transactions as
    /// unbuildable.
    UndecodableInvalidTransaction,
    /// The test creates an account at an address whose pre-state holds storage and nothing else
    /// (EIP-7610). revm reads storage slot by slot and never asks the database whether an account
    /// has any, so it cannot see the collision. The state-test gate skips the same files, by the
    /// same list (`crate::skips::skip_file`).
    CreateCollisionWithStorage,
}

impl SkipReason {
    /// Every reason, in the order the runner tests them.
    pub const ALL: [Self; 6] = [
        Self::Withdrawals,
        Self::BlobTransactions,
        Self::Requests,
        Self::HeaderOrBody,
        Self::UndecodableInvalidTransaction,
        Self::CreateCollisionWithStorage,
    ];

    /// The reason's name, as the summary prints it and the command line takes it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Withdrawals => "withdrawals",
            Self::BlobTransactions => "blob-transactions",
            Self::Requests => "requests",
            Self::HeaderOrBody => "header-or-body",
            Self::UndecodableInvalidTransaction => "undecodable-invalid-transaction",
            Self::CreateCollisionWithStorage => "create-collision-with-storage",
        }
    }
}

impl core::str::FromStr for SkipReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|reason| reason.name() == s).ok_or_else(|| {
            let names: Vec<_> = Self::ALL.iter().map(|reason| reason.name()).collect();
            format!("unknown skip reason {s:?}; expected one of: {}", names.join(", "))
        })
    }
}

/// The directories of the request tests, as the test names carry them.
const REQUEST_TESTS: [&str; 3] = [
    "tests/prague/eip7002_el_triggerable_withdrawals/",
    "tests/prague/eip7251_consolidations/",
    "tests/prague/eip7685_general_purpose_el_requests/",
];

/// The exceptions only a blob transaction raises, beyond the `TYPE_3_` ones.
const BLOB_EXCEPTIONS: [&str; 1] = ["TransactionException.INSUFFICIENT_MAX_FEE_PER_BLOB_GAS"];

/// The block exceptions about requests: the header's requests hash, the layout of a deposit event
/// a request is parsed from, and the post-block system call that dequeues requests.
const REQUEST_EXCEPTIONS: [&str; 3] = [
    "BlockException.INVALID_REQUESTS",
    "BlockException.INVALID_DEPOSIT_EVENT_LAYOUT",
    "BlockException.SYSTEM_CONTRACT_CALL_FAILED",
];

/// The block exceptions for a block's header or body that a node checks before execution.
///
/// The list names what the pinned release expects; a block exception outside it, and outside
/// [`REQUEST_EXCEPTIONS`], is not skipped: the runner then requires the executor to refuse the
/// block, so a new one fails the gate until it is classified.
const HEADER_OR_BODY_EXCEPTIONS: [&str; 9] = [
    "BlockException.BLOB_GAS_USED_ABOVE_LIMIT",
    "BlockException.INCORRECT_BLOB_GAS_USED",
    "BlockException.INCORRECT_EXCESS_BLOB_GAS",
    "BlockException.INVALID_BASEFEE_PER_GAS",
    "BlockException.INVALID_BLOCK_HASH",
    "BlockException.INVALID_GASLIMIT",
    "BlockException.INVALID_WITHDRAWALS_ROOT",
    "BlockException.RLP_BLOCK_LIMIT_EXCEEDED",
    "BlockException.RLP_STRUCTURES_ENCODING",
];

/// The type of a blob transaction.
const BLOB_TX_TYPE: u8 = 3;

/// Whether the test named `name`, in the fixture file at `path`, is skipped, and why: the first of
/// [`SkipReason::ALL`] that applies.
pub(crate) fn skip_test(path: &str, name: &str, test: &Test) -> Option<SkipReason> {
    let blocks = &test.blocks;
    if blocks.iter().any(FixtureBlock::has_withdrawals) {
        return Some(SkipReason::Withdrawals);
    }
    if blocks.iter().any(is_blob_block) {
        return Some(SkipReason::BlobTransactions);
    }
    if REQUEST_TESTS.iter().any(|dir| name.starts_with(dir)) ||
        blocks.iter().any(|block| expects_only(block, &REQUEST_EXCEPTIONS))
    {
        return Some(SkipReason::Requests);
    }
    if blocks.iter().any(|block| expects_only(block, &HEADER_OR_BODY_EXCEPTIONS)) {
        return Some(SkipReason::HeaderOrBody);
    }
    if blocks
        .iter()
        .any(|block| block.expect_exception.is_some() && block.transactions().any(is_undecodable))
    {
        return Some(SkipReason::UndecodableInvalidTransaction);
    }
    if crate::skips::skip_file(path) == Some(crate::skips::SkipReason::CreateCollisionWithStorage) {
        return Some(SkipReason::CreateCollisionWithStorage);
    }
    None
}

/// The type of an EIP-7702 transaction.
const SET_CODE_TX_TYPE: u8 = 4;

/// Whether an OP block cannot encode `tx`: an EIP-7702 transaction without a recipient, or a fee
/// wider than the 128 bits OP's transaction types give it.
fn is_undecodable(tx: &FixtureTx) -> bool {
    let wide = |fee: Option<U256>| fee.is_some_and(|fee| u128::try_from(fee).is_err());
    (tx.ty == Some(U256::from(SET_CODE_TX_TYPE)) && tx.to.is_none()) ||
        wide(tx.gas_price) ||
        wide(tx.max_fee_per_gas) ||
        wide(tx.max_priority_fee_per_gas)
}

/// Whether `block` carries a blob transaction or expects an exception only one raises.
fn is_blob_block(block: &FixtureBlock) -> bool {
    block.transactions().any(|tx| tx.ty == Some(U256::from(BLOB_TX_TYPE))) ||
        block.exception_names().any(|name| {
            name.starts_with("TransactionException.TYPE_3_") || BLOB_EXCEPTIONS.contains(&name)
        })
}

/// Whether `block` expects an exception, and every alternative it names is one of `names`.
fn expects_only(block: &FixtureBlock, names: &[&str]) -> bool {
    let mut expected = block.exception_names().peekable();
    expected.peek().is_some() && expected.all(|name| names.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test(blocks: serde_json::Value) -> Test {
        serde_json::from_value(json!({
            "genesisRLP": "0x",
            "blocks": blocks,
            "pre": {},
            "postState": {},
            "lastblockhash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "config": { "chainid": "0x01", "blobSchedule": {} },
        }))
        .expect("a test")
    }

    fn reason(name: &str, blocks: serde_json::Value) -> Option<SkipReason> {
        skip_test("blockchain_tests/a/b.json", name, &test(blocks))
    }

    #[test]
    fn test_a_plain_test_is_not_skipped() {
        let blocks =
            json!([{ "rlp": "0x", "transactions": [{ "type": "0x02" }], "withdrawals": [] }]);
        assert_eq!(reason("tests/x.py::t", blocks), None);
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.NONCE_MISMATCH_TOO_LOW" }]);
        assert_eq!(reason("tests/x.py::t", blocks), None);
    }

    #[test]
    fn test_withdrawals_valid_or_not() {
        let blocks = json!([{ "rlp": "0x", "withdrawals": [{ "index": "0x0" }] }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::Withdrawals));
        let blocks = json!([{ "rlp": "0x", "expectException": "BlockException.INVALID_WITHDRAWALS_ROOT",
            "rlp_decoded": { "withdrawals": [{ "index": "0x0" }] } }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::Withdrawals));
    }

    #[test]
    fn test_blob_transactions_valid_or_not() {
        let blocks = json!([{ "rlp": "0x", "transactions": [{ "type": "0x03" }] }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::BlobTransactions));
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS",
            "rlp_decoded": { "transactions": [{ "type": "0x03" }] } }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::BlobTransactions));
        // A block whose RLP does not decode is a blob test by the exception it expects.
        let blocks = json!([{ "rlp": "0x", "expectException":
            "BlockException.RLP_STRUCTURES_ENCODING|TransactionException.TYPE_3_TX_WITH_FULL_BLOBS" }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::BlobTransactions));
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.INSUFFICIENT_MAX_FEE_PER_BLOB_GAS" }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::BlobTransactions));
    }

    #[test]
    fn test_requests_by_name_or_exception() {
        let plain = json!([{ "rlp": "0x" }]);
        for dir in REQUEST_TESTS {
            assert_eq!(
                reason(&format!("{dir}test_x.py::t"), plain.clone()),
                Some(SkipReason::Requests)
            );
        }
        assert_eq!(reason("tests/prague/eip6110_deposits/test_x.py::t", plain), None);
        for name in REQUEST_EXCEPTIONS {
            let blocks = json!([{ "rlp": "0x" }, { "rlp": "0x", "expectException": name }]);
            assert_eq!(
                reason("tests/prague/eip6110_deposits/t", blocks),
                Some(SkipReason::Requests)
            );
        }
    }

    #[test]
    fn test_header_or_body_only_when_every_alternative_is_one() {
        for name in HEADER_OR_BODY_EXCEPTIONS {
            let blocks = json!([{ "rlp": "0x", "expectException": name }]);
            assert_eq!(reason("t", blocks), Some(SkipReason::HeaderOrBody), "{name}");
        }
        let blocks = json!([{ "rlp": "0x", "expectException":
            "BlockException.INVALID_WITHDRAWALS_ROOT|BlockException.INVALID_BLOCK_HASH" }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::HeaderOrBody));
        // An alternative the executor raises keeps the block compared.
        let blocks = json!([{ "rlp": "0x", "expectException":
            "BlockException.INVALID_GASLIMIT|TransactionException.GAS_ALLOWANCE_EXCEEDED" }]);
        assert_eq!(reason("t", blocks), None);
        // A block exception the list does not name is compared, not skipped.
        let blocks =
            json!([{ "rlp": "0x", "expectException": "BlockException.INVALID_STATE_ROOT" }]);
        assert_eq!(reason("t", blocks), None);
    }

    /// The classes are tested in order: a test with withdrawals and a blob transaction is skipped
    /// for its withdrawals.
    #[test]
    fn test_the_first_class_that_applies() {
        let blocks = json!([{ "rlp": "0x", "withdrawals": [{}], "transactions": [{ "type": "0x03" }],
            "expectException": "BlockException.INVALID_GASLIMIT" }]);
        assert_eq!(reason(REQUEST_TESTS[0], blocks), Some(SkipReason::Withdrawals));
    }

    /// A transaction an OP block cannot encode skips its test only when the fixture expects the
    /// block to be invalid.
    #[test]
    fn test_undecodable_invalid_transactions() {
        let set_code = json!({ "type": "0x04", "maxFeePerGas": "0x07" });
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.TYPE_4_TX_CONTRACT_CREATION",
            "rlp_decoded": { "transactions": [set_code] } }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::UndecodableInvalidTransaction));
        let wide = json!({ "type": "0x00", "to": "0x0000000000000000000000000000000000000001",
            "gasPrice": "0x0100000000000000000000000000000000" });
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS",
            "rlp_decoded": { "transactions": [wide] } }]);
        assert_eq!(reason("t", blocks), Some(SkipReason::UndecodableInvalidTransaction));
        // A fee of exactly 128 bits encodes, and a set-code transaction with a recipient does.
        let fits = json!({ "type": "0x02", "to": "0x0000000000000000000000000000000000000001",
            "maxFeePerGas": "0xffffffffffffffffffffffffffffffff", "maxPriorityFeePerGas": "0x00" });
        let set_code_to =
            json!({ "type": "0x04", "to": "0x0000000000000000000000000000000000000001" });
        let blocks = json!([{ "rlp": "0x", "expectException": "TransactionException.INSUFFICIENT_ACCOUNT_FUNDS",
            "rlp_decoded": { "transactions": [fits, set_code_to] } }]);
        assert_eq!(reason("t", blocks), None);
        // Valid blocks carry no such transaction; one would fail the gate rather than skip.
        let blocks = json!([{ "rlp": "0x", "transactions": [{ "type": "0x04" }] }]);
        assert_eq!(reason("t", blocks), None);
    }

    /// The collision files are the state-test gate's, matched on the fixture's path.
    #[test]
    fn test_create_collisions_by_the_state_test_list() {
        let plain = test(json!([{ "rlp": "0x" }]));
        for path in [
            "blockchain_tests/paris/eip7610_create_collision/test_init_collision_create_tx.json",
            "blockchain_tests/static/state_tests/stCreate2/create2collisionStorageParis.json",
        ] {
            assert_eq!(skip_test(path, "t", &plain), Some(SkipReason::CreateCollisionWithStorage));
        }
        // The state-test gate's other skips are not this gate's.
        for path in
            ["blockchain_tests/a/ValueOverflowParis.json", "blockchain_tests/a/loopMul.json"]
        {
            assert_eq!(skip_test(path, "t", &plain), None);
        }
    }

    #[test]
    fn test_reason_names_round_trip() {
        for reason in SkipReason::ALL {
            assert_eq!(reason.name().parse::<SkipReason>(), Ok(reason));
        }
        assert!("slow".parse::<SkipReason>().is_err());
    }
}
