# Deviations

<!-- Rendered from `src/deviations.rs` by `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`; do not edit. -->

The places Satin differs from Ethereum on purpose that the execution-spec gate meets, each with the fixture entries it explains and the hashes Satin produces for them.
In equivalence mode a failure is a deviation's only when the deviation lists its entry with the hashes it produced, and every listed entry must fail exactly as listed: a failure no deviation explains, and a listed entry that passes or fails another way, fail the gate.

## `amsterdam-opcodes-on-osaka`

2 failed entries of the pinned Osaka fixtures.

**Rule.**
Satin runs DUPN, SWAPN and EXCHANGE (EIP-8024) and SLOTNUM (EIP-7843), which its Osaka base leaves undefined.

**Reason.**
Satin takes the Amsterdam schedule and the opcodes with it.
The instruction table is Satin's machinery, which equivalence mode keeps, so an Osaka fixture that executes one of the four bytes, expecting the undefined-opcode halt Osaka gives it, runs the opcode instead.
With the four entries taken back out of the table, every Osaka fixture passes.

**Failures.** Each entry with its data, gas and value indices, how it fails and the hashes Satin produces; paths are relative to the release's `state_tests` directory.

- `frontier/opcodes/test_all_opcodes.json`
  - `tests/frontier/opcodes/test_all_opcodes.py::test_all_opcodes[fork_Osaka-state_test]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x2790bb00bde351feb12b3607acea67097e9f7d0a3913d61c7269c3603a77b8c5`
- `static/state_tests/stBadOpcode/undefinedOpcodeFirstByte.json`
  - `tests/static/state_tests/stBadOpcode/undefinedOpcodeFirstByteFiller.yml::undefinedOpcodeFirstByte[fork_Osaka-state_test-]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xefc47fa41b70e7a1b1aa1ac3c0b04af29ccb2ed2f766fe42ff764e1f3b9fe0de`

## `selfdestruct-burns-on-osaka`

37 failed entries of the pinned Amsterdam fixtures.

**Rule.**
A contract self-destructed in the transaction that created it is removed as on Satin's Osaka base, a balance it sent to itself burned: EIP-8246, which keeps that balance and clears the account only at the end of the transaction, is not active.

**Reason.**
revm gates EIP-8246 on the Amsterdam spec id, in the journal, and Satin's spec runs on Karst, an Osaka base: Satin takes Amsterdam's rules one switch at a time — EIP-8037, EIP-2780, the opcodes — and EIP-8246 has no switch, so no configuration can turn it on.
An Amsterdam fixture in which a contract destructs to itself expects the balance kept, and the EIP-7708 transfer logs a kept balance produces when it is sent on.
revm itself, on an Osaka base with Amsterdam's configuration, produces exactly the hashes listed for every one of these entries, and the fixtures' own on the Amsterdam spec.
Whether Satin takes EIP-8246 is a decision for its specification, not for this gate.

**Failures.** Each entry with its data, gas and value indices, how it fails and the hashes Satin produces; paths are relative to the release's `state_tests` directory.

- `for_amsterdam/amsterdam/eip2780_reduce_intrinsic_tx_gas/top_frame_charges/initcode_selfdestruct_keeps_top_frame_state_charge.json`
  - `tests/amsterdam/eip2780_reduce_intrinsic_tx_gas/test_top_frame_charges.py::test_initcode_selfdestruct_keeps_top_frame_state_charge[fork_Amsterdam-state_test-non-zero_value-self_beneficiary]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x8a227aa6adf983ba803d01b09126eb9d226cf95a1e083f194c39f0e29e8a1e44`
- `for_amsterdam/amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_call/call_value_to_self_destructed_same_tx_account.json`
  - `tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_call.py::test_call_value_to_self_destructed_same_tx_account[fork_Amsterdam-state_test-create2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x5857f66aee23a75fbc2b874e871eb146226ff2dab2194b4597cb87b9b954347c`
  - `tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_call.py::test_call_value_to_self_destructed_same_tx_account[fork_Amsterdam-state_test-create]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x13370ccc6e5eb2bf5a75774bef80223d973990a429e9a3a6e2abe32eea70d091`
