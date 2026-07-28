---
description: MegaETH compute gas accounting specification — the measurement window that derives compute gas from inherited EVM gas, per-opcode metering classes, non-opcode recording sites, transaction and per-call-frame limits, and exceed behavior.
spec: Rex5
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

Compute gas metering begins at MiniRex.
Under the Equivalence spec, a node MUST NOT track compute gas and MUST NOT enforce any compute gas limit; execution is bounded only by the standard EVM gas limit.

Compute gas is not an independent gas schedule.
Unless explicitly overridden elsewhere in this specification, the EVM gas cost of every operation is inherited unchanged from Optimism Isthmus / Ethereum Prague.
What this page defines is how much of that inherited cost is _recorded_ as compute gas.

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

For an opcode, the window MUST open before any EVM gas movement attributable to that opcode, and MUST close after the opcode body has fully executed.

<details>

<summary>Rex6 (unstable): canonical window boundaries for storage-affecting opcodes</summary>

Under Rex6, the window for every storage-affecting opcode — `SSTORE`, `LOG0` through `LOG4`, `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, `CREATE`, `CREATE2`, and `SELFDESTRUCT` — MUST open before the opcode's storage-gas charge and before any wrapper-side EVM gas work, and MUST close after the inner opcode body completes.

The full canonical metering order is specified in [Dual Gas Model](dual-gas-model.md#gas-metering-order).

Pre-Rex6, `CREATE2` is the one opcode whose window does not span its wrapper-side memory expansion; see [Contract Creation Memory Expansion](#contract-creation-memory-expansion) below.

</details>

#### Single-Record Rule

A node MUST record an operation's compute gas exactly once per measurement window.

If the window does not close — because operand validation fails, a storage-gas charge exhausts the budget, or the operation halts partway through — the node MUST NOT record compute gas for that operation, even when EVM gas was already consumed by work performed before the halt.
The EVM gas consumed by such work remains deducted from the transaction's gas budget.

Pre-Rex6 `CREATE2` records in two windows rather than one; that exception is specified below.

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
if no child frame was spawned:
    forwarded_child_gas = 0
else if spec >= Rex5 and scheme in (CALL, CALLCODE) and child.value != 0:
    forwarded_child_gas = child.gas_limit - CALL_STIPEND
else:
    forwarded_child_gas = child.gas_limit
```

All three conditions in the middle branch MUST hold together.
A `DELEGATECALL` or `STATICCALL` never satisfies it (wrong scheme), a zero-value `CALL` never satisfies it (no stipend is granted), and no spec before Rex5 satisfies it.
`CREATE` and `CREATE2` always take the second branch: contract creation carries no stipend.

The `CALL_STIPEND` adjustment exists because the stipend is added to the child's gas limit without being deducted from the parent's remaining gas.
Treating it as forwarded would under-count the parent's compute gas by the stipend amount.

Under MiniRex, `CALLCODE`, `DELEGATECALL`, and `STATICCALL` do not apply the [98/100 forwarding cap](gas-forwarding.md), and therefore deduct the child's uncapped gas limit from the parent's recorded compute gas.
From Rex onward these three opcodes are subject to the same forwarding cap as `CALL`.

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

#### Class Assignment

The current assignment, which Rex5 and Rex6 share, is:

