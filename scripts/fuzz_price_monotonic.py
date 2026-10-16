#!/usr/bin/env python3
"""Compare two price records of the fuzz target's `test_property_price_record`.

Each record is written by the `mega-evm` fuzz target under the `satin-price-override` feature,
with `MEGA_FUZZ_PRICE_RECORD` naming the file, one line per case:

    <index> <cpsb milli-gas> <cphb milli-gas> <kind> <state gas> <history gas> <history bytes> <gas used> <path hash>

Both records must come from the same seed and case count, so the cases match by index. Where a
case followed the same path at both prices (the same result kind, logs, output and state but for
balances), its state gas must not fall as the cost per state byte rises, nor its history gas as
the cost per history byte rises; its history bytes must be the same, since they are a count. Where
the path differs, a dearer byte may have stopped the transaction sooner, and nothing is implied.

Usage: fuzz_price_monotonic.py <record at the lower prices> <record at the higher prices>
Exits 1 on the first violation, or when the two records do not line up.
"""
import sys


def read(path):
    rows = {}
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) != 9:
                sys.exit(f"{path}: malformed line: {line.rstrip()}")
            index = int(parts[0])
            rows[index] = {
                "cpsb": int(parts[1]),
                "cphb": int(parts[2]),
                "kind": parts[3],
                "state": int(parts[4]),
                "history": int(parts[5]),
                "history_bytes": int(parts[6]),
                "gas_used": int(parts[7]),
                "path": parts[8],
            }
    return rows


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    low, high = read(sys.argv[1]), read(sys.argv[2])
    if low.keys() != high.keys():
        sys.exit("the two records do not hold the same cases: run both with the same seed and case count")
    if not low:
        sys.exit("the records are empty")
    first_low, first_high = next(iter(low.values())), next(iter(high.values()))
    if first_low["cpsb"] > first_high["cpsb"] or first_low["cphb"] > first_high["cphb"]:
        sys.exit("the first record must be the one at the lower prices")
    same_path = 0
    for index in sorted(low):
        a, b = low[index], high[index]
        if a["kind"] == "refused" or b["kind"] == "refused":
            continue
        if a["kind"] != b["kind"] or a["path"] != b["path"]:
            continue
        same_path += 1
        if a["state"] > b["state"]:
            sys.exit(f"case {index}: state gas fell from {a['state']} to {b['state']} as the cost per state byte rose")
        if a["history"] > b["history"]:
            sys.exit(f"case {index}: history gas fell from {a['history']} to {b['history']} as the cost per history byte rose")
        if a["history_bytes"] != b["history_bytes"]:
            sys.exit(f"case {index}: the history bytes moved from {a['history_bytes']} to {b['history_bytes']} on the same path")
    print(f"{len(low)} cases, {same_path} on the same path at both prices: state and history gas are monotone")


if __name__ == "__main__":
    main()