- `for_amsterdam/amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_selfdestruct/selfdestruct_to_self_in_create_tx.json`
  - `tests/amsterdam/eip8037_state_creation_gas_cost_increase/test_state_gas_selfdestruct.py::test_selfdestruct_to_self_in_create_tx[fork_Amsterdam-state_test]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xd1e99d8e4322d56db48a9c6fbf91231d405a592394fe30943639d7bb8e4af3e9`
- `for_amsterdam/amsterdam/eip8038_state_access_gas_cost_increase/selfdestruct_gas/same_tx_created_selfdestruct_self_burn.json`
  - `tests/amsterdam/eip8038_state_access_gas_cost_increase/test_selfdestruct_gas.py::test_same_tx_created_selfdestruct_self_burn[fork_Amsterdam-state_test]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x60ec6fdd1f4b9afbf1f6bae43abbc3a21fc917cf921bc7f43827d2dda86f411b`
- `for_amsterdam/amsterdam/eip8246_selfdestruct_no_burn/selfdestruct_no_burn/create_transaction_initcode_selfdestruct.json`
  - `tests/amsterdam/eip8246_selfdestruct_no_burn/test_selfdestruct_no_burn.py::test_create_transaction_initcode_selfdestruct[fork_Amsterdam-state_test-kept]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x8a227aa6adf983ba803d01b09126eb9d226cf95a1e083f194c39f0e29e8a1e44`
- `for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE2]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x5c00fce098c2fe994de7c23a67990d729c20a258651d132e2b21c3a2d0b5803a`, state root `0x2f2356140f67ac4504ff5f416e56edd0264e8ec19ed39ac6f8897b9023d27307`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x6aa38fb5b844e13cd8efa6b776f957550e0a4b27815a2f34cf3fa9bc4cae730e`, state root `0xf08b417e5ac7138f3e58e07dc90970a2780901935979e0a3010ba8ebe5aae924`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE2]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0xc6f151ee88a456efe8faee5993fac962f32645dd408bdf75e86a411e4d2b5b35`, state root `0xd42fe0ac1ad09661381fc6c43ee67a7c169bcb4610e89ec0c60aaa793d8831c4`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0xf76e4fa7500c4db34112ba382a1365e5503307c1ae300be79fa59550ba4eef32`, state root `0x9903a0c8f461940cadd5f48ea90bddfe1f019182c65d3f6a6b2533d336199063`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x2c2102d3296f1610fdf87d1182226cd4e793882caa71c96f965f0e5e7af21590`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x7cb1c8d882ce555bf85cff9961ebde8c14542d96d2394935edbf95b745c5265f`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_single_self_recipient-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xa695e4f522a7e47293590084faa0dc45ab419b7c0a2449808fd7e2a716968c80`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-multiple_calls_single_self_recipient-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x97844214d20796ee78c52c1d5a6338e21dfa8bdf6031a19cc359b9c46d5da076`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE2]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x5c00fce098c2fe994de7c23a67990d729c20a258651d132e2b21c3a2d0b5803a`, state root `0x7db1f7e8a7499e52962e8b457cbaa28545b035ec6d3491621ad8a10f0abcfec4`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self-create_opcode_CREATE]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x6aa38fb5b844e13cd8efa6b776f957550e0a4b27815a2f34cf3fa9bc4cae730e`, state root `0x7ae7c17d34642ed4d4a56e2613cc50ab5eeb617b5b2c965f946de58060b99447`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE2]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x79545168c90d8fed276adcd2bfe3610a0ba259044ba117fb9cafb0bcbbf3a3df`, state root `0x4f389b292dedd1e7effa9c1947e9ec9fb83a40f049f9cf4b87262e1d96c9682a`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_repeating_sendall_recipients_including_self_last-create_opcode_CREATE]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x7c73347bddfb53f07057096e6949c82c32d3fb61532a008261cbf1bf43fcd19e`, state root `0x254f07b354bf3e97b96e4a06ddee51489ca23a71fb6e088df9f460c35d880698`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self-create_opcode_CREATE2]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x4bd3e188f8d440a5762a67eb84a9d133a6e2979fb68d7414b1e0a08bcee83d48`, state root `0x807ff3a9c69267c578eae95089ba8087582eb630fe514aae537500b9edbcf880`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self-create_opcode_CREATE]` d=0 g=0 v=0: `logs-mismatch`, logs hash `0x88bd5130a835f0eb5cce6c8229e768ae96dec7a79cd757dca32b5e3ce57b86b0`, state root `0xe331c545e628b14d64deccaac85755e699f0e75aba8189847ed810e4a4f0ddf8`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xfbd1f3e7d906012db567e5429e1236dab7466d40798973c4dc4ba2984d621f72`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_multiple_sendall_recipients_including_self_last-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xd75f439c744aae2d05d0151919fa298073b99086c3af81087202b5c4464b2a02`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_single_self_recipient-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x1c0a70089f50f9da9183fa04210ae0f04affc03d146fcb78d32f3c5ef2853a47`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-multiple_calls_single_self_recipient-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x0c7614fbae22b3855669bfe3ee994bc18b31b32d84903a0372834f966a265b8d`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-single_call_self-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xaa95dfb767c86d4d4c261252b2a6224f32d15955eaf92650c4c97bb5c7892d95`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_create_selfdestruct_same_tx[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-single_call_self-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x9c91de53b0bc52572ca1c1ea6ccffcf42f725876a8f439ae676b7cdf9567d43a`
- `for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-call_times_2-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xdc78c509c41771960af990fbbcf5dbd0e6f72e8a6615bcb5cc1b75c8f12dc0bd`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_0-call_times_2-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x8eda22cdda403dd638ac173e1e6e648414d1d20aa8559e3a26af76939eda5d4b`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-call_times_2-create_opcode_CREATE2]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x998432460e11445f1529c1fde7e70e3c97818e9af0d2b3180e2329bd719d5e4c`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct.py::test_self_destructing_initcode[fork_Amsterdam-state_test-selfdestruct_contract_initial_balance_100000-call_times_2-create_opcode_CREATE]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x6989f4cf277b4f10675e678976e0ffde965e2d23634946b31f42e61b948fe31f`
- `for_amsterdam/cancun/eip6780_selfdestruct/selfdestruct_revert/selfdestruct_created_in_same_tx_with_revert.json`
  - `tests/cancun/eip6780_selfdestruct/test_selfdestruct_revert.py::test_selfdestruct_created_in_same_tx_with_revert[fork_Amsterdam-state_test-outer_selfdestruct_before_inner_call-same_tx]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x68de4499d08d8c9e6260ade80ed6a4203630f49dc1947b2312d308a7fd104e40`