| Class            | Opcodes                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Plain**        | `STOP`; `ADD`–`SIGNEXTEND`; `LT`–`CLZ`; `KECCAK256`; `ADDRESS`, `ORIGIN`, `CALLER`, `CALLVALUE`, `CALLDATALOAD`, `CALLDATASIZE`, `CALLDATACOPY`, `CODESIZE`, `CODECOPY`, `GASPRICE`, `RETURNDATASIZE`, `RETURNDATACOPY`, `CHAINID`; `POP`, `MLOAD`, `MSTORE`, `MSTORE8`, `JUMP`, `JUMPI`, `PC`, `MSIZE`, `GAS`, `JUMPDEST`, `TLOAD`, `TSTORE`, `MCOPY`; `PUSH0`–`PUSH32`; `DUP1`–`DUP16`; `SWAP1`–`SWAP16`; `RETURN`, `REVERT`, `INVALID`              |
| **Volatile**     | Unconditional: `BLOCKHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `BLOBHASH`.<br>Volatile when the stack target is the [beneficiary](../glossary.md#beneficiary): `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH`.<br>Volatile when the executing contract is the beneficiary: `SELFBALANCE`.<br>Volatile when the executing contract is the [oracle](../system-contracts/oracle.md): `SLOAD`. |
| **Storage**      | `SSTORE`, `LOG0`–`LOG4`                                                                                                                                                                                                                                                                                                                                                                                                                                |
| **Call**         | `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`                                                                                                                                                                                                                                                                                                                                                                                                       |
| **Create**       | `CREATE`, `CREATE2`                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| **SelfDestruct** | `SELFDESTRUCT`                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| **Untracked**    | Every opcode undefined in the inherited EVM                                                                                                                                                                                                                                                                                                                                                                                                            |

Under Equivalence every opcode is Untracked, because compute gas is not tracked at all.

Class membership is not the only volatility-related property an opcode can carry.
`CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, and `SELFDESTRUCT` remain in their own classes but additionally apply a beneficiary volatile-access guard before executing.
Their measurement windows follow their class; the guard only decides whether the opcode runs at all.

##### How the assignment evolved

Class assignment is defined by inheritance: each spec inherits the previous spec's assignment and overrides only the opcodes listed.
A node implementing replay for historical blocks needs every spec's assignment, so the base and each delta are given below.

**MiniRex** (base assignment):

