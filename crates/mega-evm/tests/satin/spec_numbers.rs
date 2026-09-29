//! The numbers `docs/spec/upgrades/satin.md` states for Satin, against the constants and the
//! schedule.
//!
//! The page states the spec's own prices. The schedule compared with it is built at those prices,
//! so a measurement build's byte prices do not move the check. The developer-impact paragraph is a
//! measurement of a few transactions and is not part of this check.

use mega_evm::{
    alloy_primitives::Address,
    constants::{
        ACCOUNT_STATE_GAS, BLOCK_DATA_LIMIT, BLOCK_ENV_ACCESS_COMPUTE_GAS, COST_PER_HISTORY_BYTE,
        COST_PER_STATE_BYTE, MAX_CONTRACT_SIZE, MAX_INITCODE_SIZE, MAX_TX_COMPUTE_GAS,
        ORACLE_ACCESS_COMPUTE_GAS, SLOT_STATE_GAS, TX_DATA_LIMIT, TX_GAS_LIMIT_CAP,
    },
    kzg_point_evaluation,
    op_revm::precompiles::{
        bls12_381::{
            JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE, JOVIAN_G2_MSM,
            JOVIAN_G2_MSM_MAX_INPUT_SIZE, JOVIAN_PAIRING, JOVIAN_PAIRING_MAX_INPUT_SIZE,
        },
        bn254_pair::{KARST, KARST_MAX_INPUT_SIZE},
    },
    revm::{
        context_interface::cfg::{gas::WARM_STORAGE_READ_COST, GasId},
        handler::SYSTEM_CALL_REGULAR_GAS_LIMIT,
        precompile::secp256r1::{P256VERIFY_BASE_GAS_FEE_OSAKA, P256VERIFY_OSAKA},
        primitives::{
            eip2780::{TX_BASE_COST, TX_VALUE_COST},
            eip7708::{ETH_TRANSFER_LOG_ADDRESS, ETH_TRANSFER_LOG_TOPIC},
            eip8037::{
                AUTH_BASE_BYTES, CODE_DEPOSIT_PER_BYTE, NEW_ACCOUNT_BYTES, SSTORE_SET_BYTES,
            },
            eip8038::COLD_ACCOUNT_ACCESS,
        },
    },
    satin_gas_params, satin_gas_params_at, satin_precompiles,
    system::{
        keyless::{KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS},
        CREATE2_FACTORY_ADDRESS, CREATE2_FACTORY_NONCE, SLOT_NUM_ACCESS_TYPE,
    },
    LimitKind, ProtocolLimits, SatinPrices, VolatileDataAccess, ACCESS_LIST_ADDRESS_SIZE,
    ACCESS_LIST_SLOT_SIZE, AUTHORIZATION_SIZE, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, HISTORY_GAS_PRICED, LOG_BASE_SIZE, LOG_TOPIC_SIZE, MIN_BUCKET_SIZE,
    STATE_GAS_REPRICED, STORAGE_CALL_STIPEND_BYTES, SYSTEM_ADDRESS, TRANSFER_LOG_SIZE,
    TX_BASE_SIZE, TX_BODY_SIZE, TX_FIXED_WRITE_RECORDS, WRITE_RECORD_SIZE,
};

const SATIN_MD: &str = include_str!("../../../../docs/spec/upgrades/satin.md");

/// Every number the Satin upgrade page states, read back from the code it describes.
#[test]
fn test_satin_md_matches_the_constants() {
    let doc = SATIN_MD;
    named_constants(doc);
    schedule(doc);
    state_and_history(doc);
    limits(doc);
    precompiles(doc);
    addresses(doc);
    detention_and_stops(doc);
}

