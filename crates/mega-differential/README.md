# mega-differential

The differential harness of the Satin engine.
It runs every scenario of a corpus through `MegaEvm` and through stock revm 43, and compares what the two report, field by field.
Every difference must be explained by the deviation registry, and every effect an active registry entry lists must explain at least one difference.

```bash
cargo test -p mega-differential --locked
```

The corpus test prints one summary line, for example:

```text
differential: 323 scenarios, 12401 fields compared, 1004 deviations matched (op-fee-vault-touch x996, op-karst-bn254-pairing-input-bound x8), 0 unexplained, 0 stale registry effects
```

CI runs the same command as the `differential` job (`.github/workflows/differential.yml`), on every pull request and every push to `satin`.

## The two arms

**Left arm: `MegaEvm`.**
Each transaction runs on a fresh `MegaEvm` over a `MegaContext` for `MegaSpecId::SATIN`, through alloy-evm's `Evm::transact_raw` (system calls through `Evm::transact_system_call`).
The configuration is the one the spec fixes (`MegaContext::with_cfg`): the Osaka gas table, EIP-8037 and EIP-2780 on, the 200,000,000 execution cap, EIP-7708 and the system-call reservoir margin off.
The L1 fees are zero (`test_utils::zero_fee_l1_block_info`), every transaction is a non-deposit one with an empty envelope, and the gas price is zero.
`mega-evm` is built with its default features, so the precompiles run on the same backends as in a node.

