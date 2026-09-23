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

    /// Whether a failure of the entry `id` of `fork`, which produced `produced`, is this
    /// deviation's: the entry is listed, with exactly those hashes.
    pub fn explains(&self, fork: Fork, id: &TestId, produced: Option<Produced>) -> bool {
        produced.is_some_and(|produced| {
            self.listed(fork).iter().any(|entry| entry.produced == produced && entry.is(id))
        })
    }
}

impl Entry {
    /// Whether `id` is this entry: the same test and indices, in a file whose path ends with this
    /// entry's path.
    pub fn is(&self, id: &TestId) -> bool {
        id.name == self.name && id.indexes == self.indexes && {
            let path = id.path.replace('\\', "/");
            path.strip_suffix(self.path).is_some_and(|dir| dir.is_empty() || dir.ends_with('/'))
        }
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
