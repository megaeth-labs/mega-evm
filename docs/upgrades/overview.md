# Overview

This section documents each MegaETH network upgrade, describing what changed, why, and what developers need to know.

## Hardfork History

### [MiniRex](minirex.md)

{% tabs %}
{% tab title="Testnet" %}
`0` (Genesis)
{% endtab %}
{% tab title="Mainnet" %}
`0` (Genesis)
{% endtab %}
{% endtabs %}

[Dual gas model](../evm/dual-gas-model.md), [multidimensional resource limits](../evm/resource-limits.md), [gas detention](../evm/gas-detention.md), 98/100 gas forwarding, SELFDESTRUCT disabled, system contracts ([Oracle](../system-contracts/oracle.md), [Timestamp](../oracle-services/timestamp.md))

### MiniRex1

{% tabs %}
{% tab title="Testnet" %}
N/A
{% endtab %}
{% tab title="Mainnet" %}
`1764845637` (Dec 4, 2025, 10:53 UTC)
{% endtab %}
{% endtabs %}

Rollback: reverted to Equivalence spec (maps to `EQUIVALENCE`).
The MiniRex features were deactivated on the network; no chain reorg or state rollback occurred.
Contracts deployed during MiniRex remained on-chain.

### MiniRex2

{% tabs %}
{% tab title="Testnet" %}
N/A
{% endtab %}
{% tab title="Mainnet" %}
`1764849932` (Dec 4, 2025, 12:05 UTC)
{% endtab %}
{% endtabs %}

Restoration: re-activated MiniRex spec (maps to `MINI_REX`).
All MiniRex features (dual gas model, resource limits, gas detention) were re-enabled.

### [Rex](rex.md)

{% tabs %}
{% tab title="Testnet" %}
`1764694618` (Dec 2, 2025, 16:56 UTC)
{% endtab %}
{% tab title="Mainnet" %}
`1764851940` (Dec 4, 2025, 12:39 UTC)
{% endtab %}
{% endtabs %}

Transaction intrinsic [storage gas](../glossary.md#storage-gas), revised storage gas economics (`base × (multiplier − 1)`), consistent CALL-like opcode behavior, [state growth](../evm/resource-accounting.md#state-growth) tracking

### [Rex1](rex1.md)

{% tabs %}
{% tab title="Testnet" %}
`1766147599` (Dec 19, 2025, 12:33 UTC)
{% endtab %}
{% tab title="Mainnet" %}
`1766282400` (Dec 21, 2025, 02:00 UTC)
{% endtab %}
{% endtabs %}

Fix: [compute gas](../glossary.md#compute-gas) limit reset between transactions

### [Rex2](rex2.md)

{% tabs %}
{% tab title="Testnet" %}
`1770116400` (Feb 3, 2026, 11:00 UTC)
{% endtab %}
{% tab title="Mainnet" %}
`1770246000` (Feb 4, 2026, 23:00 UTC)
{% endtab %}
{% endtabs %}

SELFDESTRUCT re-enabled ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)), [KeylessDeploy](../system-contracts/keyless-deploy.md) system contract

### [Rex3](rex3.md)

{% tabs %}
{% tab title="Testnet" %}
`1771380000` (Feb 18, 2026, 02:00 UTC)
{% endtab %}
{% tab title="Mainnet" %}
`1771639200` (Feb 21, 2026, 02:00 UTC)
{% endtab %}
{% endtabs %}

[Oracle](../system-contracts/oracle.md) gas cap raised to 20M, SLOAD-based oracle detention, keyless deploy compute gas tracking

## Upcoming (Unstable)

### [Rex4](rex4.md)

Per-[call-frame](../glossary.md#call-frame) resource budgets, relative gas detention, MegaAccessControl, MegaLimitControl, sandbox environment inheritance.

Rex4 is the current unstable specification and is subject to change before activation.

## How to Read These Pages

Each upgrade page follows a consistent structure:

- **Summary** — What changed and why
- **What Changed** — Previous vs new behavior for each change
- **Developer Impact** — What you need to care about
- **Safety and Compatibility** — Backward-compatibility boundaries and failure-mode differences
- **References** — Links to the normative spec and related documentation
