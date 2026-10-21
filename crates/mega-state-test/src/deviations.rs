//! The places Satin differs from Ethereum on purpose, and the fixture entries each one fails.
//!
//! Equivalence mode runs a fixture on Satin's machinery priced as the fixture's fork prices it,
//! so a failure there is either a bug or a rule `MegaETH` keeps that no configuration can turn
//! off. Every rule of the second kind is registered here, once, with the reason Satin keeps it and
//! the failures it explains; a failure no entry explains is unattributed, and one unattributed
//! failure fails the gate.
//!
//! A deviation lists every entry it explains on the pinned fixture release — the fixture file,
//! the test, the entry's data, gas and value indices — with the hashes Satin produces for it: the
//! state root, or for a logs mismatch the logs hash and the state root. It explains a failure only
//! of a listed entry and only with those hashes, so a listed entry that fails another way is
//! unattributed; and the gate requires every listed entry to fail exactly as listed, so one that
//! starts to pass, or stops running, fails the gate as surely as a new failure. The count of
//! failures a deviation explains is derived from its list.

use core::fmt;

use revm::primitives::b256;
use serde::Serialize;

use crate::{
    blockchain,
    runner::{Produced, TestId},
    Fork,
};

/// A place Satin differs from Ethereum on purpose.
#[derive(Debug, Serialize)]
pub struct Deviation {
    /// A short, stable name.
    pub id: &'static str,
    /// The `MegaETH` rule the fixtures meet.
    pub rule: &'static str,
    /// Why Satin keeps the rule, and why the neutral configuration cannot express Ethereum's.
    pub reason: &'static str,
    /// The fork whose fixtures meet the rule.
    pub fork: Fork,
    /// Every entry of the fork's pinned fixture release the rule fails, in path, name and index
    /// order.
    pub entries: &'static [Entry],
    /// Every blockchain test of the pinned main release the rule fails, in path and name order.
    /// Only an Osaka deviation lists any: the blockchain tests run are the Osaka ones.
    pub blockchain_entries: &'static [BlockchainEntry],
}

/// A blockchain test a deviation explains: every block of it whose outcome is not its header's,
/// with the outcome Satin produces for it, and how the chain ends.
///
/// The runner imports the whole chain, every listed block on Satin's own post-state, and holds
/// every block to its header or to its listed outcome exactly; a block that differs and is not
/// listed fails the test, as does a listed block that does not produce its listed outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct BlockchainEntry {
    /// The fixture file, relative to the release's `blockchain_tests` directory.
    pub path: &'static str,
    /// The test's name within the file.
    pub name: &'static str,
    /// Every block whose outcome is not its header's, in block order.
    pub blocks: &'static [ListedBlock],
    /// How the chain ends, and so whether its post-state is compared.
    pub end: ChainEnd,
}

/// A block of a listed blockchain test whose outcome is not its header's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ListedBlock {
    /// What Satin produces for the block; its index is the block's.
    pub produced: blockchain::Produced,
    /// Why the block differs.
    pub cause: Cause,
}

/// Why a listed block's outcome is not its header's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// The deviation's rule acts in this block.
    Rule,
    /// The block runs as on Ethereum — its gas used, logs bloom and receipts root are its
    /// header's — and its state root differs only by the state the listed block it names left
    /// behind. The runner holds the claim: a block listed so whose own outcome differs, or that
    /// names a block that is not an earlier listed one, fails the test.
    StateLeftBy(usize),
}

/// How a listed blockchain test's chain ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChainEnd {
    /// The chain ends on a state its last block's header describes: its head and its post-state
    /// are compared with the fixture's.
    FixtureState,
    /// The chain ends on a state a listed block left, which the fixture's post-state does not
    /// describe: its head is compared with the fixture's and its post-state is not, for the
    /// reason given.
    DeviatedState(&'static str),
}

/// A fixture entry a deviation explains, and the hashes Satin produces for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The fixture file, relative to the release's `state_tests` directory.
    pub path: &'static str,
    /// The test's name within the file.
    pub name: &'static str,
    /// The entry's data, gas and value indices.
    pub indexes: (usize, usize, usize),
    /// The hashes Satin produces for the entry, and with them how it fails.
    pub produced: Produced,
}

impl Deviation {
    /// The entries the deviation lists for `fork`.
    pub fn listed(&self, fork: Fork) -> &'static [Entry] {
        if self.fork == fork {
            self.entries
        } else {
            &[]
        }
    }

    /// The blockchain test `id`, as this deviation lists it, if it does.
    pub fn blockchain_entry(&self, id: &blockchain::TestId) -> Option<&'static BlockchainEntry> {
        self.blockchain_entries.iter().find(|entry| entry.is(id))
    }

    /// Whether a failure of the entry `id` of `fork`, which produced `produced`, is this
    /// deviation's: the entry is listed, with exactly those hashes.
    pub fn explains(&self, fork: Fork, id: &TestId, produced: Option<Produced>) -> bool {
        produced.is_some_and(|produced| {
            self.listed(fork).iter().any(|entry| entry.produced == produced && entry.is(id))
        })
    }
}

impl BlockchainEntry {
    /// Whether `id` is this test: the same name, in a file whose path ends with this entry's path.
    pub fn is(&self, id: &blockchain::TestId) -> bool {
        id.name == self.name && path_ends_with(&id.path, self.path)
    }
}

impl BlockchainEntry {
    /// The listed block at `index`, if the entry lists it.
    pub fn block(&self, index: usize) -> Option<&'static ListedBlock> {
        self.blocks.iter().find(|block| block.produced.block == index)
    }
}

/// The deviation in `registry` that lists the blockchain test `id`, and its entry for it.
///
/// No test is listed by two deviations (a test checks it on the registry), so the first is the
/// one.
pub fn blockchain_entry(
    registry: &'static [Deviation],
    id: &blockchain::TestId,
) -> Option<(&'static Deviation, &'static BlockchainEntry)> {
    registry.iter().find_map(|deviation| deviation.blockchain_entry(id).map(|e| (deviation, e)))
}

