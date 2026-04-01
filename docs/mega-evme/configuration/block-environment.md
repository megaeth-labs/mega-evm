# Block Environment

These options configure the block context for EVM execution.
They are available in the `run` and `tx` commands.
The `replay` command fetches block environment from the RPC automatically.

## Options

| Flag | Default | Aliases | Description |
|------|---------|---------|-------------|
| `--block.number <NUM>` | `1` | — | Block number |
| `--block.coinbase <ADDR>` | `0x0…0` | `--block.beneficiary` | Block coinbase / beneficiary address |
| `--block.timestamp <TS>` | `1` | — | Block timestamp (Unix seconds) |
| `--block.gaslimit <GAS>` | `10000000000` | `--block.gas-limit`, `--block.gas` | Block gas limit |
| `--block.basefee <FEE>` | `0` | `--block.base-fee` | EIP-1559 base fee |
| `--block.difficulty <DIFF>` | `0` | — | Block difficulty |
| `--block.prevrandao <HASH>` | `0x0…0` | `--block.random` | Previous randao value (post-merge) |
| `--block.blobexcessgas <GAS>` | `0` | `--block.blob-excess-gas` | EIP-4844 blob excess gas |

## Examples

```bash
# Simulate a specific block context — call WETH.name() at a fixed block
mega-evme tx \
  --receiver 0x4200000000000000000000000000000000000006 \
  --input 0x06fdde03 \
  --sender.balance 1ether \
  --block.number 1000 \
  --block.timestamp 1234567890 \
  --block.basefee 1000000000 \
  --basefee 1000000000

# Test COINBASE opcode — pushes the beneficiary address onto the stack
mega-evme run 0x41 --block.coinbase 0x1111111111111111111111111111111111111111
```

## Notes

The block environment affects opcodes like `NUMBER`, `TIMESTAMP`, `COINBASE`, `BASEFEE`, `DIFFICULTY` / `PREVRANDAO`, and `BLOBBASEFEE`.

In MegaETH specs (MiniRex and above), accessing block environment fields triggers [gas detention](https://megaeth-labs.github.io/mega-evm/spec/evm/gas-detention.html) — the remaining compute gas is capped to reduce parallel execution conflicts.
This is normal MegaETH behavior and is reflected in `mega-evme` execution.
