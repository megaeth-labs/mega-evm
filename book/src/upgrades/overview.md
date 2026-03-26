# Overview

This section documents each MegaETH network upgrade, describing what changed, why, and what developers need to know.

## Hardfork History

{% updates %}

{% update date="2025-12-04" %}

### [MiniRex](minirex.md)

{% tabs %}
{% tab title="Testnet" %}
Genesis (0)
{% endtab %}
{% tab title="Mainnet" %}
Genesis (0)
{% endtab %}
{% endtabs %}

[Dual gas model](../evm/dual-gas-model.md), [multidimensional resource limits](../evm/resource-limits.md), [gas detention](../evm/gas-detention.md), 98/100 gas forwarding, SELFDESTRUCT disabled, system contracts ([Oracle](../system-contracts/oracle.md), [Timestamp](../oracle-services/timestamp.md))

{% endupdate %}

{% update date="2025-12-04" %}

### MiniRex1

{% tabs %}
{% tab title="Testnet" %}
Not activated
{% endtab %}
{% tab title="Mainnet" %}
Dec 4, 2025, 10:53 UTC (1764845637)
{% endtab %}
{% endtabs %}

Rollback: reverted to Equivalence spec (maps to `EQUIVALENCE`).
The MiniRex features were deactivated on the network; no chain reorg or state rollback occurred.
Contracts deployed during MiniRex remained on-chain.

{% endupdate %}

{% update date="2025-12-04" %}

### MiniRex2

{% tabs %}
{% tab title="Testnet" %}
Not activated
{% endtab %}
{% tab title="Mainnet" %}
Dec 4, 2025, 12:05 UTC (1764849932)
{% endtab %}
{% endtabs %}

Restoration: re-activated MiniRex spec (maps to `MINI_REX`).
All MiniRex features (dual gas model, resource limits, gas detention) were re-enabled.

{% endupdate %}

{% update date="2025-12-04" %}

### [Rex](rex.md)

{% tabs %}
{% tab title="Testnet" %}
Dec 2, 2025, 16:56 UTC (1764694618)
{% endtab %}
{% tab title="Mainnet" %}
Dec 4, 2025, 12:39 UTC (1764851940)
{% endtab %}
{% endtabs %}

Transaction intrinsic [storage gas](../glossary.md#storage-gas), revised storage gas economics (`base × (multiplier − 1)`), consistent CALL-like opcode behavior, [state growth](../evm/resource-accounting.md#state-growth) tracking

{% endupdate %}

{% update date="2025-12-21" %}

### [Rex1](rex1.md)

{% tabs %}
{% tab title="Testnet" %}
Dec 19, 2025, 12:33 UTC (1766147599)
{% endtab %}
{% tab title="Mainnet" %}
Dec 21, 2025, 02:00 UTC (1766282400)
{% endtab %}
{% endtabs %}

Fix: [compute gas](../glossary.md#compute-gas) limit reset between transactions

{% endupdate %}

{% update date="2026-02-04" %}

### [Rex2](rex2.md)

{% tabs %}
{% tab title="Testnet" %}
Feb 3, 2026, 11:00 UTC (1770116400)
{% endtab %}
{% tab title="Mainnet" %}
Feb 4, 2026, 23:00 UTC (1770246000)
{% endtab %}
{% endtabs %}

SELFDESTRUCT re-enabled ([EIP-6780](https://eips.ethereum.org/EIPS/eip-6780)), [KeylessDeploy](../system-contracts/keyless-deploy.md) system contract

{% endupdate %}

{% update date="2026-02-21" %}

### [Rex3](rex3.md)

{% tabs %}
{% tab title="Testnet" %}
Feb 18, 2026, 02:00 UTC (1771380000)
{% endtab %}
{% tab title="Mainnet" %}
Feb 21, 2026, 02:00 UTC (1771639200)
{% endtab %}
{% endtabs %}

[Oracle](../system-contracts/oracle.md) gas cap raised to 20M, SLOAD-based oracle detention, keyless deploy compute gas tracking

{% endupdate %}

{% update date="TBD" %}

### [Rex4](rex4.md)

{% tabs %}
{% tab title="Testnet" %}
TBD
{% endtab %}
{% tab title="Mainnet" %}
TBD
{% endtab %}
{% endtabs %}

Per-[call-frame](../glossary.md#call-frame) resource budgets, relative gas detention, MegaAccessControl, MegaLimitControl, sandbox environment inheritance

{% endupdate %}

{% endupdates %}

## How to Read These Pages

Each upgrade page follows a consistent structure:

- **Summary** — What changed and why
- **What Changed** — Previous vs new behavior for each change
- **Developer Impact** — What you need to care about
- **Safety and Compatibility** — Backward-compatibility boundaries and failure-mode differences
- **References** — Links to the normative spec and related documentation