impl fmt::Display for BlockchainEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} :: {}", self.path, self.name)
    }
}

/// Whether `path` ends with `suffix` on a path component boundary.
fn path_ends_with(path: &str, suffix: &str) -> bool {
    let path = path.replace('\\', "/");
    path.strip_suffix(suffix).is_some_and(|dir| dir.is_empty() || dir.ends_with('/'))
}

impl Entry {
    /// Whether `id` is this entry: the same test and indices, in a file whose path ends with this
    /// entry's path.
    pub fn is(&self, id: &TestId) -> bool {
        id.name == self.name && id.indexes == self.indexes && path_ends_with(&id.path, self.path)
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (d, g, v) = self.indexes;
        write!(f, "{} :: {} [d={d} g={g} v={v}]", self.path, self.name)
    }
}

/// Every registered deviation.
pub const DEVIATIONS: &[Deviation] = &[AMSTERDAM_OPCODES_ON_OSAKA, SELFDESTRUCT_BURNS_ON_OSAKA];

/// Satin's instruction table carries four Amsterdam opcodes on its Osaka base.
pub const AMSTERDAM_OPCODES_ON_OSAKA: Deviation = Deviation {
    id: "amsterdam-opcodes-on-osaka",
    rule: "Satin runs DUPN, SWAPN and EXCHANGE (EIP-8024) and SLOTNUM (EIP-7843), which its Osaka \
           base leaves undefined.",
    reason: "Satin takes the Amsterdam schedule and the opcodes with it. The instruction table is \
             Satin's machinery, which equivalence mode keeps, so an Osaka fixture that executes \
             one of the four bytes, expecting the undefined-opcode halt Osaka gives it, runs the \
             opcode instead. With the four entries taken back out of the table, every Osaka \
             fixture passes.",
    fork: Fork::Osaka,
    blockchain_entries: &[
        BlockchainEntry {
            path: "frontier/opcodes/test_all_opcodes.json",
            name: "tests/frontier/opcodes/test_all_opcodes.py::test_all_opcodes[fork_Osaka-blockchain_test_from_state_test]",
            blocks: &[
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 0,
                        gas_used: 8_129_268,
                        logs_bloom_hash: b256!(
                            "0xbe67c3e27ea8993a93e1784dfad43146e9c48f71d7d56dfd86d6c1041920b232"
                        ),
                        receipts_root: b256!(
                            "0xa8efe1faeb804a16183cc48903e6d13ed9c9786bfb146708b605feb6448bdd93"
                        ),
                        state_root: b256!(
                            "0x311f0607743b16ef7eb341125d364a002eaceb2f05ac936bbd0f839865d2711d"
                        ),
                    },
                    cause: Cause::Rule,
                },
            ],
            end: ChainEnd::DeviatedState(
                "its one block runs the four opcodes Osaka halts on, so the chain ends on the state they leave",
            ),
        },
        BlockchainEntry {
            path: "frontier/scenarios/test_scenarios.json",
            name: "tests/frontier/scenarios/test_scenarios.py::test_scenarios[fork_Osaka-blockchain_test-test_program_program_INVALID-debug]",
            blocks: &[
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 0,
                        gas_used: 5_386_221,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x55de3d2d9f2c2c5622556508e5cda2ddca148e43439eb16a46115b797792d3bf"
                        ),
                        state_root: b256!(
                            "0x50cc3c920ca4d635bb156978614d544911917dda2314da86c6ae47f86d8668ff"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 1,
                        gas_used: 5_386_221,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x55de3d2d9f2c2c5622556508e5cda2ddca148e43439eb16a46115b797792d3bf"
                        ),
                        state_root: b256!(
                            "0x7a432172471d48e4eabea2f6f7843ae1dc13ba9db35ebb90ea7b71e2797381b8"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 2,
                        gas_used: 5_386_224,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xc8a1e306c05c6c22dcc04fb01dd3e84c3ba16088eccf053cfa10c07d1d604c89"
                        ),
                        state_root: b256!(
                            "0x6492319287311df06e117d1bf0167868072f0ec0522a168cbae279e88f6edeb5"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 3,
                        gas_used: 5_386_224,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xc8a1e306c05c6c22dcc04fb01dd3e84c3ba16088eccf053cfa10c07d1d604c89"
                        ),
                        state_root: b256!(
                            "0x8b428b568632e6056542468c6fc7ed031c32db3a7b7286b04285b6e735c30a53"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 4,
                        gas_used: 5_376_889,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x44b3958d3a4da284d93cd9413fdd02affc4fa0d2c2ae0dd4e08c98a38d7a6504"
                        ),
                        state_root: b256!(
                            "0xfa2a2f0e8f686cc0f5d1bf7d01f6048c17795b0d6f8c319816742ada3d5353ed"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 5,
                        gas_used: 5_386_221,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x55de3d2d9f2c2c5622556508e5cda2ddca148e43439eb16a46115b797792d3bf"
                        ),
                        state_root: b256!(
                            "0xd76637032c7aba3c937f7fbe047a1bc277dccd78377e63681570ebeb239ab721"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 6,
                        gas_used: 5_386_221,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x55de3d2d9f2c2c5622556508e5cda2ddca148e43439eb16a46115b797792d3bf"
                        ),
                        state_root: b256!(
                            "0xc4217d76247c681511a0c3e6b7bb5712fc08c8e08f8d666992cb24d40ab3b7e5"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 7,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0x150535f0c70945d4025bbd207f255412568187ff74ec5c4f068a6c3159a1b215"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 8,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0xca8b4f50f5ee3e3b992db528da9cf18d3495161478583903f26d7c58590c434b"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 9,
                        gas_used: 5_376_889,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x44b3958d3a4da284d93cd9413fdd02affc4fa0d2c2ae0dd4e08c98a38d7a6504"
                        ),
                        state_root: b256!(
                            "0x32c1d16829c873e7f9762e026d6d9e7f007aedf4b96c60a7a774d70cf68652ab"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 10,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0x3c04b7a4296b76ee622d099c6a7e5195cf9884f4e005a452dc24f431fe8679ca"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 11,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0x3c2819d923e552d459a255be2e68e04210cac0b31ca3fb45bce60b5ff9cfdfcd"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 12,
                        gas_used: 5_399_627,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x5532612bb2ec2b05606828aafab98443ddeede215ae17df462f7b6859e9fff26"
                        ),
                        state_root: b256!(
                            "0x8646405e196a3332ee3bb75af4cb1ba7f0c51c646c1f74fe88193f9971d27019"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 13,
                        gas_used: 5_399_627,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x5532612bb2ec2b05606828aafab98443ddeede215ae17df462f7b6859e9fff26"
                        ),
                        state_root: b256!(
                            "0xc95eee62b72c78d47f25497acb8715f3ed6c6b1ef1cf48fc5810bef03c87d26e"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 14,
                        gas_used: 5_383_592,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x1b4b33d4f2db7ecb0071a2ed95938ca5bf97df8c4c259c1b1542ee79cfad93b4"
                        ),
                        state_root: b256!(
                            "0x4f98fc50f394e6a3264df2c21dc77d9d0f0134b741f1faf42654ed1e9b3e2ea9"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 15,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0x1ee283e5862adf81f60e3cef87613b7f7dc02b3eef325e2246a212e24ad439bc"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 16,
                        gas_used: 5_392_924,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x20e47a185042afce840b786dcbf99dea66b1488180e2cce569511d133fcb4344"
                        ),
                        state_root: b256!(
                            "0xf72728582c1345a0b218adf1baa60e68cfa6d1bb9f16fb3d9c83be03d277f9b4"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 17,
                        gas_used: 5_399_627,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x5532612bb2ec2b05606828aafab98443ddeede215ae17df462f7b6859e9fff26"
                        ),
                        state_root: b256!(
                            "0xc9b1b1e13a3ae1a8640ca3204c1bcb5b82dcc01a75135105ad2ad43629708305"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 18,
                        gas_used: 5_399_627,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x5532612bb2ec2b05606828aafab98443ddeede215ae17df462f7b6859e9fff26"
                        ),
                        state_root: b256!(
                            "0xe5521ec635fa43963276146cd72c55fcd3119123c1fdbab42667de6c76b6b86b"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 19,
                        gas_used: 5_383_592,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x1b4b33d4f2db7ecb0071a2ed95938ca5bf97df8c4c259c1b1542ee79cfad93b4"
                        ),
                        state_root: b256!(
                            "0x7ca5f100fa3a11694d1fb64a2c57359f2bc60aa9f492413dd47c93936d2efb10"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 20,
                        gas_used: 5_412_989,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xa22daee9f564b06c805a9b32ba0f6b26e267388fe3bd24de782c7a8e3bd3200c"
                        ),
                        state_root: b256!(
                            "0xd70438c04dd69c9d1c2450a8f719e89c530aa86da49e4f1de4806b6d5219113a"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 21,
                        gas_used: 5_412_974,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x02e92ec835e38ac88ac8bb94790955bf8aadc3d53b34733e4fa77fd18bda5f82"
                        ),
                        state_root: b256!(
                            "0x7e69c5919cf9e7a663ccc53d78b3dd11128f65809862a53a58d12c12dfec4d50"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 22,
                        gas_used: 5_425_218,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xae1970250c7686623887a6629c3b89de2107591e74db05483e6cfa549d83721d"
                        ),
                        state_root: b256!(
                            "0x3d2e7d5f919008960dc9043f0aff0fcd960c6324d07900feb6be5d7829c9e25d"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 23,
                        gas_used: 5_425_218,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xae1970250c7686623887a6629c3b89de2107591e74db05483e6cfa549d83721d"
                        ),
                        state_root: b256!(
                            "0xc76d312ba2294d1e2a2e4744c47948d97e23effc4f884f6afe2d92c56971d00c"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 24,
                        gas_used: 5_431_921,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x80d1b6b1cc4c436be347448203fe33fb20a10269a969d4d7584607e81e20a523"
                        ),
                        state_root: b256!(
                            "0x9795b537f1c4a81c45ecbd68cd6a768f1edf4851c1af2f58dd36df2ce990a60c"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 25,
                        gas_used: 5_431_921,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x80d1b6b1cc4c436be347448203fe33fb20a10269a969d4d7584607e81e20a523"
                        ),
                        state_root: b256!(
                            "0xd520334239c6aca8938bcc6000730ab1d092abb057e8a9e9d48bec67d09b561d"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 26,
                        gas_used: 5_425_197,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x4592127747c3ae6dcf115d65b2cce745607a8318fe7f8e664e0555193ffb7e56"
                        ),
                        state_root: b256!(
                            "0x68bd60a5015b88c0b8e7e185f8be77cfdb27cd5589f1f215d98b7678bc414d76"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 27,
                        gas_used: 5_425_197,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x4592127747c3ae6dcf115d65b2cce745607a8318fe7f8e664e0555193ffb7e56"
                        ),
                        state_root: b256!(
                            "0x7dd74978b8093b1e70baf3100ae39040303bcda411a13beec70e22ee72a29833"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 28,
                        gas_used: 5_431_900,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x95c628e38ba7c4cfe057b1f25eabf4839e440366a6ac77af656eb6e613363a1d"
                        ),
                        state_root: b256!(
                            "0x757f6d279391869cc5645519509c639664976d262443f14c260ea0dd6555a9bc"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 29,
                        gas_used: 5_431_900,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x95c628e38ba7c4cfe057b1f25eabf4839e440366a6ac77af656eb6e613363a1d"
                        ),
                        state_root: b256!(
                            "0x112f123f5e1e6bb8b62435629d05b78b41241e7cad87f164fdfc5251514fdaab"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 30,
                        gas_used: 5_354_342,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x77d8bb582b975327486b5eaf11eefab2c24f4d7eea18a068393a0bf07fe4a12f"
                        ),
                        state_root: b256!(
                            "0xdd0f896ef11e7b76ca47798934817924b7ff6d4f69a6332dc58479a91bed4bae"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 31,
                        gas_used: 6_996_648,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x11e83acea62ae5fa281bbc0e08cbb0e8096b6639aa23b482e09be96b634478d1"
                        ),
                        state_root: b256!(
                            "0x62884511e634038fe0d74ba175861b715288e3f0f22ef2d833e90308b3d692e7"
                        ),
                    },
                    cause: Cause::StateLeftBy(30),
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 32,
                        gas_used: 5_374_242,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x49e5cee7b540b0aa0c1da72b0f14ce1c6f0462b9a5d912dc2d07f025d9703c47"
                        ),
                        state_root: b256!(
                            "0x50aa0f67eadbc50b215297d51f9963dab59f8820c0f5175eb236bc6a58eab823"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 33,
                        gas_used: 10_692_884,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x11f8bdf99c03a9e5a4434febe2b60a3d448beea5a90859e9aeaf64f1a7f39aa9"
                        ),
                        state_root: b256!(
                            "0x42f64807cbdc63fd10bff15150e7d7a6a49d74706107891f9ef43c3b81573473"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 34,
                        gas_used: 10_742_872,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xb46d2b23868948b3dca1ee22d01e7555454b3299a9b46938b0468425524b6d62"
                        ),
                        state_root: b256!(
                            "0x6124ecf0c28fafaf690def9ad816ed272a9111021599e3ffb0dcf12174de6682"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 35,
                        gas_used: 10_692_890,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xc23d5fbdb417f4a15b3c25cb25d1aa377be4e3534f218c075e24a27fcffc6e41"
                        ),
                        state_root: b256!(
                            "0xf9001fb3d1ba34652291e969f128d8ad39018e0addaae5b73c84184e4add58b8"
                        ),
                    },
                    cause: Cause::Rule,
                },
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 36,
                        gas_used: 10_692_890,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0xc23d5fbdb417f4a15b3c25cb25d1aa377be4e3534f218c075e24a27fcffc6e41"
                        ),
                        state_root: b256!(
                            "0xba79f738e1ded0240a5c87b6ee54daa4e0352b497af0ae1886df1234dbfd47e3"
                        ),
                    },
                    cause: Cause::Rule,
                },
            ],
            end: ChainEnd::DeviatedState(
                "every block runs the program's four opcodes Osaka halts on or carries the state they left, so the chain ends on that state",
            ),
        },
        BlockchainEntry {
            path: "static/state_tests/stBadOpcode/undefinedOpcodeFirstByte.json",
            name: "tests/static/state_tests/stBadOpcode/undefinedOpcodeFirstByteFiller.yml::undefinedOpcodeFirstByte[fork_Osaka-blockchain_test_from_state_test-]",
            blocks: &[
                ListedBlock {
                    produced: blockchain::Produced {
                        block: 0,
                        gas_used: 3_969_244,
                        logs_bloom_hash: b256!(
                            "0xd397b3b043d87fcd6fad1291ff0bfd16401c274896d8c63a923727f077b8e0b5"
                        ),
                        receipts_root: b256!(
                            "0x9a8f3b88fe2bb353fe86d44bd5be426458c3544f423b8e70f0dff0a06c9634bc"
                        ),
                        state_root: b256!(
                            "0xa1527ad75667d1f7455aa215c374f3c21c9fde318ec9fb652ee9c8fac11dc72b"
                        ),
                    },
                    cause: Cause::Rule,
                },
            ],
            end: ChainEnd::DeviatedState(
                "its one block runs the four opcodes Osaka halts on, so the chain ends on the state they leave",
            ),
        },

    ],
    entries: &[
        Entry {
            path: "frontier/opcodes/test_all_opcodes.json",
            name: "tests/frontier/opcodes/test_all_opcodes.py::test_all_opcodes[fork_Osaka-state_test]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x2790bb00bde351feb12b3607acea67097e9f7d0a3913d61c7269c3603a77b8c5"
            )),
        },
        Entry {
            path: "static/state_tests/stBadOpcode/undefinedOpcodeFirstByte.json",
            name: "tests/static/state_tests/stBadOpcode/undefinedOpcodeFirstByteFiller.yml::undefinedOpcodeFirstByte[fork_Osaka-state_test-]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xefc47fa41b70e7a1b1aa1ac3c0b04af29ccb2ed2f766fe42ff764e1f3b9fe0de"
            )),
        },
    ],
};