| Class         | Opcodes                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Plain**     | `STOP`; `ADD`–`SIGNEXTEND`; `LT`–`CLZ`; `KECCAK256`; `ADDRESS`, `ORIGIN`, `CALLER`, `CALLVALUE`, `CALLDATALOAD`, `CALLDATASIZE`, `CALLDATACOPY`, `CODESIZE`, `CODECOPY`, `GASPRICE`, `RETURNDATASIZE`, `RETURNDATACOPY`, `CHAINID`, `SELFBALANCE`; `POP`, `MLOAD`, `MSTORE`, `MSTORE8`, `SLOAD`, `JUMP`, `JUMPI`, `PC`, `MSIZE`, `GAS`, `JUMPDEST`, `TLOAD`, `TSTORE`, `MCOPY`; `PUSH0`–`PUSH32`; `DUP1`–`DUP16`; `SWAP1`–`SWAP16`; `RETURN`, `REVERT`, `INVALID` |
| **Volatile**  | `BLOCKHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `BLOBHASH` (unconditional); `BALANCE`, `EXTCODESIZE`, `EXTCODECOPY`, `EXTCODEHASH` (conditional on the [beneficiary](../glossary.md#beneficiary))                                                                                                                                                                                                             |
| **Storage**   | `SSTORE`, `LOG0`–`LOG4`                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **Call**      | `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`                                                                                                                                                                                                                                                                                                                                                                                                                  |
| **Create**    | `CREATE`, `CREATE2`                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **Untracked** | `SELFDESTRUCT` (disabled — halts with an invalid-opcode result); every opcode undefined in the inherited EVM                                                                                                                                                                                                                                                                                                                                                      |

Per-spec overrides, applied in order on top of the MiniRex base to reach the current assignment above:

| Spec     | Override                                                                                                                                                                                                             |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Rex**  | `CALLCODE`, `DELEGATECALL`, `STATICCALL` become subject to the 98/100 forwarding cap (class unchanged; the forwarded-gas exclusion changes accordingly)                                                              |
| **Rex1** | No change                                                                                                                                                                                                            |
| **Rex2** | `SELFDESTRUCT`: Untracked → SelfDestruct (re-enabled with EIP-6780 semantics)                                                                                                                                        |
| **Rex3** | `SLOAD`: Plain → Volatile (conditional on the [oracle](../system-contracts/oracle.md) contract)                                                                                                                      |
| **Rex4** | `SELFBALANCE`: Plain → Volatile (conditional on the beneficiary). `CALL`, `CALLCODE`, `DELEGATECALL`, `STATICCALL`, and `SELFDESTRUCT` gain a beneficiary volatile-access guard that runs before the opcode executes |
| **Rex5** | `SELFDESTRUCT` gains a storage-gas charge for empty-beneficiary creation (class unchanged; the storage gas exclusion becomes applicable)                                                                             |
| **Rex6** | No class changes                                                                                                                                                                                                     |

#### Contract Creation Memory Expansion

`CREATE2` expands memory to hash its initcode before the inner opcode runs.
The EVM gas consumed by that expansion is compute gas, and the spec determines which window records it:

| Spec         | Recording                                                                                         |
| ------------ | ------------------------------------------------------------------------------------------------- |
| MiniRex–Rex4 | Recorded in a second window, after the inner opcode completes. Skipped if the inner opcode fails. |
| Rex5         | Recorded in a separate window, before the contract-creation storage gas is charged.               |
| Rex6         | Folded into the single window covering the whole opcode.                                          |

Under MiniRex through Rex5 a node MUST therefore record `CREATE2` compute gas in two windows.
This is the sole stable exception to the [single-record rule](#single-record-rule).

<details>

<summary>Rex6 (unstable): CREATE2 memory expansion folded into the single window</summary>

Under Rex6, a node MUST record `CREATE2` compute gas in exactly one window, opened before the memory expansion and closed after the inner opcode completes.

This is observable only when a halt occurs between the memory expansion and the completion of the inner opcode.
On that path, Rex6 records no compute gas for the opcode, whereas Rex5 has already recorded the expansion gas.
The EVM gas consumed by the expansion is debited from the transaction's budget under both specs.

</details>

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

Every term is defined on this page, and a node MUST NOT record compute gas at any site not listed here.
`sum(recorded(opcode window))` sums over every window closed during execution — including windows in call frames that later reverted, since compute gas is not reverted.
The `keyless_deploy_recorded` term is zero before Rex3.
Under Equivalence every term is zero, because compute gas is not tracked at all.

#### Transaction Intrinsic Gas

Before the first call frame begins, a node MUST record the transaction's standard EVM intrinsic gas as compute gas.

This amount MUST comprise only the intrinsic gas defined by the inherited EVM — the base transaction cost, calldata token cost, access-list cost, and EIP-7702 authorization-list cost.
A node MUST NOT include MegaETH's intrinsic storage gas additions — the calldata storage gas and the flat transaction intrinsic storage gas defined in [Dual Gas Model](dual-gas-model.md) — in the recorded compute gas, even though both are added to the same intrinsic gas total charged against the transaction's gas limit.

This recording is made outside any call frame.
It therefore reduces the budget available to the top-level call frame, as specified in [Per-Call-Frame Budget](#per-call-frame-budget).

#### Precompiles

A node MUST record a precompile invocation's compute gas as follows:

| Spec         | Recorded amount              |
| ------------ | ---------------------------- |
| Equivalence  | Not recorded                 |
| MiniRex–Rex4 | The gas the precompile spent |
| Rex5+        | See the three cases below.   |

Under MiniRex through Rex4, "the gas the precompile spent" is the amount the precompile itself recorded as consumed.
On a failing precompile invocation, no cost is recorded, so the amount recorded as compute gas is zero — even though the parent frame still burns the forwarded gas limit.
Rex5 changed this; see below.

From Rex5 onward, the gas limit a node forwards into a precompile is capped at the remaining compute gas budget, as specified in [Precompile Compute-Gas Bound](resource-limits.md#precompile-compute-gas-bound).
Call that capped value the **effective gas limit**:

```
effective_gas_limit = min(call_gas_limit,
                          min(remaining(current frame),
                              effective_limit − compute_gas_used))
```

The inner minimum is the same quantity the [per-call-frame budget](#per-call-frame-budget) and [effective limit](#effective-limit) define: a precompile is bounded by whichever of the frame budget and the detained transaction budget binds first.

A node MUST record a Rex5+ precompile invocation's compute gas as:

```text
if the precompile returned or reverted:
    recorded = gas_spent
else if target is the KZG point-evaluation precompile
        and effective_gas_limit >= KZG_POINT_EVALUATION_GAS_COST:
    recorded = KZG_POINT_EVALUATION_GAS_COST
