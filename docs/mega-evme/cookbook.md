---
description: Runnable recipes for debugging, gas analysis, state diffing, and more.
---

# Cookbook

Worked examples for common `mega-evme` tasks.

## Debug a Failing Transaction

Replay a transaction from MegaETH mainnet with opcode-level tracing to find the revert reason.
Replace `0xabc123...` with a real transaction hash:

```bash
mega-evme replay 0xabc123... \
  --rpc https://mainnet.megaeth.com/rpc \
  --trace --tracer opcode \
  --trace.opcode.enable-return-data \
  --trace.output trace.json
```

Inspect `trace.json` for the opcode that triggered `REVERT`.
The return data contains the revert reason (ABI-encoded).

## Compare Gas Usage Across Specs

Run the same calldata against different specs to see how gas costs differ:

```bash
# Under Rex3 — call WETH.balanceOf
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --spec Rex3

# Under Rex4
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --spec Rex4
```

Compare the `gasUsed` in the output to understand the impact of spec changes on your contract.

## Deploy and Interact in Two Steps

Deploy a contract, capture the state, then call it:

```bash
# Step 1: Deploy
mega-evme run --create true 0x6080604052... \
  --sender.balance 10ether \
  --dump --dump.output post-deploy.json

# Note the deployed contract address printed in the output.

# Step 2: Call the deployed contract — replace 0xDeployedAddress with the printed address
# and 0x06fdde03 with the selector you want to call
mega-evme run 0x \
  --prestate post-deploy.json \
  --receiver 0xDeployedAddress \
  --input 0x06fdde03 \
  --dump
```

## Test SALT Bucket Impact on Storage Gas

Observe how storage gas costs scale with bucket capacity:

```bash
# Minimum bucket (free storage gas)
mega-evme run 0x60aa600055 \
  --spec Rex3 \
  --trace --tracer opcode

# Large bucket (expensive storage gas)
mega-evme run 0x60aa600055 \
  --spec Rex3 \
  --bucket-capacity 0:5000000 \
  --trace --tracer opcode
```

Compare the `gasCost` of the `SSTORE` opcode in both traces.

## Fork and Patch Storage for Access Control Testing

Override an access-control slot to test a protected function without needing the actual admin key.
This example patches WETH's slot 0 (total supply) and reads it back with `totalSupply()`:

```bash
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --storage "0x4200000000000000000000000000000000000006:0x0=0x0000000000000000000000000000000000000000000000000000000000000001" \
  --input 0x18160ddd
```

The `--storage` flag sets the slot to `1` before execution, so `totalSupply()` returns `1` regardless of the live chain value.

## Replay With Modified Calldata

Test what would happen if a transaction had been called with different arguments.
Replace `0xabc123...` with a real transaction hash from MegaETH mainnet:

```bash
mega-evme replay 0xabc123... \
  --rpc https://mainnet.megaeth.com/rpc \
  --override.input 0x70a08231000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266 \
  --trace --tracer call
```

## Fund Multiple Accounts for Multi-Party Testing

Set up balances for several participants in a single command:

```bash
mega-evme tx \
  --faucet 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266+=100ether \
  --faucet 0x70997970C51812dc3A010C7d01b50e0d17dc79C8+=50ether \
  --faucet 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC+=25ether \
  --sender 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x06fdde03 \
  --dump --dump.output multi-party-state.json
```

## Capture a State Diff

See exactly which accounts and storage slots a transaction modifies.
`WETH.deposit()` wraps ETH and updates the caller's balance mapping:

```bash
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0xd0e30db0 \
  --value 1000000000000000 \
  --trace --tracer pre-state \
  --trace.prestate.diff-mode \
  --trace.output diff.json
```

The `diff.json` file shows `pre` and `post` states for all touched accounts.

## Investigate Gas Detention Behavior

When a transaction reads volatile data (block environment fields, oracle storage), gas detention caps the remaining compute gas.
Call `HighPrecisionTimestamp.timestamp()` — it reads oracle storage, which triggers detention:

```bash
mega-evme tx \
  --fork --fork.rpc https://mainnet.megaeth.com/rpc \
  --sender.balance 1ether \
  --receiver 0x6342000000000000000000000000000000000002 \
  --input 0xb80777ea \
  --spec Rex4 \
  --trace --tracer opcode \
  --trace.output detention-trace.json
```

Look for a sharp drop in `gas` remaining after the oracle `SLOAD` opcode.
