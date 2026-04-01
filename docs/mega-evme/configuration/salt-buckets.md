---
description: Control SALT bucket capacities to test dynamic storage gas pricing.
---

# SALT Bucket Configuration

MegaETH uses SALT buckets to dynamically scale storage gas costs.
Each account and storage slot maps to a SALT bucket; the gas cost of storage operations is multiplied by a factor derived from the bucket's capacity.
Crowded state regions become progressively more expensive, while fresh storage (minimum-sized buckets) is free.

The `--bucket-capacity` flag lets you configure bucket capacities for testing how storage gas costs behave under different state conditions.

## Options

| Flag                              | Default | Description                      |
| --------------------------------- | ------- | -------------------------------- |
| `--bucket-capacity <ID:CAPACITY>` | empty   | Set bucket capacity (repeatable) |

Format: `bucket_id:capacity`, where:

- `bucket_id` — 32-bit bucket identifier
- `capacity` — current capacity in bytes

## Examples

```bash
# Single bucket
mega-evme run contract.hex --bucket-capacity 123:1000000

# Multiple buckets
mega-evme run contract.hex \
  --bucket-capacity 123:1000000 \
  --bucket-capacity 456:2000000 \
  --bucket-capacity 789:500000

# Combined with state and tracing to observe gas costs
mega-evme tx \
  --receiver 0x1234... \
  --input 0x55... \
  --bucket-capacity 1:1000 \
  --trace --tracer opcode
```

## Notes

- Without any `--bucket-capacity` flags, all buckets default to the minimum size, which means storage operations incur zero storage gas.
- This option is available in all commands (`run`, `tx`, and `replay`).
- SALT-based dynamic gas pricing is active in MiniRex and later specs.
  In `Equivalence` mode, bucket capacities have no effect.