else:
    recorded = effective_gas_limit
```

The branches are evaluated in order; the first match applies.

Case 2 covers the KZG point-evaluation precompile reaching its verification step and returning a non-out-of-gas error.
The condition is expressed structurally — on the effective gas limit rather than on the specific error returned — so that it remains correct if the inherited EVM adds further non-out-of-gas error variants to that precompile.
No other precompile is subject to case 2.

`KZG_POINT_EVALUATION_GAS_COST` is a MegaETH override, not an inherited value: from MiniRex onward MegaETH replaces the KZG point-evaluation precompile with a fixed-cost variant.
See [Precompiles](precompiles.md) for the override itself.

Cases 2 and 3 reflect that the precompile never recorded a cost on those paths, so recording the spent amount — zero — would under-count the work performed.

{% hint style="info" %}
**Design intent.** On a halting precompile the recorded compute gas is deliberately distinct from the EVM gas the parent frame burns: the parent burns the caller-supplied call gas limit, while the compute gas recorded is the effective gas limit.
When the compute-gas cap is the binding constraint, these two amounts differ — a node MUST NOT reconcile them.
{% endhint %}

#### Contract Creation Code Deposit

For any contract creation (`CREATE`, `CREATE2`, or a contract-creation transaction), a node MUST record the code-deposit compute gas — `code_length × CODEDEPOSIT` — exactly once.

| Spec         | Recording point                                                                                                                                                                                    |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| MiniRex–Rex4 | During frame-return processing, in the window covering the EVM's code-deposit charge.                                                                                                              |
| Rex5+        | Atomically with the deployment commit: recorded when the deployment's pre-commit success conditions hold, at the same point the EVM charges the code-deposit gas and commits the created contract. |

A node MUST NOT record this amount twice.

The code-deposit _storage_ gas is charged before this window opens and therefore falls outside it, consistent with the [storage gas exclusion](#storage-gas-exclusion).

#### Keyless Deploy Sandbox

From Rex3 onward, a node MUST record the [KeylessDeploy](../system-contracts/keyless-deploy.md) fixed dispatch overhead (`KEYLESS_DEPLOY_OVERHEAD_GAS`) as compute gas when that overhead is charged.

From Rex5 onward, a node MUST additionally merge the compute gas consumed inside the keyless-deploy sandbox into the enclosing transaction's compute gas usage.

#### System Contract Interception

An intercepted [system contract](../system-contracts/interception.md) call produces a synthetic frame result without executing a child EVM frame.

A node MUST NOT record compute gas for the interception itself.
The gas forwarded to the intercepted call is returned to the caller in full unless the interceptor explicitly charges it, so the interception consumes no compute gas beyond that of the `CALL`-family opcode that initiated it.

A node MUST still push and pop a per-frame budget entry for the intercepted call, so that per-frame budget nesting stays aligned with the EVM call stack.

### Limits

#### Transaction-Level Limit

A node MUST enforce a per-transaction compute gas limit of `TX_COMPUTE_GAS_LIMIT`.

#### Effective Limit

The effective transaction-level limit is the minimum of `TX_COMPUTE_GAS_LIMIT` and the current detained limit produced by [gas detention](gas-detention.md):

```
effective_limit = min(TX_COMPUTE_GAS_LIMIT, detained_limit)
```

`detained_limit` starts at `TX_COMPUTE_GAS_LIMIT` and is lowered by each volatile-data access. It never rises again within a transaction:

```text
# on each volatile-data access
if spec >= Rex4:
    detained_limit = min(detained_limit, compute_gas_used + cap)
else:
    detained_limit = min(detained_limit, cap)
```

Because each access takes a minimum, the most restrictive cap wins regardless of access order.
The `cap` values, and the reset of `detained_limit` between transactions, are specified in [Gas Detention](gas-detention.md).

A node MUST evaluate the transaction-level check with strict greater-than:

```
tx_exceeded = compute_gas_used > effective_limit
```

Usage exactly equal to the effective limit is within limit.

#### Per-Call-Frame Budget

From Rex4 onward, a node MUST additionally enforce a per-call-frame compute gas budget.
Before Rex4, compute gas is enforced at the transaction level only, and no per-frame budget exists.

The budget for a frame MUST be:

```
budget(top-level frame) = TX_COMPUTE_GAS_LIMIT − intrinsic_recorded

