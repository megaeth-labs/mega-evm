#!/usr/bin/env bash
set -euo pipefail

# Keyless deployment script for MegaETH
#
# Usage:
#   PRIVATE_KEY=<key> ./scripts/keyless_deploy.sh <rlp-encoded-keyless-tx-hex>
#
# Environment variables:
#   PRIVATE_KEY  - Private key of a funded account (required)
#   RPC_URL      - RPC endpoint (default: http://localhost:8545)
#   MEGA_EVME    - Path to mega-evme binary (default: mega-evme from PATH)
#   FUND_MARGIN  - Extra wei to fund above simulated cost (default: 0.001 ether)

KEYLESS_TX="${1:?Usage: PRIVATE_KEY=<key> $0 <keyless-tx-hex>}"
: "${PRIVATE_KEY:?PRIVATE_KEY environment variable is required}"
RPC_URL="${RPC_URL:-http://localhost:8545}"

SYSTEM_CONTRACT="0x6342000000000000000000000000000000000003"
MEGA_EVME="${MEGA_EVME:-mega-evme}"
FUND_MARGIN="${FUND_MARGIN:-1000000000000000}"  # default 0.001 ether
export FOUNDRY_DISABLE_NIGHTLY_WARNING=1 # Suppress foundry nightly warning

# Strip 0x prefix if present
KEYLESS_TX="${KEYLESS_TX#0x}"

echo "=== Keyless Deployment ==="
echo "RPC:         $RPC_URL"
echo "mega-evme:   $MEGA_EVME"
echo "FUND_MARGIN: $FUND_MARGIN"
echo ""

# Step 1: Decode the keyless transaction
echo "--- Step 1: Decoding keyless transaction ---"
DECODED=$(cast decode-tx "0x${KEYLESS_TX}" --json)

SIGNER=$(echo "$DECODED" | jq -r '.signer')
GAS_PRICE=$(echo "$DECODED" | jq -r '.gasPrice')
ORIG_GAS=$(echo "$DECODED" | jq -r '.gas')
TX_VALUE=$(echo "$DECODED" | jq -r '.value')

# Convert hex values to decimal
GAS_PRICE_DEC=$(cast to-dec "$GAS_PRICE")
ORIG_GAS_DEC=$(cast to-dec "$ORIG_GAS")
TX_VALUE_DEC=$(cast to-dec "$TX_VALUE")

echo "Signer:   $SIGNER"
echo "Gas Price: $GAS_PRICE_DEC wei"
echo "Gas Limit: $ORIG_GAS_DEC"
echo "Value:     $TX_VALUE_DEC wei"
echo ""

# Step 2: Pre-deployment checks
echo "--- Step 2: Pre-deployment checks ---"

SIGNER_NONCE=$(cast nonce "$SIGNER" --rpc-url "$RPC_URL")
echo "Signer nonce: $SIGNER_NONCE"
if [ "$SIGNER_NONCE" -gt 1 ]; then
    echo "ERROR: Signer nonce is $SIGNER_NONCE (must be <= 1). Keyless deploy is permanently disabled."
    exit 1
fi

EXPECTED_ADDR=$(cast compute-address "$SIGNER" --nonce 0 | grep -oE '0x[0-9a-fA-F]{40}')
echo "Expected deployment address: $EXPECTED_ADDR"

EXISTING_CODE=$(cast code "$EXPECTED_ADDR" --rpc-url "$RPC_URL")
if [ "$EXISTING_CODE" != "0x" ]; then
    EXISTING_HASH=$(cast keccak "$EXISTING_CODE")
    echo "ERROR: Contract already exists at $EXPECTED_ADDR (codehash: $EXISTING_HASH)"
    exit 1
fi
echo "No existing code at deployment address (OK)"
echo ""

# Step 3: Simulate with mega-evme to find gas used
echo "--- Step 3: Simulating with mega-evme ---"

DUMP_FILE=$(mktemp)
trap "rm -f $DUMP_FILE" EXIT

SIMULATION_OUTPUT=$("$MEGA_EVME" tx \
    "0x${KEYLESS_TX}" \
    --fork --fork.rpc "$RPC_URL" \
    --sender.balance 1000ether \
    --gas 1000000000 \
    --nonce 0 \
    --spec Rex2 \
    --dump --dump.output "$DUMP_FILE" \
    2>&1)

echo "$SIMULATION_OUTPUT" | head -20