fn named_constants(doc: &str) {
    require_eq(doc, "COST_PER_STATE_BYTE", COST_PER_STATE_BYTE);
    require_eq(doc, "COST_PER_HISTORY_BYTE", COST_PER_HISTORY_BYTE);
    require_eq(doc, "EXECUTION_CAP", TX_GAS_LIMIT_CAP);
    require_eq(doc, "MAX_CONTRACT_SIZE", MAX_CONTRACT_SIZE as u64);
    require_eq(doc, "MAX_INITCODE_SIZE", MAX_INITCODE_SIZE as u64);
    require_eq(doc, "TX_DATA_LIMIT", TX_DATA_LIMIT);
    require_eq(doc, "TX_BODY_SIZE", TX_BODY_SIZE);
    require_eq(doc, "TX_BASE_SIZE", TX_BASE_SIZE);
    require_eq(doc, "TX_FIXED_WRITE_RECORDS", TX_FIXED_WRITE_RECORDS);
    require_eq(doc, "ACCESS_LIST_ADDRESS_SIZE", ACCESS_LIST_ADDRESS_SIZE);
    require_eq(doc, "ACCESS_LIST_SLOT_SIZE", ACCESS_LIST_SLOT_SIZE);
    require_eq(doc, "AUTHORIZATION_SIZE", AUTHORIZATION_SIZE);
    require_eq(doc, "WRITE_RECORD_SIZE", WRITE_RECORD_SIZE);
    require_eq(doc, "TRANSFER_LOG_SIZE", TRANSFER_LOG_SIZE);
    require_eq(doc, "HISTORY_ALLOWANCE_BYTES", STORAGE_CALL_STIPEND_BYTES);
    require_eq(doc, "MIN_BUCKET_SIZE", MIN_BUCKET_SIZE as u64);
    require_eq(doc, "KEYLESS_DEPLOY_OVERHEAD_GAS", KEYLESS_DEPLOY_OVERHEAD_GAS);
    require_eq(doc, "BLOCK_ENV_ACCESS_COMPUTE_GAS", BLOCK_ENV_ACCESS_COMPUTE_GAS);
    require_eq(doc, "ORACLE_ACCESS_COMPUTE_GAS", ORACLE_ACCESS_COMPUTE_GAS);
    require_eq(doc, "SYSTEM_CALL_REGULAR_GAS_LIMIT", SYSTEM_CALL_REGULAR_GAS_LIMIT);
    require_eq(doc, "TX_BASE_COST", TX_BASE_COST);

    require(
        doc,
        &format!(
            "`LOG_BASE_SIZE` = {}, plus {} per topic",
            grouped(LOG_BASE_SIZE),
            grouped(LOG_TOPIC_SIZE)
        ),
    );
    require(doc, &format!("({} gas)", grouped(STORAGE_CALL_STIPEND_BYTES * COST_PER_HISTORY_BYTE)));
    require(doc, &format!("capped at {} per transaction", grouped(TX_GAS_LIMIT_CAP)));
    require(doc, &format!("at most {} of regular gas", grouped(SYSTEM_CALL_REGULAR_GAS_LIMIT)));
    require(doc, &format!("max(block_gas_limit, {})", grouped(SYSTEM_CALL_REGULAR_GAS_LIMIT)));
    require(doc, &format!("A {}-byte allowance", STORAGE_CALL_STIPEND_BYTES));
    require(
        doc,
        &format!(
            "`FRAME_SHARE_NUMERATOR / FRAME_SHARE_DENOMINATOR` = {}/{}",
            FRAME_DATA_SHARE_NUMERATOR, FRAME_DATA_SHARE_DENOMINATOR
        ),
    );
    require(doc, &format!("KV × {WRITE_RECORD_SIZE} ≤ data size"));
    let records = TX_DATA_LIMIT / WRITE_RECORD_SIZE;
    require(
        doc,
        &format!(
            "fewer than {} write records ({} / {})",
            grouped(records),
            grouped(TX_DATA_LIMIT),
            WRITE_RECORD_SIZE
        ),
    );
}