**Right arm: the oracle.**
Stock `revm = "=43.0.0"` from crates.io, on its mainnet handler.
Its configuration is copied from the left arm's (`oracle::cfg`): the Ethereum spec the Satin spec runs on (Osaka), the gas table entry by entry, and every switch.
The fork numbers its gas ids as upstream does and adds its own at the top of the table (`tests/claims.rs` checks both, and that the fork-only prices are zero).
If `MegaEvm` switches on a mechanism revm 43 cannot run (EIP-7708 before Amsterdam, the system-call state-gas margin), `oracle::cfg` panics: the mechanism that switches it on has to model it or register the difference.
revm 43's `ResultGas` has no `reservoir_remaining`; the oracle reads the reservoir from the frame's gas at the point the fork records it (a handler that wraps revm 43's mainnet handler and only reads; a unit test checks it changes nothing else).

Both arms share the scenario format and its loader, `mega_evm::test_utils::Scenario`.

## How the two revms coexist

The workspace redirects `revm` to the MegaETH fork with `[patch.crates-io]`.
A patch only replaces a crates.io dependency whose version requirement the patched crate satisfies: the fork is `revm 40.0.3`, and this crate asks for `=43.0.0`, which it does not satisfy, so the requirement resolves to the registry.
Both revms therefore sit in this crate's graph, and only here: the crate is a workspace member but not a default member, and nothing else depends on it.

```bash
cargo tree -i revm --locked -p mega-evm        # one revm, the fork
cargo tree -p mega-differential -i revm --locked  # "ambiguous": revm@40.0.3 and revm@43.0.0
cargo tree -p mega-differential -i revm@43.0.0 --locked
cargo tree -p mega-differential -i revm@40.0.3 --locked
```

The oracle is built without revm's default features, so it adds no native precompile backend (c-kzg, blst, secp256k1) to the build; it runs the pure-Rust ones.
Its `dev` feature compiles the same optional configuration fields `mega-evm` compiles on the fork, so each of them is copied.

The lockfile is shared.
Cargo writes the version requirements of revm 43's optional dependencies into it even though they are not built, so adding the oracle moved two crates of the core's graph to newer semver-compatible releases: `c-kzg` 2.1.7 → 2.1.8 and `tracing-core` 0.1.34 → 0.1.36.

## The corpus

`scenarios/` holds one JSON file per scenario; the file stem is the scenario name.
The format is `mega_evm::test_utils::Scenario`: an optional description, a coinbase, the pre-state accounts (nonce, balance, code, storage; code is legacy bytecode or an EIP-7702 delegation designator) and the transactions, run in order.
A transaction is a `call`, a `create` or a `system_call`; it may carry an EIP-2930 access list and an EIP-7702 authorization list with the signer already recovered (`authority`, absent for a signature that does not recover).
The block is fixed: number 1, timestamp 1, zero base fee, unlimited gas.

| Directory      | Scenarios | Origin                                                                                                                             |
| -------------- | --------: | ---------------------------------------------------------------------------------------------------------------------------------- |
| `handwritten/` |        23 | the hand-written scenarios of the reference differential of the fork: state-gas spill and refill, creates, nested reverts and halts |
| `eest/`        |       263 | derived from the execution-spec-test Amsterdam state tests of EIP-8037 (state gas, its reservoir, and its interplay with the EIP-7623 calldata floor); the names keep the fixture test ids |
| `harness/`     |        37 | written for this harness, for what the reference corpus lacks: EIP-7702 authorizations, access lists, SELFDESTRUCT to existing accounts, logs, precompiles, and the three cases of `crates/mega-evm/tests/satin/equivalence.rs` |

The first two sets come from a reference harness that ran them at per-scenario state-gas prices and execution caps.
Satin fixes both, so the import dropped the `cap` and `prices` fields and moved every gas limit above the reference cap `C` to the Satin cap plus the same reservoir: `G' = 200,000,000 + (G − C)`.
Gas limits at or below the reference cap are unchanged, and the system call lost its gas limit, since a system call runs with the engine's own.
Under the Osaka gas table state gas is priced at zero, so today those scenarios exercise the reservoir, refunds and the regular-gas paths; their state-gas paths start to count when the Satin gas table prices state and the oracle takes the same table.

Each scenario of `harness/` carries a `description` of what it exercises.
To add a scenario, drop a JSON file into the directory of its origin; the loader rejects unknown fields, a name that is not the file stem, and a duplicate name.

## What is compared

For every transaction:

- the outcome: `success`, `revert`, `halt:<reason>` or `error:<reason>`, with op-revm's naming wrappers taken off;
- every gas figure: each field of `ResultGas` as it serializes (`gas_spent`, `state_gas_spent`, `gas_refunded`, `floor_gas`, `reservoir_remaining`), so a field the fork adds shows up as a difference, and the derived `tx_gas_used`, `block_regular_gas_used`, `block_state_gas_used` and `final_refunded`;
  history gas has no field of its own in `ResultGas` and is part of `gas_spent` and `block_regular_gas_used`;
- the output bytes and the created address;
- the logs;
- every touched account: created and self-destructed flags, balance, nonce, code hash, and the value of every slot the transaction changed.

## The deviation registry

`deviations.json` lists the accepted differences.
An entry names one mechanism and every effect it has on the compared fields:

```json
{
  "id": "op-fee-vault-touch",
  "status": "active",
  "scenario": "*",
  "mechanism": "op-revm fee distribution: the OP handler's reward_beneficiary",
  "reason": "…",
  "effects": [
    {
      "field": "tx[*].state[0x4200000000000000000000000000000000000019]",
      "left": "created=false selfdestructed=false balance=0x0 …",
      "right": "<absent>"
    }
  ]
}
```

- `scenario` and each effect's `field`, `left` (the `MegaEvm` value) and `right` (the oracle's) are patterns in which `*` stands for any run of characters.
  Keep them as narrow as the mechanism allows: exact values, and a scenario pattern that covers only the scenarios the mechanism reaches.
- `status` is `active` or `retired`; a retired entry keeps its history in `retired_reason` and explains nothing.
- The run fails on a difference no effect of an active entry explains, and on an effect of an active entry that explains nothing.
  When a mechanism stops producing an effect, remove the effect, or retire the entry when none is left.
- The `version` field is the format version this crate reads.

The active entries today:

- `op-fee-vault-touch`: after every non-deposit transaction op-revm credits the base fee, the L1 data fee and the operator fee to three vault predeploys, which touches them even at a zero amount.
  `tests/claims.rs` checks that state clearing drops those empty accounts when the state is committed.
- `op-karst-bn254-pairing-input-bound`: op-revm's Karst precompile set rejects a BN254 pairing input over 57,600 bytes, which Ethereum's pairing accepts; `harness/precompile_bn254_pairing_over_op_input_limit` crosses the bound and `harness/precompile_bn254_pairing_at_op_input_limit` stays on it.

## Relation to the equivalence tests

`crates/mega-evm/tests/satin/equivalence.rs` compares `MegaEvm` with op-revm's `OpEvm` on the same fork, which pins that the Satin layer adds nothing to op-revm yet.
This harness compares `MegaEvm` with an independent implementation, which pins that the fork plus op-revm execute Ethereum semantics.
The three equivalence cases are also scenarios here (`harness/equivalence_*`); the equivalence tests stay as they are, since they check a different claim that later changes update on purpose.