/// A contract self-destructed in its creating transaction is removed as on Satin's Osaka base,
/// where Amsterdam keeps a balance it sent to itself (EIP-8246).
pub const SELFDESTRUCT_BURNS_ON_OSAKA: Deviation = Deviation {
    id: "selfdestruct-burns-on-osaka",
    rule: "A contract self-destructed in the transaction that created it is removed as on Satin's \
           Osaka base, a balance it sent to itself burned: EIP-8246, which keeps that balance and \
           clears the account only at the end of the transaction, is not active.",
    reason: "revm gates EIP-8246 on the Amsterdam spec id, in the journal, and Satin's spec runs \
             on Karst, an Osaka base: Satin takes Amsterdam's rules one switch at a time — EIP-8037, \
             EIP-2780, the opcodes — and EIP-8246 has no switch, so no configuration can turn it \
             on. An Amsterdam fixture in which a contract destructs to itself expects the balance \
             kept, and the EIP-7708 transfer logs a kept balance produces when it is sent on. revm \
             itself, on an Osaka base with Amsterdam's configuration, produces exactly the hashes \
             listed for every one of these entries, and the fixtures' own on the Amsterdam spec. \
             Whether Satin takes EIP-8246 is a decision for its specification, not for this gate.",
    fork: Fork::Amsterdam,
    blockchain_entries: &[],
    entries: &[
        Entry {
            path: "for_amsterdam/amsterdam/eip2780_reduce_intrinsic_tx_gas/top_frame_charges/initcode_selfdestruct_keeps_top_frame_state_charge.json",
            name: "tests/amsterdam/eip2780_reduce_intrinsic_tx_gas/test_top_frame_charges.py::test_initcode_selfdestruct_keeps_top_frame_state_charge[fork_Amsterdam-state_test-non-zero_value-self_beneficiary]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x8a227aa6adf983ba803d01b09126eb9d226cf95a1e083f194c39f0e29e8a1e44"
            )),
        },
        Entry {
            path: "for_amsterdam/amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_call/call_value_to_self_destructed_same_tx_account.json",
            name: "tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_call.py::test_call_value_to_self_destructed_same_tx_account[fork_Amsterdam-state_test-create2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x5857f66aee23a75fbc2b874e871eb146226ff2dab2194b4597cb87b9b954347c"
            )),
        },
        Entry {
            path: "for_amsterdam/amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_call/call_value_to_self_destructed_same_tx_account.json",
            name: "tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_call.py::test_call_value_to_self_destructed_same_tx_account[fork_Amsterdam-state_test-create]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x13370ccc6e5eb2bf5a75774bef80223d973990a429e9a3a6e2abe32eea70d091"
            )),
        },
        Entry {
            path: "for_amsterdam/amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_selfdestruct/selfdestruct_to_self_in_create_tx.json",
            name: "tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_selfdestruct.py::test_selfdestruct_to_self_in_create_tx[fork_Amsterdam-state_test]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xd1e99d8e4322d56db48a9c6fbf91231d405a592394fe30943639d7bb8e4af3e9"
            )),
        },
        Entry {
            path: "for_amsterdam/amsterdam/eip8038_state_access_gas_cost_increase/selfdestruct_gas/same_tx_created_selfdestruct_self_burn.json",
            name: "tests/amsterdam/eip8038_state_access_gas_cost_increase/test_selfdestruct_gas.py::test_same_tx_created_selfdestruct_self_burn[fork_Amsterdam-state_test]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x60ec6fdd1f4b9afbf1f6bae43abbc3a21fc917cf921bc7f43827d2dda86f411b"
            )),
        },
        Entry {
            path: "for_amsterdam/amsterdam/eip8246_selfdestruct_no_burn/selfdestruct_no_burn/create_transaction_initcode_selfdestruct.json",
            name: "tests/amsterdam/eip8246_selfdestruct_no_burn/test_selfdestruct_no_burn.py::test_create_transaction_initcode_selfdestruct[fork_Amsterdam-state_test-kept]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x8a227aa6adf983ba803d01b09126eb9d226cf95a1e083f194c39f0e29e8a1e44"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x5c00fce098c2fe994de7c23a67990d729c20a258651d132e2b21c3a2d0b5803a"),
                state_root: b256!("0x2f2356140f67ac4504ff5f416e56edd0264e8ec19ed39ac6f8897b9023d27307"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x6aa38fb5b844e13cd8efa6b776f957550e0a4b27815a2f34cf3fa9bc4cae730e"),
                state_root: b256!("0xf08b417e5ac7138f3e58e07dc90970a2780901935979e0a3010ba8ebe5aae924"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0xc6f151ee88a456efe8faee5993fac962f32645dd408bdf75e86a411e4d2b5b35"),
                state_root: b256!("0xd42fe0ac1ad09661381fc6c43ee67a7c169bcb4610e89ec0c60aaa793d8831c4"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0xf76e4fa7500c4db34112ba382a1365e5503307c1ae300be79fa59550ba4eef32"),
                state_root: b256!("0x9903a0c8f461940cadd5f48ea90bddfe1f019182c65d3f6a6b2533d336199063"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x2c2102d3296f1610fdf87d1182226cd4e793882caa71c96f965f0e5e7af21590"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x7cb1c8d882ce555bf85cff9961ebde8c14542d96d2394935edbf95b745c5265f"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_single_self_recipient-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xa695e4f522a7e47293590084faa0dc45ab419b7c0a2449808fd7e2a716968c80"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_single_self_recipient-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x97844214d20796ee78c52c1d5a6338e21dfa8bdf6031a19cc359b9c46d5da076"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x5c00fce098c2fe994de7c23a67990d729c20a258651d132e2b21c3a2d0b5803a"),
                state_root: b256!("0x7db1f7e8a7499e52962e8b457cbaa28545b035ec6d3491621ad8a10f0abcfec4"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x6aa38fb5b844e13cd8efa6b776f957550e0a4b27815a2f34cf3fa9bc4cae730e"),
                state_root: b256!("0x7ae7c17d34642ed4d4a56e2613cc50ab5eeb617b5b2c965f946de58060b99447"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x79545168c90d8fed276adcd2bfe3610a0ba259044ba117fb9cafb0bcbbf3a3df"),
                state_root: b256!("0x4f389b292dedd1e7effa9c1947e9ec9fb83a40f049f9cf4b87262e1d96c9682a"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x7c73347bddfb53f07057096e6949c82c32d3fb61532a008261cbf1bf43fcd19e"),
                state_root: b256!("0x254f07b354bf3e97b96e4a06ddee51489ca23a71fb6e088df9f460c35d880698"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x4bd3e188f8d440a5762a67eb84a9d133a6e2979fb68d7414b1e0a08bcee83d48"),
                state_root: b256!("0x807ff3a9c69267c578eae95089ba8087582eb630fe514aae537500b9edbcf880"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::Logs {
                logs: b256!("0x88bd5130a835f0eb5cce6c8229e768ae96dec7a79cd757dca32b5e3ce57b86b0"),
                state_root: b256!("0xe331c545e628b14d64deccaac85755e699f0e75aba8189847ed810e4a4f0ddf8"),
            },
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xfbd1f3e7d906012db567e5429e1236dab7466d40798973c4dc4ba2984d621f72"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xd75f439c744aae2d05d0151919fa298073b99086c3af81087202b5c4464b2a02"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_single_self_recipient-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x1c0a70089f50f9da9183fa04210ae0f04affc03d146fcb78d32f3c5ef2853a47"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_single_self_recipient-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x0c7614fbae22b3855669bfe3ee994bc18b31b32d84903a0372834f966a265b8d"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-single_call_self-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xaa95dfb767c86d4d4c261252b2a6224f32d15955eaf92650c4c97bb5c7892d95"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-single_call_self-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x9c91de53b0bc52572ca1c1ea6ccffcf42f725876a8f439ae676b7cdf9567d43a"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-call_times_2-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xdc78c509c41771960af990fbbcf5dbd0e6f72e8a6615bcb5cc1b75c8f12dc0bd"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-call_times_2-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x8eda22cdda403dd638ac173e1e6e648414d1d20aa8559e3a26af76939eda5d4b"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-call_times_2-create_opcode_CREATE2]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x998432460e11445f1529c1fde7e70e3c97818e9af0d2b3180e2329bd719d5e4c"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-call_times_2-create_opcode_CREATE]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x6989f4cf277b4f10675e678976e0ffde965e2d23634946b31f42e61b948fe31f"
            )),
        },
        Entry {
            path: "for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct_revert/selfdestruct_created_in_same_tx_with_revert.json",
            name: "tests/cancun/eip6780_selfdestruct/test_selfdestruct_revert.py::test_selfdestruct_created_in_same_tx_with_revert[fork_Amsterdam-state_test-outer_selfdestruct_before_inner_call-same_tx]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x68de4499d08d8c9e6260ade80ed6a4203630f49dc1947b2312d308a7fd104e40"
            )),
        },
        Entry {
            path: "for_amsterdam/frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json",
            name: "tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_False]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x723798b921cd0f3a700aa00dffcb87da7df2440def96d3ed96b36e2bc0a1e545"
            )),
        },
        Entry {
            path: "for_amsterdam/frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json",
            name: "tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_True]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xd82f53b114fbab4d8784e89069ef7b272e1d74f2cc26a363f96141b9eb771b6c"
            )),
        },
        Entry {
            path: "for_amsterdam/frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json",
            name: "tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE2-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_False]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x52695eb8bf3c83b205d4cfd24acb88af856a2db2ec5236e66202833d8aa94f3c"
            )),
        },
        Entry {
            path: "for_amsterdam/ported_static/stCreate2/create2_suicide/create2_suicide.json",
            name: "tests/ported_static/stCreate2/test_create2_suicide.py::test_create2_suicide[fork_Amsterdam-state_test-d6]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xe6d927efd83aabb7d94664d06b79744a361a74374f377f6c8dd31307e577ccde"
            )),
        },
        Entry {
            path: "for_amsterdam/ported_static/stCreate2/create2_suicide/create2_suicide.json",
            name: "tests/ported_static/stCreate2/test_create2_suicide.py::test_create2_suicide[fork_Amsterdam-state_test-d7]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0x087df75fad91a65e75b9373ad835882429c431166f9a3e6d086206cd39e2f662"
            )),
        },
        Entry {
            path: "for_amsterdam/ported_static/stInitCodeTest/transaction_create_suicide_in_initcode/transaction_create_suicide_in_initcode.json",
            name: "tests/ported_static/stInitCodeTest/test_transaction_create_suicide_in_initcode.py::test_transaction_create_suicide_in_initcode[fork_Amsterdam-state_test]",
            indexes: (0, 0, 0),
            produced: Produced::StateRoot(b256!(
                "0xf6456935a8fdccc4fa60c10233d27e602088fdfbce68c9db41363767cffd367d"
            )),
        },
    ],
};