fn schedule(doc: &str) {
    let spec = satin_gas_params_at(SatinPrices::CONSTANTS);
    let page = section(doc, "### 5. Gas Schedule", "### 6. State Gas");
    let rows = [
        ("Warm storage read", GasId::warm_storage_read_cost()),
        ("Cold account access, additional", GasId::cold_account_additional_cost()),
        ("Cold storage access, additional", GasId::cold_storage_additional_cost()),
        ("Cold `SLOAD`", GasId::cold_storage_cost()),
        ("Value transfer of a call", GasId::transfer_value_cost()),
        ("New account of a call", GasId::new_account_cost()),
        ("New account of `SELFDESTRUCT`", GasId::new_account_cost_for_selfdestruct()),
        ("`SSTORE` static cost", GasId::sstore_static()),
        ("`SSTORE` of a fresh slot, before the load cost", GasId::sstore_set_without_load_cost()),
        ("`SSTORE` reset, before the cold load", GasId::sstore_reset_without_cold_load_cost()),
        ("`SSTORE` refund of a restored fresh slot", GasId::sstore_set_refund()),
        ("`SSTORE` refund of a restored reset", GasId::sstore_reset_refund()),
        ("`SSTORE` refund of a cleared slot", GasId::sstore_clearing_slot_refund()),
        ("`CREATE` and `CREATE2`", GasId::create()),
        ("Creation transaction (unused under EIP-2780)", GasId::tx_create_cost()),
        ("Access-list address", GasId::tx_access_list_address_cost()),
        ("Access-list storage key", GasId::tx_access_list_storage_key_cost()),
        ("Code deposit, regular gas per byte", GasId::code_deposit_cost()),
        ("Code deposit, state gas per byte", GasId::code_deposit_state_gas()),
        ("Code deposit, history gas per byte", GasId::code_deposit_history_gas()),
        ("Fresh-slot `SSTORE`, state gas", GasId::sstore_set_state_gas()),
        ("New account, state gas", GasId::new_account_state_gas()),
        ("Created account, state gas", GasId::create_state_gas()),
        ("EIP-7702 delegation indicator, state gas", GasId::tx_eip7702_state_gas_bytecode()),
        ("EIP-7702 authorization, intrinsic regular gas", GasId::tx_eip7702_regular_gas()),
        ("EIP-7702 refund for an existing authority", GasId::tx_eip7702_regular_refund()),
        ("Account write (EIP-2780)", GasId::tx_account_write_cost()),
        ("Creation access (EIP-2780)", GasId::tx_create_access_cost()),
        ("Floor cost per token", GasId::tx_floor_cost_per_token()),
        ("Floor base", GasId::tx_floor_cost_base_gas()),
        ("Floor tokens per zero calldata byte", GasId::tx_floor_token_zero_byte_multiplier()),
        ("Floor tokens per access-list byte", GasId::tx_access_list_floor_byte_multiplier()),
    ];
    for (label, id) in rows {
        let shown = *table_row(page, label).last().expect("a satin cell");
        assert_eq!(shown, grouped(spec.get(id)), "{label}");
        if !byte_priced(id) {
            assert_eq!(satin_gas_params().get(id), spec.get(id), "{label} is not a byte price");
        }
    }
    if !crate::common::runs_at_measurement_prices() {
        let live = satin_gas_params();
        for (label, id) in rows {
            assert_eq!(live.get(id), spec.get(id), "{label} at the prices in effect");
        }
    }

    let base = TX_BASE_COST;
    let access = COLD_ACCOUNT_ACCESS;
    let value = TX_VALUE_COST;
    let creation = spec.get(GasId::tx_create_access_cost());
    require(
        page,
        &format!(
            "MUST be `TX_BASE_COST` = {}, plus, for a call to another account, {} for the \
             recipient's access and {} more when it carries value; plus, for a creation, {}",
            grouped(base),
            grouped(access),
            grouped(value),
            grouped(creation)
        ),
    );
}

