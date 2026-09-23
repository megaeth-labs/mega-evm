# Deviations

<!-- Rendered from `src/deviations.rs` by `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`; do not edit. -->

The places Satin differs from Ethereum on purpose that the execution-spec gate meets.
In equivalence mode a failure one of them explains is counted against it, and the count is pinned; a failure none of them explains fails the gate.

## `amsterdam-opcodes-on-osaka`

2 failed tests on the pinned Osaka fixtures.

**Rule.**
Satin runs DUPN, SWAPN and EXCHANGE (EIP-8024) and SLOTNUM (EIP-7843), which its Osaka base leaves undefined.

**Reason.**
Satin takes the Amsterdam schedule and the opcodes with it.
The instruction table is Satin's machinery, which equivalence mode keeps, so an Osaka fixture that executes one of the four bytes, expecting the undefined-opcode halt Osaka gives it, runs the opcode instead.
With the four entries taken back out of the table, every Osaka fixture passes.

**Failures.** `state-root-mismatch`, in:

- `frontier/opcodes/test_all_opcodes.json`
- `stBadOpcode/undefinedOpcodeFirstByte.json`

## `selfdestruct-burns-on-osaka`

37 failed tests on the pinned Amsterdam fixtures.

**Rule.**
A contract self-destructed in the transaction that created it is removed as on Satin's Osaka base, a balance it sent to itself burned: EIP-8246, which keeps that balance and clears the account only at the end of the transaction, is not active.

**Reason.**
revm gates EIP-8246 on the Amsterdam spec id, in the journal, and Satin's spec runs on Karst, an Osaka base: Satin takes Amsterdam's rules one switch at a time — EIP-8037, EIP-2780, the opcodes — and EIP-8246 has no switch, so no configuration can turn it on.
An Amsterdam fixture in which a contract destructs to itself expects the balance kept, and the EIP-7708 transfer logs a kept balance produces when it is sent on.
revm itself, on an Osaka base with Amsterdam's configuration, produces exactly the post-state and logs Satin does for every one of these fixtures, and the fixtures' own post-state and logs on the Amsterdam spec.
Whether Satin takes EIP-8246 is a decision for its specification, not for this gate.

**Failures.** `state-root-mismatch`, `logs-mismatch`, in:

- `amsterdam/eip2780_reduce_intrinsic_tx_gas/top_frame_charges/initcode_selfdestruct_keeps_top_frame_state_charge.json`
- `amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_call/call_value_to_self_destructed_same_tx_account.json`
- `amsterdam/eip8037_state_creation_gas_cost_increase/state_gas_selfdestruct/selfdestruct_to_self_in_create_tx.json`
- `amsterdam/eip8038_state_access_gas_cost_increase/selfdestruct_gas/same_tx_created_selfdestruct_self_burn.json`
- `amsterdam/eip8246_selfdestruct_no_burn/selfdestruct_no_burn/create_transaction_initcode_selfdestruct.json`
- `cancun/eip6780_selfdestruct/selfdestruct_revert/selfdestruct_created_in_same_tx_with_revert.json`
- `cancun/eip6780_selfdestruct/selfdestruct/create_selfdestruct_same_tx.json`
- `cancun/eip6780_selfdestruct/selfdestruct/self_destructing_initcode.json`
- `frontier/create/create_suicide_during_init/create_suicide_during_transaction_create.json`
- `ported_static/stCreate2/create2_suicide/create2_suicide.json`
- `ported_static/stInitCodeTest/transaction_create_suicide_in_initcode/transaction_create_suicide_in_initcode.json`