budget(nested frame)    = remaining(parent) × FRAME_LIMIT_NUMERATOR / FRAME_LIMIT_DENOMINATOR

remaining(frame)        = max(0, budget(frame) − usage(frame))
```

Where `intrinsic_recorded` is the [transaction intrinsic gas](#transaction-intrinsic-gas) recorded before the first frame began, and `usage(frame)` is the compute gas recorded in that frame so far.
The nested-frame division truncates toward zero.

The per-frame check uses the same strict comparison as the transaction-level one:

```
frame_exceeded = usage(frame) > budget(frame)
```

A node MUST evaluate the per-frame check before the transaction-level check.
A node MUST NOT skip the transaction-level check when the per-frame check is within limit: the intrinsic recording lies outside every frame budget, and the detained limit may be lowered at any point during execution.

{% hint style="info" %}
**Design intent.** The per-frame budget is a termination guardrail, not a budget guarantee.
Because compute gas is never reverted (see [Revert Behavior](#revert-behavior)), a child frame that exceeds its budget and reverts still contributes its full recorded usage to the transaction total.
The budget bounds how far a single frame may run; it does not protect the transaction's remaining budget from that frame's consumption.
{% endhint %}

### Exceed Behavior

A compute gas exceed is either _frame-local_ or _transaction-level_, and the two produce different outcomes.

| Condition                                 | Scope             | Result                                                                   | Gas                                                                                 |
| ----------------------------------------- | ----------------- | ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------- |
| Per-frame budget exceeded                 | Frame-local       | The frame MUST revert with `MegaLimitExceeded(uint8 kind, uint64 limit)` | Returns to the parent frame normally                                                |
| Transaction limit exceeded                | Transaction-level | The transaction MUST halt with `OutOfGas`                                | Remaining gas is rescued and refunded to the sender                                 |
| Detained limit exceeded                   | Transaction-level | The transaction MUST halt with `OutOfGas`, reported as a detention halt  | Remaining gas is rescued and refunded to the sender                                 |
| Keyless-deploy dispatch overhead exceeded | See below         | The frame MUST revert, or the transaction MUST halt with `OutOfGas`      | Rescued only from Rex6 onward — see [Keyless Deploy Exceed](#keyless-deploy-exceed) |

A frame-local exceed does not fail the transaction: the parent frame MAY continue execution.

On a transaction-level exceed reached through opcode dispatch, a node MUST preserve the frame's remaining gas for refund to the sender.
The rescued amount MUST exclude any portion contributed by the [storage gas stipend](../glossary.md#storage-gas-stipend), so that system-granted gas is not recovered by the sender.

The keyless-deploy dispatch path is the one exception to the rescue rule; it is specified next.

#### Keyless Deploy Exceed

When recording the [KeylessDeploy](../system-contracts/keyless-deploy.md) dispatch overhead exceeds a compute gas limit, the outcome follows the frame-local / transaction-level split above: a frame-local exceed MUST revert, and a transaction-level exceed MUST halt with `OutOfGas`.
Both report the failure as `InsufficientComputeGas`.

Gas rescue on this path is spec-dependent:

- Under Rex3 through Rex5, a node MUST NOT rescue the outer transaction's remaining gas on a transaction-level exceed here. The halt records a full spend and the sender loses the entire unused envelope.
- From Rex6 onward, a node MUST rescue the outer transaction's unspent gas, aligning this path with opcode dispatch.

A node MUST NOT generalize the opcode-dispatch rescue rule to this path on Rex3 through Rex5.
See [Keyless Deployment](../system-contracts/keyless-deploy.md) for the full dispatch semantics.

A node MUST record compute gas before evaluating any exceed, including an exceed already latched on another resource dimension.
The compute work was performed, and the recorded total feeds the transaction outcome and the block-level compute accounting even for a transaction halted on a different dimension.

<details>

<summary>Rex6 (unstable): forwarded gas returned on halt, and system-originated transaction exemption</summary>

Under Rex6, when a `CALL`-family or `CREATE` / `CREATE2` opcode halts on the compute gas limit, its pending child frame is discarded before the child runs.
A node MUST return the gas already forwarded to that discarded child to the parent frame before halting, so the transaction's `gas_used` reflects only the gas actually consumed.

Under Rex6, a node MUST NOT halt a [system-originated transaction](../system-contracts/system-tx.md#system-originated-transaction-metering-exemption) on the compute gas limit or on gas detention.
The node MUST still record its compute gas usage; only the halt decision is suppressed.
The transaction's standard EVM `gas_limit` remains the only bound that can halt it.

</details>

### Revert Behavior

Compute gas MUST accumulate globally and MUST NOT be reverted.
Usage recorded within a call frame contributes to the transaction total whether that frame succeeds or reverts.

This makes compute gas the sole exception among the four runtime resource dimensions; the other three are call-frame-aware and discard a reverted frame's usage.
See [Resource Accounting](resource-accounting.md#revert-behavior).

### Detention Reset

The detained compute gas limit is reset at the start of each transaction from Rex1 onward.
Before Rex1, the detained limit persists across transactions within the same block.
See [Gas Detention](gas-detention.md).

## Constants

| Constant                        | Value         | Spec           | Description                                                                                                      |
| ------------------------------- | ------------- | -------------- | ---------------------------------------------------------------------------------------------------------------- |
| `TX_COMPUTE_GAS_LIMIT`          | 1,000,000,000 | MiniRex        | Maximum compute gas per transaction under MiniRex                                                                |
| `TX_COMPUTE_GAS_LIMIT`          | 200,000,000   | Rex onward     | Maximum compute gas per transaction from Rex onward                                                              |
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

**Why does MiniRex exclude three call opcodes from the forwarding cap?**

The MiniRex instruction table omitted the forwarding wrapper for `CALLCODE`, `DELEGATECALL`, and `STATICCALL`.
Rex corrected the omission.
MiniRex behavior remains frozen for replay compatibility.

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

- [MiniRex](../upgrades/minirex.md) — introduced compute gas metering, the per-opcode measurement window, and `TX_COMPUTE_GAS_LIMIT` at 1,000,000,000. `CALLCODE`, `DELEGATECALL`, and `STATICCALL` are not subject to the 98/100 forwarding cap.
- [Rex](../upgrades/rex.md) — brought `CALLCODE`, `DELEGATECALL`, and `STATICCALL` under the forwarding cap, changing their forwarded-gas exclusion; lowered `TX_COMPUTE_GAS_LIMIT` to 200,000,000.
- [Rex1](../upgrades/rex1.md) — reset the detained compute gas limit between transactions in the same block.
- [Rex2](../upgrades/rex2.md) — re-enabled `SELFDESTRUCT`, adding the SelfDestruct metering class; introduced the keyless-deploy dispatch overhead.
- [Rex3](../upgrades/rex3.md) — moved `SLOAD` into the Volatile class for oracle access; began recording the keyless-deploy dispatch overhead as compute gas.
- [Rex4](../upgrades/rex4.md) — introduced the per-call-frame compute gas budget; made gas detention caps relative to usage at the access point; added beneficiary volatile-access guards to the `CALL` family, `SELFDESTRUCT`, and `SELFBALANCE`.
- [Rex5](../upgrades/rex5.md) — excluded the `CALL_STIPEND` from the forwarded-gas deduction; moved `CREATE2` memory-expansion recording ahead of the storage-gas charge; made contract-creation code-deposit compute gas atomic with the deployment commit; refined precompile compute-gas recording and bounded it by the remaining compute budget; added the `SELFDESTRUCT` empty-beneficiary storage-gas charge.
- [Rex6](../upgrades/rex6.md) (**unstable**) — unified the measurement window across all storage-affecting opcodes and folded `CREATE2` memory expansion into it; returned forwarded gas to the parent frame on a compute-gas halt; exempted system-originated transactions from the compute gas limit and gas detention.