/// The deviation in `registry` that explains a failure of the entry `id` of `fork`, which
/// produced `produced`, if one does.
///
/// No entry is listed by two deviations (a test checks it on the registry), so the first that
/// explains a failure is the one.
pub fn attribute(
    registry: &'static [Deviation],
    fork: Fork,
    id: &TestId,
    produced: Option<Produced>,
) -> Option<&'static Deviation> {
    registry.iter().find(|deviation| deviation.explains(fork, id, produced))
}

/// The registry as the crate's `DEVIATIONS.md` has it, one sentence to a line.
pub fn render_markdown() -> String {
    let sentences = |text: &str| text.replace(". ", ".\n");
    let mut out = String::from(
        "# Deviations\n\n\
         <!-- Rendered from `src/deviations.rs` by \
         `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`; do not edit. -->\n\n\
         The places Satin differs from Ethereum on purpose that the execution-spec gate meets, \
         each with the fixture entries it explains and the hashes Satin produces for them.\n\
         In equivalence mode a failure is a deviation's only when the deviation lists its entry \
         with the hashes it produced, and every listed entry must fail exactly as listed: a \
         failure no deviation explains, and a listed entry that passes or fails another way, \
         fail the gate.\n",
    );
    for deviation in DEVIATIONS {
        out.push_str(&format!("\n## `{}`\n\n", deviation.id));
        out.push_str(&format!(
            "{} failed entries of the pinned {} fixtures.\n\n",
            deviation.entries.len(),
            deviation.fork
        ));
        out.push_str(&format!("**Rule.**\n{}\n\n", sentences(deviation.rule)));
        out.push_str(&format!("**Reason.**\n{}\n\n", sentences(deviation.reason)));
        out.push_str(
            "**Failures.** Each entry with its data, gas and value indices, how it fails and the \
             hashes Satin produces; paths are relative to the release's `state_tests` \
             directory.\n\n",
        );
        let mut path = "";
        for entry in deviation.entries {
            if entry.path != path {
                path = entry.path;
                out.push_str(&format!("- `{path}`\n"));
            }
            let (d, g, v) = entry.indexes;
            let hashes = match entry.produced {
                Produced::StateRoot(root) => format!("state root `{root}`"),
                Produced::Logs { logs, state_root } => {
                    format!("logs hash `{logs}`, state root `{state_root}`")
                }
            };
            out.push_str(&format!(
                "  - `{}` d={d} g={g} v={v}: `{}`, {hashes}\n",
                entry.name,
                entry.produced.kind().name()
            ));
        }
        if !deviation.blockchain_entries.is_empty() {
            out.push_str(&format!(
                "\n**Blockchain tests.** {} tests of the pinned main release's `blockchain_tests`, \
                 each with every block whose outcome is not its header's — the gas used, the keccak \
                 hash of the logs bloom, the receipts root and the state root Satin produces for it, \
                 and why it differs — and how the chain ends; every other block of the test matches \
                 its header, and paths are relative to the release's `blockchain_tests` \
                 directory.\n\n",
                deviation.blockchain_entries.len()
            ));
            for entry in deviation.blockchain_entries {
                out.push_str(&format!("- `{}`\n  - `{}`\n", entry.path, entry.name));
                for listed in entry.blocks {
                    let produced = listed.produced;
                    let cause = match listed.cause {
                        Cause::Rule => "the rule acts".to_string(),
                        Cause::StateLeftBy(block) => format!("the state block {block} left"),
                    };
                    out.push_str(&format!(
                        "    - block {} ({cause}): gas used {}, logs bloom hash `{}`, receipts \
                         root `{}`, state root `{}`\n",
                        produced.block,
                        produced.gas_used,
                        produced.logs_bloom_hash,
                        produced.receipts_root,
                        produced.state_root
                    ));
                }
                let end = match entry.end {
                    ChainEnd::FixtureState => {
                        "the chain ends on its last header's state; its head and post-state are \
                         compared"
                            .to_string()
                    }
                    ChainEnd::DeviatedState(reason) => format!(
                        "the chain ends on a state a listed block left; its head is compared and \
                         its post-state is not: {reason}"
                    ),
                };
                out.push_str(&format!("    - End: {end}.\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FailureKind;
    use revm::primitives::B256;
    use std::collections::BTreeSet;

    fn id(entry: &Entry, dir: &str) -> TestId {
        TestId {
            path: format!("{dir}{}", entry.path),
            name: entry.name.into(),
            entry: 0,
            indexes: entry.indexes,
        }
    }

    #[test]
    fn test_ids_are_unique_and_every_entry_is_complete() {
        let ids: BTreeSet<_> = DEVIATIONS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DEVIATIONS.len(), "duplicate deviation id");
        for deviation in DEVIATIONS {
            assert!(!deviation.rule.is_empty() && !deviation.reason.is_empty(), "{}", deviation.id);
            assert!(
                !deviation.entries.is_empty(),
                "{}: a deviation that lists nothing",
                deviation.id
            );
            for entry in deviation.entries {
                assert!(!entry.name.is_empty(), "{}: {entry}", deviation.id);
                assert!(
                    entry.path.ends_with(".json") &&
                        !entry.path.starts_with('/') &&
                        !entry.path.contains('\\') &&
                        entry.path.split('/').all(|part| !part.is_empty() && part != ".."),
                    "{}: {} is not a path relative to `state_tests`",
                    deviation.id,
                    entry.path
                );
                assert!(
                    matches!(
                        entry.produced.kind(),
                        FailureKind::StateRootMismatch | FailureKind::LogsMismatch
                    ),
                    "{}: {entry}",
                    deviation.id
                );
            }
            assert_eq!(deviation.listed(deviation.fork).len(), deviation.entries.len());
            for fork in Fork::ALL.into_iter().filter(|fork| *fork != deviation.fork) {
                assert!(deviation.listed(fork).is_empty(), "{}", deviation.id);
            }
        }
    }

    /// Only an Osaka deviation lists blockchain tests, each complete, in path and name order, each
    /// once across the registry, its blocks in order and its causes naming earlier listed blocks;
    /// an entry is the test's wherever the release's `blockchain_tests` directory is.
    #[test]
    fn test_blockchain_entries() {
        let mut all = BTreeSet::new();
        for deviation in DEVIATIONS {
            if deviation.fork != Fork::Osaka {
                assert!(deviation.blockchain_entries.is_empty(), "{}", deviation.id);
            }
            let keys: Vec<_> =
                deviation.blockchain_entries.iter().map(|e| (e.path, e.name)).collect();
            assert!(
                keys.windows(2).all(|pair| pair[0] < pair[1]),
                "{}: out of order",
                deviation.id
            );
            for entry in deviation.blockchain_entries {
                assert!(all.insert((entry.path, entry.name)), "{entry} is listed twice");
                assert!(
                    !entry.name.is_empty() &&
                        entry.path.ends_with(".json") &&
                        !entry.path.starts_with('/') &&
                        entry.path.split('/').all(|part| !part.is_empty() && part != ".."),
                    "{}: {entry}",
                    deviation.id
                );
                // Blocks are listed once each, in block order, and a block whose state an earlier
                // block left names a listed block before it.
                assert!(!entry.blocks.is_empty(), "{entry}: lists no block");
                let indexes: Vec<_> = entry.blocks.iter().map(|b| b.produced.block).collect();
                assert!(indexes.windows(2).all(|pair| pair[0] < pair[1]), "{entry}: block order");
                for listed in entry.blocks {
                    assert_eq!(entry.block(listed.produced.block), Some(listed));
                    if let Cause::StateLeftBy(earlier) = listed.cause {
                        assert!(earlier < listed.produced.block, "{entry}: a later block named");
                        assert!(entry.block(earlier).is_some(), "{entry}: an unlisted block named");
                    }
                }
                if let ChainEnd::DeviatedState(reason) = entry.end {
                    assert!(!reason.is_empty(), "{entry}: an end without a reason");
                }
                // The entry is the test's wherever the release's directory is, and nowhere else.
                for dir in ["", "fixtures/main/blockchain_tests/", "/tmp/x/blockchain_tests/"] {
                    let id = blockchain::TestId {
                        path: format!("{dir}{}", entry.path),
                        name: entry.name.into(),
                    };
                    assert_eq!(deviation.blockchain_entry(&id), Some(entry));
                    let (found, _) = blockchain_entry(DEVIATIONS, &id).expect("listed");
                    assert_eq!(found.id, deviation.id);
                }
                let elsewhere = blockchain::TestId {
                    path: format!("x{}", entry.path),
                    name: entry.name.into(),
                };
                assert!(blockchain_entry(DEVIATIONS, &elsewhere).is_none());
                let renamed = blockchain::TestId {
                    path: entry.path.into(),
                    name: format!("{}x", entry.name),
                };
                assert!(blockchain_entry(DEVIATIONS, &renamed).is_none());
            }
        }
    }

    /// Entries are listed in path, name and index order, each once, and no entry is listed by two
    /// deviations.
    #[test]
    fn test_entries_are_ordered_and_disjoint() {
        let mut all = BTreeSet::new();
        for deviation in DEVIATIONS {
            let keys: Vec<_> =
                deviation.entries.iter().map(|e| (e.path, e.name, e.indexes)).collect();
            assert!(
                keys.windows(2).all(|pair| pair[0] < pair[1]),
                "{}: entries out of order or listed twice",
                deviation.id
            );
            for key in keys {
                assert!(all.insert((deviation.fork, key)), "{key:?} is listed twice");
            }
        }
    }

    /// Every listed entry is attributed to its deviation with the hashes it lists, wherever the
    /// release's `state_tests` directory is; with any other hashes, or none, it is attributed to
    /// nothing, and neither is the same entry of another fork.
    #[test]
    fn test_an_entry_is_explained_only_with_its_hashes() {
        for deviation in DEVIATIONS {
            for entry in deviation.entries {
                for dir in ["", "fixtures/main/state_tests/", "/tmp/x/state_tests/"] {
                    let id = id(entry, dir);
                    let explained =
                        attribute(DEVIATIONS, deviation.fork, &id, Some(entry.produced));
                    assert_eq!(explained.map(|d| d.id), Some(deviation.id), "{entry}");
                }
                let id = id(entry, "fixtures/");
                let other = match entry.produced {
                    Produced::StateRoot(root) => {
                        Produced::Logs { logs: B256::repeat_byte(1), state_root: root }
                    }
                    Produced::Logs { state_root, .. } => Produced::StateRoot(state_root),
                };
                assert!(attribute(DEVIATIONS, deviation.fork, &id, Some(other)).is_none());
                assert!(attribute(DEVIATIONS, deviation.fork, &id, None).is_none());
                for fork in Fork::ALL.into_iter().filter(|fork| *fork != deviation.fork) {
                    assert!(attribute(DEVIATIONS, fork, &id, Some(entry.produced)).is_none());
                }
            }
        }
    }

    /// An entry is its own test and indices in a file whose path ends with the entry's path, on
    /// a path component boundary.
    #[test]
    fn test_an_entry_is_matched_by_its_whole_id() {
        let entry = Entry {
            path: "cancun/a.json",
            name: "t",
            indexes: (1, 2, 3),
            produced: Produced::StateRoot(B256::ZERO),
        };
        let at = |path: &str, name: &str, indexes| TestId {
            path: path.into(),
            name: name.into(),
            entry: 0,
            indexes,
        };
        assert!(entry.is(&at("cancun/a.json", "t", (1, 2, 3))));
        assert!(entry.is(&at("fixtures/state_tests/cancun/a.json", "t", (1, 2, 3))));
        assert!(entry.is(&at("fixtures\\state_tests\\cancun\\a.json", "t", (1, 2, 3))));
        assert!(!entry.is(&at("fixtures/xcancun/a.json", "t", (1, 2, 3))));
        assert!(!entry.is(&at("fixtures/cancun/a.json/b.json", "t", (1, 2, 3))));
        assert!(!entry.is(&at("fixtures/cancun/a.json", "u", (1, 2, 3))));
        assert!(!entry.is(&at("fixtures/cancun/a.json", "t", (1, 2, 4))));
        assert!(!entry.is(&at("fixtures/cancun/a.json", "t", (0, 2, 3))));
    }

    /// `DEVIATIONS.md` is the rendered registry. `UPDATE_DEVIATIONS=1` writes it instead.
    #[test]
    fn test_the_rendered_registry_is_up_to_date() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("DEVIATIONS.md");
        let rendered = render_markdown();
        if std::env::var_os("UPDATE_DEVIATIONS").is_some() {
            std::fs::write(&path, &rendered).expect("the file is writable");
            return;
        }
        let checked_in = std::fs::read_to_string(&path).expect("DEVIATIONS.md is checked in");
        assert_eq!(
            checked_in, rendered,
            "DEVIATIONS.md is out of date; regenerate it with \
             UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations"
        );
    }
}
