---
description: MegaETH compute gas accounting specification — the measurement window that derives compute gas from inherited EVM gas, per-opcode metering classes, non-opcode recording sites, transaction and per-call-frame limits, and exceed behavior.
spec: Rex6
---

# Compute Gas Accounting

[Compute gas](../glossary.md#compute-gas) is one of the two dimensions of MegaETH's [dual gas model](dual-gas-model.md), and is independently bounded by a per-transaction limit.
This page specifies how a node derives compute gas from inherited EVM gas: where each measurement window opens and closes, what is excluded from it, which sites record compute gas, and what happens when a limit is exceeded.

## Motivation

MegaETH does not define its own opcode gas schedule.
Compute gas is a _derived measurement_ of the EVM gas inherited from Optimism Isthmus / Ethereum Prague: a node observes how much EVM gas an operation consumed, removes the components that are not computation, and records the remainder.

That derivation is consensus-critical for two reasons.

First, compute gas is itself a metered resource with its own limit.
When a transaction halts on that limit, the exact amount recorded determines _which_ limit was reached first and _at which opcode_ the halt lands.
Two implementations that agree on every opcode's EVM gas cost can still disagree on the transaction outcome if they open their measurement windows at different points.

Second, MegaETH charges [storage gas](../glossary.md#storage-gas) out of the same EVM gas budget as computation, and forwards gas into child frames that perform their own accounting.
Without a precise rule for what is removed from the measurement, a node would double-count storage surcharges as computation, or count a child frame's budget twice.

Stating only that "each opcode costs the same compute gas as it does on Ethereum" is insufficient: it fixes the gas _schedule_ but leaves the _measurement_ undefined.
This page defines the measurement.

## Specification

The named constants referenced in this section are defined later in [Constants](#constants).

### Overview

A node MUST track compute gas as a per-transaction resource across the whole of transaction execution.
Compute gas usage is the sum of the amounts recorded at every recording site defined on this page.

Compute gas metering begins at [MiniRex](../upgrades/minirex.md).
Under the Equivalence spec, a node MUST NOT track compute gas and MUST NOT enforce any compute gas limit; execution is bounded only by the standard EVM gas limit.

Compute gas is not an independent gas schedule.
Unless explicitly overridden elsewhere in this specification, the EVM gas cost of every operation is inherited unchanged from Optimism Isthmus / Ethereum Prague.
What this page defines is how much of that inherited cost is _recorded_ as compute gas.
The one departure from inherited pricing that this page itself defines is specified next.

### Inherited-Cost Exception: Preload-Warm Addresses

The inherited EVM treats some addresses as warm from the start of every transaction without loading them: precompile addresses, access-list addresses listed without storage keys, and the block [beneficiary](../glossary.md#beneficiary).
MegaETH departs from the inherited account-access pricing on the first touch of such an address.

When the first access to such an address in a transaction is made by one of the opcodes below, the opcode MUST charge the cold account access cost in place of the warm cost:

| Opcode                       | Charged cold since                                                                                                                     |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `CALL`                       | MiniRex                                                                                                                                |
| `DELEGATECALL`, `STATICCALL` | [Rex](../upgrades/rex.md)                                                                                                              |
| `CALLCODE`                   | Rex through [Rex4](../upgrades/rex4.md); not charged under [Rex5](../upgrades/rex5.md); charged again from [Rex6](../upgrades/rex6.md) |
| `SELFDESTRUCT` (beneficiary) | Rex5                                                                                                                                   |

The rule covers the inspected address only.
A preload-warm _delegate_ keeps its inherited warmth for the delegate-access charge: where the pre-execution inspection resolves an EIP-7702 delegation, it reads the delegate's address, not the delegate's account.
Under MiniRex through Rex4 the inspection loaded the delegate's account instead, and the delegate address was charged cold on first touch as well.

`CALLCODE`'s two boundaries in the table both follow from which address that inspection materializes.
Under Rex5 the inspection targets the executing account, which is already warm, so the inherited pricing is restored.
From Rex6 the beneficiary detection for `CALL`-family targets is delegation-aware, and resolving the delegation materializes the call target — without its preloaded warmth — ahead of the inherited load, which reintroduces the cold first-touch charge.
The other three call opcodes already materialize their call targets, so their pricing is unaffected by that change.

The rule does not extend beyond that first touch or those opcodes:

- the same address is warm for every subsequent access in the transaction, as inherited;
- a first touch by any other opcode (`BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`) observes the inherited warmth;
- addresses loaded rather than merely preloaded — the transaction sender and recipient, access-list addresses listed with storage keys, and addresses created by `CREATE` / `CREATE2` — are unaffected.

The extra charge is ordinary EVM gas: it is debited from the transaction's gas budget, recorded as compute gas by the opcode's measurement window, and visible in the receipt's `gas_used`.

### Measurement Window

#### Recording Rule

A node MUST record an operation's compute gas as a single amount computed over a measurement window:

```
recorded = (gas_before − gas_after) − storage_gas_charged − forwarded_child_gas
```

Where:

- `gas_before` is the EVM gas remaining at the point the window opens,
- `gas_after` is the EVM gas remaining at the point the window closes,
- `storage_gas_charged` is the storage gas debited from the EVM gas budget inside the window, and
- `forwarded_child_gas` is the gas transferred into a child call frame inside the window.

Each subtraction is saturating: the recorded amount MUST NOT underflow below zero.

#### Window Boundaries

The window MUST cover all of the opcode's compute work: it MUST open before any EVM gas movement that will be recorded as compute gas, and MUST close after the inner opcode body has fully executed.

A storage-gas charge is the one movement that MAY fall on either side of the opening point, because it is excluded from the recorded amount either way — see [Storage Gas Exclusion](#storage-gas-exclusion).
A forwarding-cap adjustment applied after the inner body completes changes the remaining gas and the pending child's gas limit by the same amount, so a window closed before that adjustment records the same amount as one closed after it; both placements satisfy this rule.

The window for every storage-affecting opcode — `SSTORE`, `LOG0` through `LOG4`, `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`, and `SELFDESTRUCT` — MUST open before the opcode's storage-gas charge and before any wrapper-side EVM gas work, and MUST close after the inner opcode body completes.
An opcode MAY realize this canonical window through the equivalent charge-outside-window form of the [storage gas exclusion](#storage-gas-exclusion), opening after its storage-gas charge with `storage_gas_charged` treated as zero; the recorded amount is identical by construction.

The full canonical metering order is specified in [Dual Gas Model](dual-gas-model.md#gas-metering-order).

#### Single-Record Rule

A node MUST record an operation's compute gas exactly once per measurement window.

If the window does not close — because operand validation fails, a storage-gas charge exhausts the budget, or the operation halts partway through — the node MUST NOT record compute gas for that operation, even when EVM gas was already consumed by work performed before the halt.
The EVM gas consumed by such work remains deducted from the transaction's gas budget.

There is no per-opcode exception: every opcode, `CREATE2` included, records in exactly one window.

#### Storage Gas Exclusion

A node MUST NOT record [storage gas](../glossary.md#storage-gas) as compute gas.
The two dimensions are charged from the same EVM gas budget but accounted separately.

A node MAY implement the exclusion in either of two equivalent forms:

- by subtracting `storage_gas_charged` from a window that spans the storage-gas charge, or
- by opening the window after the storage-gas charge, so that the charge falls outside it.

Both forms MUST produce identical recorded amounts.
When the window is placed so that a storage-gas charge falls outside it, `storage_gas_charged` MUST be treated as zero for that window; subtracting it again would under-count compute gas.

#### Forwarded Gas Exclusion

A node MUST NOT record gas forwarded into a child call frame as the parent's compute gas.
The child frame records its own compute gas as it executes.

A node MUST compute `forwarded_child_gas` as:

```text
if the opcode produced no pending child call:
    forwarded_child_gas = 0
else if spec >= Rex5 and scheme in (CALL, CALLCODE) and child.value != 0:
    forwarded_child_gas = child.gas_limit - CALL_STIPEND
else:
    forwarded_child_gas = child.gas_limit
```

The condition is on what the opcode produced, not on what frame initialization later did with it.
A pending child call may still resolve without any child EVM frame executing — as a precompile invocation, a [system contract interception](#system-contract-interception), or a too-deep call result.
The parent has already parted with the gas in every one of those cases, so it MUST be excluded from the parent's compute gas all the same; whatever runs in the child's place accounts for its own consumption.

`child.gas_limit` is the gas limit the opcode itself assigns to the pending child frame.
It is not the limit the child ultimately runs with: under Rex4 the [storage gas stipend](../glossary.md#storage-gas-stipend) is granted by inflating that limit after the parent's measurement window has already closed.
A node MUST use the pre-inflation value here on every spec; subtracting the inflated one would under-record the parent by the stipend amount.

All three conditions in the `CALL_STIPEND` branch MUST hold together.
A `DELEGATECALL` or `STATICCALL` never satisfies it (wrong scheme), a zero-value `CALL` never satisfies it (no stipend is granted), and no spec before Rex5 satisfies it.
`CREATE` and `CREATE2` always fall through to the final branch: contract creation carries no stipend.

The `CALL_STIPEND` adjustment exists because the stipend is added to the child's gas limit without being deducted from the parent's remaining gas.
Treating it as forwarded would under-count the parent's compute gas by the stipend amount.

`CALLCODE`, `DELEGATECALL`, and `STATICCALL` are subject to the same [98/100 forwarding cap](gas-forwarding.md) as `CALL`.
Under MiniRex only, these three opcodes do not apply the cap, and therefore deduct the child's uncapped gas limit from the parent's recorded compute gas; Rex brought them under the cap.

#### Refund Exclusion

A node MUST NOT subtract EVM gas refunds from compute gas usage.
Refunds affect final gas settlement but do not reduce the compute gas recorded during execution.

### Opcode Metering Classes

Every opcode belongs to exactly one metering class.
The class determines which exclusions apply to its measurement window and which resource dimensions are checked after it records.

| Class            | Storage gas exclusion | Forwarded gas exclusion | Dimensions checked after recording |
| ---------------- | --------------------- | ----------------------- | ---------------------------------- |
| **Plain**        | Not applicable        | Not applicable          | Compute gas only                   |
| **Volatile**     | Not applicable        | Not applicable          | Compute gas only                   |
| **Storage**      | Applies               | Not applicable          | Compute gas only                   |
| **Call**         | Applies               | Applies                 | Compute gas only                   |
| **Create**       | Applies               | Applies                 | Compute gas only                   |
| **SelfDestruct** | Applies               | Not applicable          | All four dimensions                |
| **Untracked**    | Not applicable        | Not applicable          | None — no compute gas recorded     |

A **Volatile** opcode is a Plain opcode that additionally applies a [gas detention](gas-detention.md) cap after it executes.
Its measurement window is identical to a Plain opcode's.

A **SelfDestruct** opcode checks all four resource dimensions after recording, rather than compute gas alone.
This is required because `SELFDESTRUCT` records its data-size, KV-update, and state-growth contributions _before_ its inner instruction runs, without latching an exceed; those dimensions must be latched only once the inner instruction has succeeded.
Every other opcode records its non-compute dimensions after its inner instruction and latches at the recording site, so a compute-gas-only check suffices.
When that check finds more than one dimension over its limit, the reported dimension follows the fixed priority specified in [Multidimensional Resource Limits](resource-limits.md#runtime-transaction-level-limits).

#### Class Assignment

The current assignment is:

| Class            | Opcodes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Plain**        | `STOP`; `ADD`–`SIGNEXTEND`; `LT`–`SAR`; `KECCAK256`; `ADDRESS`, `ORIGIN`, `CALLER`, `CALLVALUE`, `CALLDATALOAD`, `CALLDATASIZE`, `CALLDATACOPY`, `CODESIZE`, `CODECOPY`, `GASPRICE`, `RETURNDATASIZE`, `RETURNDATACOPY`, `CHAINID`; `POP`, `MLOAD`, `MSTORE`, `MSTORE8`, `JUMP`, `JUMPI`, `PC`, `MSIZE`, `GAS`, `JUMPDEST`, `TLOAD`, `TSTORE`, `MCOPY`; `PUSH0`–`PUSH32`; `DUP1`–`DUP16`; `SWAP1`–`SWAP16`; `RETURN`, `REVERT`, `INVALID`                                                                                                      |
| **Volatile**     | Unconditional: `BLOCKHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `BLOBHASH`.<br>Volatile when the stack target is the [beneficiary](../glossary.md#beneficiary): `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`.<br>Volatile when the executing contract is the beneficiary: `SELFBALANCE`.<br>Volatile when the executing contract is the [oracle](../system-contracts/oracle.md): `SLOAD`, subject to the system-address exemption specified in [Gas Detention](gas-detention.md). |
| **Storage**      | `SSTORE`, `LOG0`–`LOG4`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| **Call**         | `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **Create**       | `CREATE`, `CREATE2`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **SelfDestruct** | `SELFDESTRUCT`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **Untracked**    | Every opcode not available in the inherited EVM, including `CLZ`. `CLZ` requires a base revision later than the one MegaETH inherits, so it always fails before its measurement window closes and records nothing; if the inherited base revision is ever raised, its class MUST be re-declared rather than inherited silently                                                                                                                                                                                                                 |

Under Equivalence every opcode is Untracked, because compute gas is not tracked at all.

Class membership is not the only volatility-related property an opcode can carry.
`CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, and `SELFDESTRUCT` remain in their own classes but additionally apply a beneficiary volatile-access guard before executing.
Their measurement windows follow their class; the guard decides whether the opcode runs, and when the call touches the beneficiary it also applies the detention cap after the opcode executes (see [Gas Detention](gas-detention.md)).

**How the assignment evolved.** Class assignment is defined by inheritance: each spec inherits the previous spec's assignment and overrides only the opcodes listed.
A node implementing replay for historical blocks needs every spec's assignment, so the base and each delta are given below.

**MiniRex** (base assignment):

| Class         | Opcodes                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Plain**     | `STOP`; `ADD`–`SIGNEXTEND`; `LT`–`SAR`; `KECCAK256`; `ADDRESS`, `ORIGIN`, `CALLER`, `CALLVALUE`, `CALLDATALOAD`, `CALLDATASIZE`, `CALLDATACOPY`, `CODESIZE`, `CODECOPY`, `GASPRICE`, `RETURNDATASIZE`, `RETURNDATACOPY`, `CHAINID`, `SELFBALANCE`; `POP`, `MLOAD`, `MSTORE`, `MSTORE8`, `SLOAD`, `JUMP`, `JUMPI`, `PC`, `MSIZE`, `GAS`, `JUMPDEST`, `TLOAD`, `TSTORE`, `MCOPY`; `PUSH0`–`PUSH32`; `DUP1`–`DUP16`; `SWAP1`–`SWAP16`; `RETURN`, `REVERT`, `INVALID` |
| **Volatile**  | `BLOCKHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `BLOBHASH` (unconditional); `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH` (conditional on the [beneficiary](../glossary.md#beneficiary))                                                                                                                                                                                                             |
| **Storage**   | `SSTORE`, `LOG0`–`LOG4`                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **Call**      | `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`                                                                                                                                                                                                                                                                                                                                                                                                                  |
| **Create**    | `CREATE`, `CREATE2`                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **Untracked** | `SELFDESTRUCT` (disabled — halts with an invalid-opcode result); every opcode undefined in the inherited EVM, including `CLZ`                                                                                                                                                                                                                                                                                                                                     |

Per-spec overrides, applied in order on top of the MiniRex base to reach the current assignment above:

| Spec                            | Override                                                                                                                                                                                                                                                                       |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Rex**                         | `CALLCODE`, `DELEGATECALL`, `STATICCALL` become subject to the 98/100 forwarding cap (class unchanged; the forwarded-gas exclusion changes accordingly, and `CALLCODE`'s storage gas exclusion becomes non-vacuous — it gains the empty-account value-transfer storage charge) |
| [**Rex1**](../upgrades/rex1.md) | No change                                                                                                                                                                                                                                                                      |
| [**Rex2**](../upgrades/rex2.md) | `SELFDESTRUCT`: Untracked → SelfDestruct (re-enabled with EIP-6780 semantics)                                                                                                                                                                                                  |
| [**Rex3**](../upgrades/rex3.md) | `SLOAD`: Plain → Volatile (conditional on the [oracle](../system-contracts/oracle.md) contract)                                                                                                                                                                                |
| **Rex4**                        | `SELFBALANCE`: Plain → Volatile (conditional on the beneficiary). `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, and `SELFDESTRUCT` gain a beneficiary volatile-access guard that runs before the opcode executes                                                           |
| **Rex5**                        | `SELFDESTRUCT` gains a storage-gas charge for empty-beneficiary creation (class unchanged; the storage gas exclusion becomes applicable)                                                                                                                                       |
| **Rex6**                        | No change                                                                                                                                                                                                                                                                      |

#### Contract Creation Memory Expansion

`CREATE2` expands memory to hash its initcode before the inner opcode runs.
The EVM gas consumed by that expansion is compute gas, and a node MUST record it inside the opcode's single measurement window, which opens before the memory expansion and closes after the inner opcode completes.
A halt between the two therefore records no compute gas for the opcode, per the [single-record rule](#single-record-rule); the EVM gas consumed by the expansion is still debited from the transaction's budget.

Earlier specs split the expansion into a window of its own, making `CREATE2` the one opcode that recorded twice.
Under Rex5 that expansion window records first, before the contract-creation storage gas is charged and ahead of the opcode's main window.
Under MiniRex through Rex4 it instead records last — after the inner opcode completes and after the main window records — and is skipped if the inner opcode fails, or if the main window's recording fails the opcode on a compute-gas exceed.

### Non-Opcode Recording Sites

Compute gas is recorded outside opcode execution at the following sites.

Taken together with the per-opcode windows, they close the transaction's compute-gas accounting:

```
compute_gas_used = intrinsic_recorded
                 + sum(recorded(opcode window))
                 + sum(recorded(precompile invocation))
                 + sum(code_deposit_recorded(contract creation))
                 + sum(keyless_deploy_recorded)
```

`sum(recorded(opcode window))` sums the per-opcode windows defined in [Measurement Window](#measurement-window); every other term is one of the subsections below.
A node MUST NOT record compute gas at any site not listed here.
`sum(recorded(opcode window))` sums over every window closed during execution — including windows in call frames that later reverted, since compute gas is not reverted.
The `keyless_deploy_recorded` term is zero before Rex3.

#### Transaction Intrinsic Gas

Before the first call frame begins, a node MUST record the transaction's standard EVM intrinsic gas as compute gas.

This amount MUST be the whole of the intrinsic gas the inherited EVM defines for the transaction, and nothing beyond it.
For every transaction that means the base transaction cost, the calldata token cost, the access-list cost, and the EIP-7702 authorization-list cost.
A contract-creation transaction additionally carries the inherited creation surcharge and the EIP-3860 per-initcode-word charge, and both are part of this amount.
A node MUST NOT include MegaETH's intrinsic storage gas additions in the recorded compute gas, even though they are added to the same intrinsic gas total charged against the transaction's gas limit.
Those additions comprise every storage gas component folded into the intrinsic total: the calldata storage gas and the flat transaction intrinsic storage gas defined in [Dual Gas Model](dual-gas-model.md), and the recipient account-creation or contract-creation storage gas charged when the transaction materializes its target.

This recording is made outside any call frame.
It therefore reduces the budget available to the top-level call frame, as specified in [Per-Call-Frame Budget](#per-call-frame-budget).

#### Precompiles

The gas limit a node forwards into a precompile is capped at the remaining compute gas budget, as specified in [Precompile Compute-Gas Bound](resource-limits.md#precompile-compute-gas-bound).
Call that capped value the **effective gas limit**:

```
effective_gas_limit   = min(call_gas_limit, remaining_compute_gas)
remaining_compute_gas = min(current frame's remaining budget,
                            effective_limit − compute_gas_used)
```

A precompile is therefore bounded by whichever of the [per-call-frame budget](#per-call-frame-budget) and the detained [transaction budget](#effective-limit) binds first.

For a transaction whose recipient is a precompile, the invocation runs before any call-frame budget exists.
The frame term of `remaining_compute_gas` is then the transaction-level remaining budget itself — the effective limit minus the compute gas already recorded, including the [transaction intrinsic gas](#transaction-intrinsic-gas) — so the cap reduces to `effective_limit − compute_gas_used`.

A node MUST record a precompile invocation's compute gas as:

```text
if the precompile returned or reverted:
    recorded = gas_spent
else if target is the KZG point-evaluation precompile
        and effective_gas_limit >= KZG_POINT_EVALUATION_GAS_COST:
    recorded = KZG_POINT_EVALUATION_GAS_COST
else:
    recorded = effective_gas_limit
```

The KZG branch covers that precompile reaching its verification step and returning a non-out-of-gas error.
Its condition is expressed structurally — on the effective gas limit rather than on the specific error returned — so that it remains correct if the inherited EVM adds further non-out-of-gas error variants.
No other precompile takes that branch.
`KZG_POINT_EVALUATION_GAS_COST` is a MegaETH override, not an inherited value; see [Precompiles](precompiles.md).

Both non-`gas_spent` branches exist because the precompile recorded no cost on those paths, so recording the spent amount — zero — would under-count the work performed.

On a halting precompile the recorded compute gas is deliberately distinct from the EVM gas the parent frame burns: the parent burns the caller-supplied call gas limit, while the compute gas recorded is the effective gas limit.
When the compute-gas cap is the binding constraint the two amounts differ, and a node MUST NOT reconcile them.

Earlier specs record less:

| Spec         | Recorded amount              |
| ------------ | ---------------------------- |
| MiniRex–Rex4 | The gas the precompile spent |
| Equivalence  | Not recorded                 |

Under MiniRex through Rex4, no compute-gas cap bounds the forwarded gas, and "the gas the precompile spent" is the amount the precompile itself recorded as consumed.
On a failing invocation no cost is recorded, so the compute gas recorded is zero — even though the parent frame still burns the forwarded gas limit.
Rex5 introduced the cap and the three-branch recording above.

#### Contract Creation Code Deposit

For any contract creation (`CREATE`, `CREATE2`, or a contract-creation transaction), a node MUST record the code-deposit compute gas — `code_length × CODEDEPOSIT` — exactly once, and only when the deposit's success conditions hold at the recording point.

The deposit does not occur, and a node MUST therefore record nothing, when the returned runtime code exceeds the maximum contract size, when it begins with the `0xEF` byte reserved by EIP-3541, or when the frame's remaining gas cannot cover the code-deposit charge.
These conditions apply on every spec; only the point at which the recording happens differs.

| Spec         | Recording point                                                                                                                                                                                    |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rex5+        | Atomically with the deployment commit: recorded when the deployment's pre-commit success conditions hold, at the same point the EVM charges the code-deposit gas and commits the created contract. |
| MiniRex–Rex4 | During frame-return processing, in the window covering the EVM's code-deposit charge.                                                                                                              |

A node MUST NOT record this amount twice.

The recording itself can latch a compute-gas exceed, and the two recording points then produce different deployment outcomes:

- From Rex5, the recording precedes the commit: the frame fails as specified in [Exceed Behavior](#exceed-behavior) and the deployment commits nothing, but the recorded amount stands — recording precedes exceed evaluation, and compute gas is never reverted.
- Under Rex4, the only earlier spec with a per-frame budget, the recording happens after the EVM has already charged the deposit and committed the created contract.
  A frame-budget exceed latched by this recording therefore produces a split outcome: the frame's result is the frame-local revert, while the deployed code remains committed.
  A node MUST NOT roll the deployment back on this path.

The code-deposit _storage_ gas is charged before this window opens and therefore falls outside it, consistent with the [storage gas exclusion](#storage-gas-exclusion).

#### Keyless Deploy Sandbox

From Rex3 onward, a node MUST record the [KeylessDeploy](../system-contracts/keyless-deploy.md) fixed dispatch overhead (`KEYLESS_DEPLOY_OVERHEAD_GAS`) as compute gas when that overhead is charged.

From Rex5 onward, a node MUST additionally merge the compute gas consumed inside the keyless-deploy sandbox into the enclosing transaction's compute gas usage.

#### System Contract Interception

An intercepted [system contract](../system-contracts/interception.md) call produces a synthetic frame result without executing a child EVM frame.

Where an interceptor performs no metering of its own, a node MUST NOT record compute gas for the interception: the forwarded gas is returned to the caller in full, so the interception consumes nothing beyond the `CALL`-family opcode that initiated it.

An interceptor that records compute gas of its own is the exception, and its charges are specified with that operation rather than here.
[KeylessDeploy](#keyless-deploy-sandbox) is the only interceptor that records compute gas: it records a fixed dispatch overhead from Rex3, and merges its sandbox's compute gas from Rex5.
A node MUST record those amounts even though the call reaches KeylessDeploy through the same interception framework, and even though such a call is necessarily top-level — KeylessDeploy intercepts only top-level transactions (see [Keyless Deployment](../system-contracts/keyless-deploy.md)) — and therefore has no initiating opcode.

An interception MUST NOT perturb per-call-frame budgets: the budgets observed by frames that run after it MUST be the same as if a real child frame had been entered and returned without recording compute gas.

### Limits

#### Transaction-Level Limit

A node MUST enforce a per-transaction compute gas limit of `TX_COMPUTE_GAS_LIMIT`.

#### Effective Limit

The effective transaction-level limit is the minimum of `TX_COMPUTE_GAS_LIMIT` and the current detained limit produced by [gas detention](gas-detention.md):

```
effective_limit = min(TX_COMPUTE_GAS_LIMIT, detained_limit)
```

How `detained_limit` is lowered by volatile-data access, how competing caps combine, and when it resets between transactions are specified in [Gas Detention](gas-detention.md).
This page only defines how the resulting limit is applied.

A node MUST evaluate the transaction-level check with strict greater-than:

```
tx_exceeded = compute_gas_used > effective_limit
```

Usage exactly equal to the effective limit is within limit.

#### Per-Call-Frame Budget

From Rex4 onward, a node MUST additionally enforce a per-call-frame compute gas budget.
Before Rex4, compute gas is enforced at the transaction level only, and no per-frame budget exists.

How a frame's budget is derived — the top-level frame's budget net of pre-frame usage, and the `FRAME_LIMIT_NUMERATOR` / `FRAME_LIMIT_DENOMINATOR` share forwarded to each nested frame — is specified for all four resource dimensions in [Per-Call-Frame Runtime Budgets](resource-limits.md#per-call-frame-runtime-budgets).
For compute gas, the pre-frame usage deducted from the top-level budget is the [transaction intrinsic gas](#transaction-intrinsic-gas).

Under Rex4 only, an internal value-transferring `CALL` or `CALLCODE` carries one further constraint.
Rex4 grants the [storage gas stipend](../glossary.md#storage-gas-stipend) by inflating the child's gas limit, so the child's compute gas budget MUST additionally be capped at the pre-inflation gas limit — otherwise the system-granted stipend becomes spendable on computation.
The child's budget is therefore the minimum of the forwarded share and that pre-inflation limit.
Rex5 replaced the inflation with a separated allowance that never enters the child's gas limit, so no such cap applies from Rex5 onward.

The per-frame check uses the same strict comparison as the transaction-level one, and a node MUST evaluate it before the transaction-level check.
A node MUST NOT skip the transaction-level check when the per-frame check is within limit: the intrinsic recording lies outside every frame budget, and the detained limit may be lowered at any point during execution.

{% hint style="info" %}
**Design intent.** The per-frame budget is a termination guardrail, not a budget guarantee.
Because compute gas is never reverted (see [Revert Behavior](#revert-behavior)), a child frame that exceeds its budget and reverts still contributes its full recorded usage to the transaction total.
The budget bounds how far a single frame may run; it does not protect the transaction's remaining budget from that frame's consumption.
{% endhint %}

### Exceed Behavior

A compute gas exceed is either _frame-local_ or _transaction-level_, and the two produce different outcomes.

| Condition                                 | Scope             | Result                                                                                              | Gas                                                 |
| ----------------------------------------- | ----------------- | --------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| Per-frame budget exceeded                 | Frame-local       | The frame MUST revert with `MegaLimitExceeded(uint8 kind, uint64 limit)`                            | Returns to the parent frame normally                |
| Transaction limit exceeded                | Transaction-level | The transaction MUST halt with `OutOfGas`                                                           | Remaining gas is rescued and refunded to the sender |
| Detained limit exceeded                   | Transaction-level | The transaction MUST halt with `VolatileDataAccessOutOfGas` (see [Gas Detention](gas-detention.md)) | Remaining gas is rescued and refunded to the sender |
| Keyless-deploy dispatch overhead exceeded | See below         | The frame MUST revert, or the transaction MUST halt with `OutOfGas`                                 | As for the matching scope above                     |

A frame-local exceed in a nested frame does not fail the transaction: the parent frame MAY continue execution.
The top-level frame also carries a budget, and a frame-local exceed there has no parent to return to — the transaction's own result becomes the revert, and the receipt reports failure.

On a transaction-level exceed, a node MUST preserve the frame's remaining gas for refund to the sender.
The rescued amount MUST exclude any portion contributed by the [storage gas stipend](../glossary.md#storage-gas-stipend), so that system-granted gas is not recovered by the sender.
The rule admits no exception: the keyless-deploy dispatch path rescues on the same terms as opcode dispatch.

Rescue is specific to a transaction-level exceed.
A frame-local exceed needs none: the frame reverts and its unspent gas returns to the parent through ordinary frame accounting.

When a `CALL`-family or `CREATE` / `CREATE2` opcode fails on a compute-gas exceed — the frame-local revert and the transaction-level halt alike — its pending child frame is discarded before the child runs.
A node MUST return the gas already forwarded to that discarded child to the frame before it terminates, so that gas is not charged as consumed: on a frame-local revert it returns to the parent frame, and on a transaction-level halt it is excluded from the transaction's `gas_used`.

A node MUST NOT halt a [system-originated transaction](../system-contracts/system-tx.md#system-originated-transaction-metering-exemption) on the compute gas limit or on gas detention.
The node MUST still record its compute gas usage; only the halt decision is suppressed.
The transaction's standard EVM `gas_limit` remains the only bound that can halt it.

A node MUST record compute gas before evaluating any exceed, including an exceed already latched on another resource dimension.
The compute work was performed, and the recorded total feeds the transaction outcome and the block-level compute accounting even for a transaction halted on a different dimension.

<details>
<summary>Rex7 (unstable): checkpoint settlement and gas-clamp enforcement</summary>

Rex7 replaces per-opcode recording for plain opcodes with checkpoint settlement, and enforces compute-gas and detention limits inside plain segments by clamping interpreter-visible gas.
The full previous/new pairing is on the [Rex7 Network Upgrade](../upgrades/rex7.md) page; the normative rules for implementers follow.

#### Checkpoint set

A node MUST settle compute gas at each of the following **checkpoints**, and MUST NOT open a per-opcode measurement window for any other opcode:

- storage-gas opcodes: `SSTORE`, `LOG0`–`LOG4`, `SELFDESTRUCT`;
- call-family opcodes: `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`;
- create opcodes: `CREATE`, `CREATE2`;
- volatile / detention-guarded opcodes: the unconditional block-environment set, the beneficiary-conditional set, and oracle-conditional `SLOAD` (same membership as the Volatile class and the call-family / `SELFDESTRUCT` beneficiary guards above);
- the `GAS` opcode;
- frame entry, frame resume after a child returns, and frame exit.

Plain opcodes between checkpoints MUST run without recording compute gas when they finish.

#### Segment settlement

At each checkpoint a node MUST:

1. Settle the open plain-opcode segment as the interpreter-gas delta since the previous checkpoint or frame open/resume, applying the same storage-gas and forwarded-child exclusions as the checkpoint opcode's measurement window under this page's stable rules.
2. Record that segment amount as compute gas and evaluate the compute-gas limit (and any latched non-compute resource-limit exceed) at that checkpoint — the latch-surface point is the next checkpoint rather than the next per-opcode recording site.
3. Record the checkpoint opcode's own body under the measurement-window rules for its metering class, then re-open the settlement window.

Non-opcode recording sites on this page (intrinsic gas, precompiles, code deposit, KeylessDeploy) are unchanged.

For every transaction that stays within every runtime resource limit, a node MUST produce the same recorded compute-gas total, the same four-dimension usage, the same receipt `gas_used`, the same execution result, and the same state as under Rex6.

#### Gas-clamp enforcement

After settlement and body recording at a checkpoint (and at frame entry and resume), a node MUST clamp the interpreter-visible remaining gas to the remaining compute headroom — the minimum of the current frame's remaining per-frame compute budget and the transaction-level remaining budget under the effective limit (including detention) — and MUST restore the hidden amount before the next checkpoint body, before `GAS` is observed, before call-gas forwarding, and before storage-gas charges.

The clamp is in force for the segment that follows whenever the true remaining gas is at or above the headroom, and a node MUST remember which constraint bound it along with that constraint's own limit value.
An exact equality is a binding clamp that hides nothing, not the absence of a clamp.
When the true remaining gas is below the headroom, no clamp is in force and an out-of-gas inside the segment is the inherited EVM's own.

Inside a plain-opcode segment:

- An opcode that would cost more than the clamped visible remainder MUST NOT execute.
- The frame's final result MUST restore the hidden gas.
- The node MUST reclassify that out-of-gas as the resource-limit exceed the clamp stood for: frame-local budget → frame revert with `MegaLimitExceeded`; transaction-level compute → transaction halt with `OutOfGas` and rescued remaining gas; detained limit → transaction halt with `VolatileDataAccessOutOfGas` and rescued remaining gas.

The `limit` reported by either shape MUST be the constraint that bound the clamp — the frame's own compute budget for a frame-local binding, the effective transaction-level limit otherwise — matching what the per-opcode check path on this page reports.

Because the crossing opcode never executes, a node MUST NOT include its cost in recorded compute-gas usage.
The `actual` a transaction-level clamp halt reports MUST be the transaction's final compute usage, after the frame-exit settlement has closed the partial segment the crossing opcode stopped inside.

When the current frame's remaining per-frame compute budget equals the transaction-level remaining budget, a node MUST bind the clamp to the transaction-level constraint (including detention when detention is the effective transaction-level bound).
A clamp-induced exceed under that binding MUST halt the transaction with gas rescue; a node MUST NOT classify the equality as frame-local.
Through Rex6, the same equality is classified by the per-opcode check as a frame-local exceed; at the top-level frame that surfaces as a revert rather than a halt.

When the crossing opcode would exhaust both the true remaining EVM gas and the compute headroom, a node MUST attribute the halt to the compute-gas or detention limit (with rescue) rather than to ordinary EVM out-of-gas.

#### Exceptional-halt frame carve-out

A frame that ends in an exceptional halt — ordinary out-of-gas, memory out-of-gas, stack underflow or overflow, invalid jump, unknown opcode, and every other error result — returns none of its remaining budget.
A node MUST settle that entire burned remainder as compute gas at frame exit: the open plain-opcode segment measured against a zero remainder, plus any gas the clamp was hiding.
The rule is driven by the halt classification rather than by the interpreter's own counter, which an inherited EVM zeroes for ordinary out-of-gas only.
Under per-opcode recording through Rex6 neither the failing opcode nor the burn is attributed to compute gas, so a transaction that halts exceptionally, or that contains an inner call frame which does, MAY report a strictly higher compute-gas total under Rex7 while EVM gas and the receipt remain identical.

A node MUST NOT evaluate any resource limit against the burned remainder: it is bounded by the sender's gas envelope rather than by the compute limit, and halting on it would rescue gas the EVM already burned and change the receipt this carve-out requires to stay identical.
The usage a limit is evaluated against therefore excludes the burn, while the reported compute-gas total and the block-level compute accounting include it.
Nothing is lost by the exclusion: the executed part of an exceptionally halted frame's tail is bounded either by the clamp or by a frame gas remainder that was already below the headroom.

A clamp-induced out-of-gas is not an exceptional halt for this rule — the crossing opcode never executed and the remaining gas is rescued rather than burned.

</details>

#### Keyless Deploy Exceed

When recording the [KeylessDeploy](../system-contracts/keyless-deploy.md) dispatch overhead exceeds a compute gas limit, the outcome follows the frame-local / transaction-level split above, but the two branches are not observably the same:

- A frame-local exceed MUST revert with an ABI-encoded `InsufficientComputeGas` error carrying the limit and the usage.
- A transaction-level exceed MUST halt with `OutOfGas` and empty output.
  A node MUST NOT attach `InsufficientComputeGas` to this branch; the caller cannot distinguish it from any other out-of-gas halt.

Each branch handles gas as its scope does above: the frame-local revert returns unspent gas to the parent through ordinary frame accounting, and the transaction-level halt rescues the outer transaction's remaining gas for the sender.
Specs through Rex5 do not rescue on this path: on those specs a transaction-level exceed here records a full spend and the sender loses the entire unused envelope.
See [Keyless Deployment](../system-contracts/keyless-deploy.md) for the full dispatch semantics.

### Revert Behavior

Compute gas MUST accumulate globally and MUST NOT be reverted.
Usage recorded within a call frame contributes to the transaction total whether that frame succeeds or reverts.

This makes compute gas the sole exception among the four runtime resource dimensions; the other three are call-frame-aware and discard a reverted frame's usage.
See [Resource Accounting](resource-accounting.md#revert-behavior).

## Constants

| Constant                        | Value         | Spec           | Description                                                                                                      |
| ------------------------------- | ------------- | -------------- | ---------------------------------------------------------------------------------------------------------------- |
| `TX_COMPUTE_GAS_LIMIT`          | 200,000,000   | Rex onward     | Maximum compute gas per transaction from Rex onward                                                              |
| `TX_COMPUTE_GAS_LIMIT`          | 1,000,000,000 | MiniRex        | Maximum compute gas per transaction under MiniRex                                                                |
| `FRAME_LIMIT_NUMERATOR`         | 98            | Rex4 onward    | Numerator of the per-call-frame budget forwarding fraction                                                       |
| `FRAME_LIMIT_DENOMINATOR`       | 100           | Rex4 onward    | Denominator of the per-call-frame budget forwarding fraction                                                     |
| `CALL_STIPEND`                  | 2,300         | All            | Standard EVM value-transfer call stipend, inherited unchanged                                                    |
| `CODEDEPOSIT`                   | 200           | All            | Standard EVM per-byte code-deposit gas, inherited unchanged                                                      |
| `KEYLESS_DEPLOY_OVERHEAD_GAS`   | 100,000       | Rex2 onward    | Fixed dispatch overhead for a keyless deploy                                                                     |
| `KZG_POINT_EVALUATION_GAS_COST` | 100,000       | MiniRex onward | MegaETH's fixed-cost override for the KZG point-evaluation precompile (defined in [Precompiles](precompiles.md)) |

The gas detention caps that lower the effective compute gas limit are defined in [Gas Detention](gas-detention.md).

## Rationale

**Why define a measurement window instead of a per-opcode gas table?**

MegaETH inherits its opcode gas schedule unchanged from Optimism Isthmus / Ethereum Prague.
Restating 256 absolute gas costs would duplicate the inherited specification, would need re-verification on every upstream EVM revision, and would still leave the consensus-critical question unanswered — namely, how much of an opcode's inherited cost counts as compute gas and when it is recorded.
The measurement window is the part MegaETH actually defines, so it is the part this page specifies.

**Why is the window's opening point normative rather than an implementation detail?**

Compute gas is bounded by a limit, so the amount recorded at the moment of a halt is consensus-visible.
Moving the opening point changes how much has been recorded when an opcode halts partway through, which changes which limit is reached first and at which opcode the transaction fails.
Two nodes with identical gas schedules but different window placement will disagree on transaction outcomes.

**Why are two forms of storage gas exclusion permitted?**

Subtracting the charge from a spanning window and placing the window after the charge produce identical results.
Permitting both lets an implementation choose whichever is cheaper at a given site without a spec change, and makes explicit that they must not be combined — a window placed after the charge must treat `storage_gas_charged` as zero.

**Why does the Rex5+ forwarded-gas exclusion subtract the `CALL_STIPEND`?**

For a value-transferring `CALL` or `CALLCODE`, the inherited EVM adds `CALL_STIPEND` to the child's gas limit without deducting it from the parent's remaining gas.
Treating the child's full gas limit as forwarded would therefore subtract gas the parent never contributed, under-counting the parent's compute gas by the stipend.

**Why is the first `CALL`-family touch of a preload-warm address charged cold?**

MegaETH's storage-gas pricing inspects the callee account before the opcode's own access, and that inspection materializes the account without inheriting its preloaded warmth, so the opcode's subsequent access observes a cold account.
The pricing has been charged gas on every spec since MiniRex and is therefore frozen; later specs changed which opcodes perform the inspection, which is why the affected opcode set varies by spec while the pricing of a performed inspection never changed.

**Why does MiniRex exclude three call opcodes from the forwarding cap?**

The MiniRex instruction table omitted the forwarding wrapper for `CALLCODE`, `DELEGATECALL`, and `STATICCALL`.
Rex corrected the omission.
MiniRex behavior remains frozen for replay compatibility.

**Why does this page carry per-spec behavior in the main specification?**

Concept pages normally describe only the latest stable spec and leave earlier behavior to the Spec History and upgrade pages.
This page departs from that.
Compute gas is not a fixed rule that happened to change; it is a measurement whose definition differs at MiniRex, Rex, Rex3, Rex4, Rex5, and Rex6, and a node replaying historical blocks must implement every one of those definitions simultaneously.
Splitting them across pages would force an implementer to reassemble the measurement from a changelog before they could execute a single historical block.
The current assignment is therefore stated first and directly, with the earlier definitions kept alongside it rather than relocated.

**Why is compute gas the only non-revertible dimension?**

CPU cycles cannot be undone.
If compute gas were scoped to call frames like the other dimensions, an attacker could execute and revert expensive subcalls repeatedly within a single transaction, consuming negligible apparent compute gas while imposing real execution cost on nodes.

**Why is the per-frame budget a guardrail rather than a guarantee?**

Making per-frame budgets protective would require reverting a failed child's compute gas, which reintroduces the attack above.
Keeping compute gas persistent while still bounding each frame gives early termination of runaway frames without creating a revert-based escape from transaction-level accounting.

**Why does intrinsic compute gas exclude MegaETH's intrinsic storage gas?**

Both are added to the same intrinsic total charged against the transaction's gas limit, but the storage components price persistent storage burden, not computation.
Recording them as compute gas would consume the compute budget for work that performs no computation, and would make the compute limit sensitive to calldata length.

## Security Considerations

**If the measurement window is opened after work that consumes EVM gas**

That gas escapes compute-gas accounting entirely.
An attacker could then construct a transaction whose real execution cost far exceeds its recorded compute gas, defeating the compute gas limit as a bound on node CPU time.

**If storage gas is recorded as compute gas**

Storage-heavy transactions exhaust the compute limit prematurely and halt while still holding gas, and the compute limit stops measuring computation.

**If forwarded child gas is not excluded**

It is counted twice — once by the parent's window and again by the child's own recording — so deep call chains exhaust the compute limit at a fraction of their true computational cost.

**If a recording is skipped when another dimension has already latched an exceed**

Block-level compute accounting under-reports the work the node actually performed, and a transaction can impose uncounted CPU cost by first tripping a cheaper dimension.

**If rescued gas includes the storage gas stipend**

System-granted gas leaks to the sender, who recovers gas that was never theirs to spend.

## Spec History

- [MiniRex](../upgrades/minirex.md) — introduced compute gas metering, the per-opcode measurement window, and `TX_COMPUTE_GAS_LIMIT` at 1,000,000,000.
  `CALLCODE`, `DELEGATECALL`, and `STATICCALL` are not subject to the 98/100 forwarding cap.
  The first `CALL` touch of a preload-warm address is charged cold.
- [Rex](../upgrades/rex.md) — brought `CALLCODE`, `DELEGATECALL`, and `STATICCALL` under the forwarding cap, changing their forwarded-gas exclusion; extended the cold first-touch charge to those three opcodes; lowered `TX_COMPUTE_GAS_LIMIT` to 200,000,000.
- [Rex1](../upgrades/rex1.md) — reset the detained compute gas limit between transactions in the same block.
- [Rex2](../upgrades/rex2.md) — re-enabled `SELFDESTRUCT`, adding the SelfDestruct metering class; introduced the keyless-deploy dispatch overhead.
- [Rex3](../upgrades/rex3.md) — moved `SLOAD` into the Volatile class for oracle access; began recording the keyless-deploy dispatch overhead as compute gas.
- [Rex4](../upgrades/rex4.md) — introduced the per-call-frame compute gas budget; made gas detention caps relative to usage at the access point; added beneficiary volatile-access guards to the `CALL` family, `SELFDESTRUCT`, and `SELFBALANCE`.
- [Rex5](../upgrades/rex5.md) — excluded the `CALL_STIPEND` from the forwarded-gas deduction; moved `CREATE2` memory-expansion recording ahead of the storage-gas charge; made contract-creation code-deposit compute gas atomic with the deployment commit; refined precompile compute-gas recording and bounded it by the remaining compute budget; added the `SELFDESTRUCT` empty-beneficiary storage-gas charge; removed `CALLCODE` from the cold first-touch charge and added `SELFDESTRUCT`'s beneficiary to it; stopped following EIP-7702 delegation in the pre-execution inspection, restoring inherited warmth for delegates.
- [Rex6](../upgrades/rex6.md) — unified the measurement window across all storage-affecting opcodes and folded `CREATE2` memory expansion into it, ending the two-window exception; returned forwarded gas to the failing frame on a compute-gas exceed; rescued the unused envelope on a keyless-deploy dispatch exceed; made beneficiary detection delegation-aware, returning `CALLCODE` call targets to the cold first-touch charge; exempted system-originated transactions from the compute gas limit and gas detention.
- [Rex7](../upgrades/rex7.md) _(unstable)_ — settles compute gas at checkpoints rather than after every plain opcode; enforces compute and detention limits inside plain segments by clamping interpreter-visible gas so a crossing opcode does not execute; records an exceptional-halt frame's burned remainder as compute gas at frame exit.