- `for_amsterdam/frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json`
  - `tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_False]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x723798b921cd0f3a700aa00dffcb87da7df2440def96d3ed96b36e2bc0a1e545`
  - `tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_True]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xd82f53b114fbab4d8784e89069ef7b272e1d74f2cc26a363f96141b9eb771b6c`
  - `tests/frontier/create/test_create_suicide_during_init.py::test_create_suicide_during_transaction_create[fork_Amsterdam-create_opcode_CREATE2-state_test-operation_Operation.SUICIDE_TO_ITSELF-transaction_create_False]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x52695eb8bf3c83b205d4cfd24acb88af856a2db2ec5236e66202833d8aa94f3c`
- `for_amsterdam/ported_static/stCreate2/create2_suicide/create2_suicide.json`
  - `tests/ported_static/stCreate2/test_create2_suicide.py::test_create2_suicide[fork_Amsterdam-state_test-d6]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xe6d927efd83aabb7d94664d06b79744a361a74374f377f6c8dd31307e577ccde`
  - `tests/ported_static/stCreate2/test_create2_suicide.py::test_create2_suicide[fork_Amsterdam-state_test-d7]` d=0 g=0 v=0: `state-root-mismatch`, state root `0x087df75fad91a65e75b9373ad835882429c431166f9a3e6d086206cd39e2f662`
- `for_amsterdam/ported_static/stInitCodeTest/transaction_create_suicide_in_initcode/transaction_create_suicide_in_initcode.json`
  - `tests/ported_static/stInitCodeTest/test_transaction_create_suicide_in_initcode.py::test_transaction_create_suicide_in_initcode[fork_Amsterdam-state_test]` d=0 g=0 v=0: `state-root-mismatch`, state root `0xf6456935a8fdccc4fa60c10233d27e602088fdfbce68c9db41363767cffd367d`