# Check execution status
STATUS=$(echo "$SIMULATION_OUTPUT" | sed -n 's/.*Status:[[:space:]]\+\([^[:space:]]\+\).*/\1/p')
if [ "$STATUS" != "Success" ]; then
    echo "ERROR: Simulation failed with status: $STATUS"
    echo "Full output:"
    echo "$SIMULATION_OUTPUT"
    exit 1
fi

# Parse gas_used and contract address from Receipt JSON
RECEIPT_JSON=$(echo "$SIMULATION_OUTPUT" | awk '/=== Receipt ===/{f=1; next} /^===/ && f{exit} f{print}')
GAS_USED=$(echo "$RECEIPT_JSON" | jq -r '.gasUsed')
DEPLOYED_ADDR=$(echo "$RECEIPT_JSON" | jq -r '.contractAddress // empty')

# Compute actual balance spent from state dump (accounts for L1 data fees)
INITIAL_BALANCE="1000000000000000000000"  # 1000 ether in wei
SIGNER_LOWER=$(echo "$SIGNER" | tr '[:upper:]' '[:lower:]')
FINAL_BALANCE_HEX=$(jq -r "to_entries[] | select(.key | ascii_downcase == \"$SIGNER_LOWER\") | .value.balance" "$DUMP_FILE")
FINAL_BALANCE=$(python3 -c "print(int('$FINAL_BALANCE_HEX', 16))")
BALANCE_SPENT=$(python3 -c "print($INITIAL_BALANCE - $FINAL_BALANCE)")

echo ""
echo "Simulation results:"
echo "  Gas used:          $GAS_USED"
echo "  Deployed address:  $DEPLOYED_ADDR"
echo "  Balance spent:     $BALANCE_SPENT wei"

if [ -z "$DEPLOYED_ADDR" ] || [ "$DEPLOYED_ADDR" = "null" ]; then
    echo "ERROR: Simulation shows deployment would fail (no contract address)"
    exit 1
fi
echo ""

# Step 4: Compute gasLimitOverride and funding amount
echo "--- Step 4: Computing funding requirements ---"

GAS_OVERRIDE=$(cast to-dec "$GAS_USED")
FUND_AMOUNT=$(python3 -c "print($BALANCE_SPENT + $FUND_MARGIN)")

echo "Gas override:  $GAS_OVERRIDE"
echo "Fund amount:   $FUND_AMOUNT wei ($(python3 -c "print(f'{$FUND_AMOUNT / 10**18:.6f}')")  ETH)"
echo ""

# Step 5: Fund the signer
echo "--- Step 5: Funding signer ---"

SIGNER_BALANCE=$(cast balance "$SIGNER" --rpc-url "$RPC_URL")
DEFICIT=$(python3 -c "d = $FUND_AMOUNT - $SIGNER_BALANCE; print(d if d > 0 else 0)")

if [ "$DEFICIT" = "0" ]; then
    echo "Signer already has sufficient balance ($SIGNER_BALANCE wei), skipping funding"
else
    echo "Signer balance: $SIGNER_BALANCE wei, need $FUND_AMOUNT wei, sending $DEFICIT wei..."
    cast send "$SIGNER" \
        --value "${DEFICIT}wei" \
        --rpc-url "$RPC_URL" \
        --private-key "$PRIVATE_KEY" \
        > /dev/null
    echo "Funded successfully"
fi
echo ""

# Step 6: Perform keyless deployment
echo "--- Step 6: Deploying ---"

cast send "$SYSTEM_CONTRACT" \
    "keylessDeploy(bytes,uint256)" \
    "0x${KEYLESS_TX}" "$GAS_OVERRIDE" \
    --rpc-url "$RPC_URL" \
    --private-key "$PRIVATE_KEY"
echo ""

# Step 7: Verify deployment
echo "--- Step 7: Verifying deployment ---"

DEPLOYED_CODE=$(cast code "$EXPECTED_ADDR" --rpc-url "$RPC_URL")
if [ "$DEPLOYED_CODE" = "0x" ]; then
    echo "ERROR: No code found at $EXPECTED_ADDR after deployment"
    exit 1
fi

CODE_LEN=$(( (${#DEPLOYED_CODE} - 2) / 2 ))
CODE_HASH=$(cast keccak "$DEPLOYED_CODE")
echo "Contract deployed successfully at $EXPECTED_ADDR ($CODE_LEN bytes, codehash: $CODE_HASH)"