fn state_and_history(doc: &str) {
    let spec = satin_gas_params_at(SatinPrices::CONSTANTS);
    let bytes = SSTORE_SET_BYTES;
    assert_eq!(spec.get(GasId::sstore_set_state_gas()), bytes * COST_PER_STATE_BYTE);
    assert_eq!(spec.get(GasId::sstore_set_state_gas()), SLOT_STATE_GAS);
    let account = NEW_ACCOUNT_BYTES;
    assert_eq!(spec.get(GasId::new_account_state_gas()), account * COST_PER_STATE_BYTE);
    assert_eq!(spec.get(GasId::new_account_state_gas()), ACCOUNT_STATE_GAS);
    assert_eq!(spec.get(GasId::create_state_gas()), ACCOUNT_STATE_GAS);
    let code = CODE_DEPOSIT_PER_BYTE;
    assert_eq!(spec.get(GasId::code_deposit_state_gas()), code * COST_PER_STATE_BYTE);
    let indicator = AUTH_BASE_BYTES;
    assert_eq!(spec.get(GasId::tx_eip7702_state_gas_bytecode()), indicator * COST_PER_STATE_BYTE);
    assert_eq!(spec.get(GasId::code_deposit_history_gas()), COST_PER_HISTORY_BYTE);

    let page = section(doc, "### 6. State Gas", "### 7. History Gas");
    assert_state_row(page, "Fresh storage slot", bytes, SLOT_STATE_GAS);
    assert_state_row(page, "| New account ", account, ACCOUNT_STATE_GAS);
    assert_state_row(page, "Created contract account", account, ACCOUNT_STATE_GAS);
    assert_state_row(page, "Deployed code, per byte", code, code * COST_PER_STATE_BYTE);
    assert_state_row(
        page,
        "EIP-7702 delegation indicator",
        indicator,
        indicator * COST_PER_STATE_BYTE,
    );
    require(doc, &format!("`{} × m`", grouped(ACCOUNT_STATE_GAS)));
}

fn limits(doc: &str) {
    let defaults = ProtocolLimits::DEFAULT;
    assert_eq!(defaults.tx_runtime_limits.tx_data_size_limit, TX_DATA_LIMIT);
    assert_eq!(defaults.block_txs_data_limit, BLOCK_DATA_LIMIT);
    assert_eq!(TX_DATA_LIMIT, BLOCK_DATA_LIMIT, "the page states one data-size number for both");
    assert_eq!(defaults.tx_runtime_limits.tx_kv_update_limit, u64::MAX);
    assert_eq!(defaults.tx_runtime_limits.tx_state_gas_limit, u64::MAX);
    assert_eq!(defaults.block_execution_gas_limit, u64::MAX);
    assert_eq!(defaults.block_state_gas_limit, u64::MAX);
    assert_eq!(defaults.block_kv_update_limit, u64::MAX);
    assert_eq!(
        defaults.tx_runtime_limits.block_env_access_compute_gas_limit,
        BLOCK_ENV_ACCESS_COMPUTE_GAS
    );
    assert_eq!(
        defaults.tx_runtime_limits.oracle_access_compute_gas_limit,
        ORACLE_ACCESS_COMPUTE_GAS
    );
    assert_eq!(
        BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS,
        "the page states one detention cap for both"
    );

    let resources = section(doc, "### 10. Resource Limits", "### 11. The Revert-Class");
    assert!(
        table_row(resources, "KV updates").iter().any(|cell| cell.starts_with("Unlimited")),
        "the KV limit"
    );
    assert!(
        table_row(resources, "State gas").iter().any(|cell| cell.starts_with("Unlimited")),
        "the state-gas limit"
    );

    let blocks = section(doc, "### 20. Block Limits", "### 21. Transaction and Block");
    assert_eq!(*table_row(blocks, "Execution gas").last().unwrap(), "Unlimited");
    assert_eq!(*table_row(blocks, "State gas").last().unwrap(), "Unlimited");
    assert_eq!(*table_row(blocks, "KV updates").last().unwrap(), "Unlimited");
    assert_eq!(
        *table_row(blocks, "Data size").last().unwrap(),
        format!("{} bytes", grouped(BLOCK_DATA_LIMIT))
    );

    let safety = section(doc, "## Safety and Compatibility", "## References");
    require(
        safety,
        &format!(
            "the data-size limits are {} bytes, and the detention caps are {} each",
            grouped(TX_DATA_LIMIT),
            grouped(BLOCK_ENV_ACCESS_COMPUTE_GAS)
        ),
    );
}

fn precompiles(doc: &str) {
    let page = section(doc, "### 2. Precompiles", "### 3. Contract Size");
    let kzg = kzg_point_evaluation::ADDRESS;
    assert_precompile(
        page,
        "KZG point evaluation",
        kzg,
        &format!("Fixed {} gas", grouped(kzg_point_evaluation::GAS_COST)),
    );
    require(doc, &format!("exactly {}", grouped(kzg_point_evaluation::GAS_COST)));
    require(doc, &format!("still at {}", grouped(kzg_point_evaluation::GAS_COST)));

    let p256_gas = P256VERIFY_BASE_GAS_FEE_OSAKA;
    assert_precompile(
        page,
        "P256VERIFY",
        *P256VERIFY_OSAKA.address(),
        &format!("{} gas", grouped(p256_gas)),
    );

    let bn_bound = KARST_MAX_INPUT_SIZE as u64;
    assert_precompile(
        page,
        "BN254 pairing",
        *KARST.address(),
        &format!("at most {} bytes", grouped(bn_bound)),
    );

    assert_precompile(
        page,
        "BLS12-381 G1 MSM",
        *JOVIAN_G1_MSM.address(),
        &format!("at most {} bytes", grouped(JOVIAN_G1_MSM_MAX_INPUT_SIZE as u64)),
    );
    assert_precompile(
        page,
        "BLS12-381 G2 MSM",
        *JOVIAN_G2_MSM.address(),
        &format!("at most {} bytes", grouped(JOVIAN_G2_MSM_MAX_INPUT_SIZE as u64)),
    );
    assert_precompile(
        page,
        "BLS12-381 pairing",
        *JOVIAN_PAIRING.address(),
        &format!("at most {} bytes", grouped(JOVIAN_PAIRING_MAX_INPUT_SIZE as u64)),
    );
    let installed = satin_precompiles();
    for address in [
        kzg,
        *P256VERIFY_OSAKA.address(),
        *KARST.address(),
        *JOVIAN_G1_MSM.address(),
        *JOVIAN_G2_MSM.address(),
        *JOVIAN_PAIRING.address(),
    ] {
        assert!(installed.contains(&address), "{address} is in the Satin set");
    }
}

fn addresses(doc: &str) {
    require_addr(doc, &format!("`KEYLESS_DEPLOY_ADDRESS` = `{KEYLESS_DEPLOY_ADDRESS}`"));
    require_addr(doc, &CREATE2_FACTORY_ADDRESS.to_string());
    require(doc, &format!("with nonce {CREATE2_FACTORY_NONCE}"));
    assert_eq!(
        ETH_TRANSFER_LOG_ADDRESS, SYSTEM_ADDRESS,
        "the transfer log and the pre-block caller are the address the page names once"
    );
    require_addr(doc, &SYSTEM_ADDRESS.to_string());
    require_addr(doc, &ETH_TRANSFER_LOG_TOPIC.to_string());
}

fn detention_and_stops(doc: &str) {
    let base = TX_BASE_COST;
    assert_eq!(MAX_TX_COMPUTE_GAS, TX_GAS_LIMIT_CAP - base - WARM_STORAGE_READ_COST);
    let detention = section(doc, "### 12. Gas Detention", "### 13. Volatile-Data");
    require(detention, &format!("below {}", grouped(MAX_TX_COMPUTE_GAS)));
    require(
        detention,
        &format!(
            "base cost of {}, which every transaction pays, and the {} of a warm account access",
            grouped(base),
            WARM_STORAGE_READ_COST
        ),
    );

    let stops = section(doc, "### 11. The Revert-Class", "### 12. Gas Detention");
    let kinds = [
        (LimitKind::DataSize, "Data size"),
        (LimitKind::KVUpdate, "KV updates"),
        (LimitKind::ComputeGas, "Compute (gas detention)"),
        (LimitKind::StateGrowth, "State growth"),
    ];
    for (kind, name) in kinds {
        let cells = row_with_first(stops, &kind.as_u8().to_string());
        assert_eq!(cells[1], name, "kind {}", kind.as_u8());
    }

    let access = section(doc, "### 13. Volatile-Data", "### 14. EIP-7708");
    let reads = [
        (VolatileDataAccess::BLOCK_NUMBER, "`NUMBER`"),
        (VolatileDataAccess::TIMESTAMP, "`TIMESTAMP`"),
        (VolatileDataAccess::COINBASE, "`COINBASE`"),
        (VolatileDataAccess::GAS_LIMIT, "`GASLIMIT`"),
        (VolatileDataAccess::BASE_FEE, "`BASEFEE`"),
        (VolatileDataAccess::PREV_RANDAO, "`PREVRANDAO`"),
        (VolatileDataAccess::BLOCK_HASH, "`BLOCKHASH`"),
        (VolatileDataAccess::BLOB_BASE_FEE, "`BLOBBASEFEE`"),
        (VolatileDataAccess::BENEFICIARY_BALANCE, "block beneficiary's account"),
        (VolatileDataAccess::ORACLE, "Oracle's storage"),
        (VolatileDataAccess::SLOT_NUM, "`SLOTNUM`"),
    ];
    for (kind, name) in reads {
        let cells = row_with_first(access, &kind.as_u8().to_string());
        assert!(cells[1].contains(name), "access {} is {name}, row {cells:?}", kind.as_u8());
    }
    assert_eq!(VolatileDataAccess::SLOT_NUM.as_u8(), SLOT_NUM_ACCESS_TYPE);
    require(
        access,
        &format!(
            "Values {} (`Difficulty`) and {} (`BlobHash`) are not produced.",
            VolatileDataAccess::DIFFICULTY.as_u8(),
            VolatileDataAccess::BLOB_HASH.as_u8()
        ),
    );
}

fn byte_priced(id: GasId) -> bool {
    STATE_GAS_REPRICED.iter().any(|&(entry, _)| entry() == id) ||
        HISTORY_GAS_PRICED.iter().any(|&(entry, _)| entry() == id)
}

fn require_eq(doc: &str, name: &str, value: u64) {
    require(doc, &format!("`{name}` = {}", grouped(value)));
}

fn require(doc: &str, phrase: &str) {
    assert!(doc.contains(phrase), "satin.md is missing {phrase}");
}

fn require_addr(doc: &str, phrase: &str) {
    assert!(
        doc.to_ascii_lowercase().contains(&phrase.to_ascii_lowercase()),
        "satin.md is missing {phrase}"
    );
}

fn section<'a>(doc: &'a str, start: &str, end: &str) -> &'a str {
    let at = doc.find(start).unwrap_or_else(|| panic!("satin.md is missing {start}"));
    let rest = &doc[at..];
    let stop = rest[start.len()..]
        .find(end)
        .map(|index| index + start.len())
        .unwrap_or_else(|| panic!("satin.md is missing {end}"));
    &rest[..stop]
}

fn table_row<'a>(doc: &'a str, label: &str) -> Vec<&'a str> {
    let line = doc
        .lines()
        .find(|line| line.contains(label) && line.contains('|'))
        .unwrap_or_else(|| panic!("satin.md has no table row containing {label}"));
    cells(line)
}

fn row_with_first<'a>(doc: &'a str, first: &str) -> Vec<&'a str> {
    doc.lines()
        .map(cells)
        .find(|cells| cells.first() == Some(&first))
        .unwrap_or_else(|| panic!("satin.md has no table row starting with {first}"))
}

fn cells(line: &str) -> Vec<&str> {
    line.split('|').map(str::trim).filter(|cell| !cell.is_empty()).collect()
}

fn assert_state_row(doc: &str, label: &str, bytes: u64, entry: u64) {
    let cells = table_row(doc, label);
    assert_eq!(cells[1], grouped(bytes), "{label} bytes");
    assert_eq!(cells[2], grouped(entry), "{label} entry");
}

fn assert_precompile(doc: &str, label: &str, address: Address, tail: &str) {
    let cells = table_row(doc, label);
    let shown = cells[1].trim_matches('`');
    let parsed = short_address(shown);
    assert_eq!(parsed, address, "{label}");
    assert!(cells[2].contains(tail), "{label} should state {tail}, row says {}", cells[2]);
}

/// A precompile address as the page writes it (`0x0A`, `0x100`), left-padded to 20 bytes.
fn short_address(shown: &str) -> Address {
    let hex = shown.trim().trim_start_matches("0x").trim_start_matches("0X");
    let value =
        u64::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("not a short address: {shown}"));
    Address::left_padding_from(&value.to_be_bytes())
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out.chars().rev().collect()
}
