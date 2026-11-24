//! The legacy leg runs the line the chain ran, at the versions it shipped with.
//!
//! The released `mega-evme` 1.7.1 and `mega-evm` 1.7.1 are pinned exactly in the manifest, but
//! what they depend on is resolved in this workspace's lockfile, where a `cargo update` could
//! move it. Every package below is pinned to the version the 1.7.1 release's own lockfile holds;
//! a change of any of them changes how history replays and has to be a decision, made here.
//!
//! Many of them the leg shares with the Satin engine and the tool, and a lockfile holds one
//! version of a package per compatibility line, so the two engines run the same copy. Where the
//! Satin engine's own dependencies require a newer version than the release shipped, the leg
//! cannot have the released one; those packages are pinned at the version the workspace holds,
//! next to the one the release held, so that a further move is a decision too.

/// The workspace lockfile.
const LOCKFILE: &str = include_str!("../../../Cargo.lock");

/// The legacy line: every package whose version decides what the legacy leg executes or prints,
/// at the version `v1.7.1`'s `Cargo.lock` holds.
const LEGACY_LINE: &[(&str, &str)] = &[
    // The released engine, CLI and the crates they are published with.
    ("mega-evm", "1.7.1"),
    ("mega-evme", "1.7.1"),
    ("mega-state-test", "1.7.1"),
    ("mega-system-contracts", "1.7.1"),
    // revm 27 and op-revm 8: what executes.
    ("revm", "27.1.0"),
    ("revm-bytecode", "6.2.1"),
    ("revm-context", "8.0.4"),
    ("revm-context-interface", "9.0.0"),
    ("revm-database", "7.0.4"),
    ("revm-database-interface", "7.0.4"),
    ("revm-handler", "8.1.0"),
    ("revm-inspector", "8.1.0"),
    ("revm-interpreter", "24.0.0"),
    ("revm-precompile", "25.0.0"),
    ("revm-primitives", "20.2.1"),
    ("revm-state", "7.0.4"),
    ("op-revm", "8.1.0"),
    // The block executor's layer and the hardfork tables.
    ("alloy-evm", "0.15.0"),
    ("alloy-op-evm", "0.15.0"),
    ("alloy-hardforks", "0.2.13"),
    ("alloy-op-hardforks", "0.2.13"),
    // What the legacy CLI decodes, encodes, fetches and prints.
    ("alloy-consensus", "1.1.0"),
    ("alloy-eips", "1.8.3"),
    ("alloy-network", "1.1.0"),
    ("alloy-provider", "1.1.0"),
    ("alloy-rpc-types-eth", "1.1.0"),
    ("alloy-rpc-types-trace", "1.0.24"),
    ("op-alloy-consensus", "0.18.14"),
    ("op-alloy-network", "0.18.14"),
    ("op-alloy-rpc-types", "0.18.14"),
    ("revm-inspectors", "0.27.3"),
    // The precompiles' backends, as revm-precompile 25 selects them: secp256k1 for ecrecover,
    // sha2, ripemd, the modexp crate, arkworks for BN254, blst for BLS12-381, c-kzg for the point
    // evaluation, p256 for P256VERIFY; k256 recovers EIP-7702 authorities.
    ("secp256k1", "0.31.1"),
    ("secp256k1-sys", "0.11.0"),
    ("sha2", "0.10.9"),
    ("ripemd", "0.1.3"),
    ("aurora-engine-modexp", "1.2.0"),
    ("ark-bn254", "0.5.0"),
    ("ark-ec", "0.5.0"),
    ("ark-ff", "0.5.0"),
    ("ark-ff-asm", "0.5.0"),
    ("ark-ff-macros", "0.5.0"),
    ("ark-serialize", "0.5.0"),
    ("blst", "0.3.16"),
    ("c-kzg", "2.1.7"),
    ("p256", "0.13.2"),
    ("primeorder", "0.13.6"),
    ("k256", "0.13.4"),
    ("ecdsa", "0.16.9"),
    ("elliptic-curve", "0.13.8"),
    ("crypto-bigint", "0.5.5"),
    // Hashing and word arithmetic.
    ("alloy-primitives", "1.6.0"),
    ("ruint", "1.17.2"),
    ("sha3", "0.11.0"),
    ("keccak", "0.2.0"),
    // Encodings: access lists and authorizations, trie paths, the FastLZ size the L1 fee is
    // priced on, and the roots a dumped fixture carries.
    ("alloy-eip2930", "0.2.3"),
    ("alloy-eip7702", "0.6.3"),
    ("nybbles", "0.4.3"),
    ("op-alloy-flz", "0.13.1"),
    ("triehash", "0.8.4"),
    ("rlp", "0.5.2"),
    ("hash-db", "0.15.2"),
];

/// The packages of the legacy leg's encodings that the Satin engine's alloy 2 line holds above
/// the released version: `(name, locked, released)`. The alloy 2 line the workspace locks (2.1)
/// requires `alloy-rlp` 0.3.14, `alloy-trie` 0.9.2 and `alloy-sol-types` 1.6.0 at least, so the leg
/// runs these at `locked`.
const AHEAD_OF_THE_RELEASE: &[(&str, &str, &str)] = &[
    ("alloy-rlp", "0.3.16", "0.3.12"),
    ("alloy-rlp-derive", "0.3.16", "0.3.12"),
    ("alloy-trie", "0.9.5", "0.9.0"),
    ("alloy-sol-types", "1.6.0", "1.4.1"),
    ("alloy-sol-macro", "1.6.0", "1.4.1"),
    ("alloy-sol-macro-expander", "1.6.0", "1.4.1"),
    ("alloy-sol-macro-input", "1.6.0", "1.4.1"),
    ("syn-solidity", "1.6.0", "1.4.1"),
];

/// Every `(name, version)` package in `lockfile`.
fn locked_packages(lockfile: &str) -> Vec<(&str, &str)> {
    lockfile
        .split("[[package]]")
        .skip(1)
        .filter_map(|entry| {
            let field = |key: &str| {
                entry.lines().find_map(|line| {
                    line.strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"')
                })
            };
            Some((field("name")?, field("version")?))
        })
        .collect()
}

/// The semver-compatibility line of `version`: its major, or `0.minor` below 1.
fn compatibility_line(version: &str) -> &str {
    let mut parts = version.splitn(3, '.');
    let major = parts.next().unwrap_or_default();
    if major == "0" {
        let minor_len = parts.next().map_or(0, str::len);
        &version[..major.len() + 1 + minor_len]
    } else {
        major
    }
}

/// The versions of `name` the lockfile holds on `pinned`'s compatibility line.
fn locked_on_the_line_of<'a>(locked: &[(&str, &'a str)], name: &str, pinned: &str) -> Vec<&'a str> {
    let line = compatibility_line(pinned);
    locked
        .iter()
        .filter(|(n, v)| *n == name && compatibility_line(v) == line)
        .map(|(_, v)| *v)
        .collect()
}

#[test]
fn test_the_legacy_line_is_at_its_released_versions() {
    let locked = locked_packages(LOCKFILE);
    for &(name, pinned) in LEGACY_LINE {
        let versions = locked_on_the_line_of(&locked, name, pinned);
        assert_eq!(
            versions,
            [pinned],
            "{name}: the legacy line resolves {versions:?}, the 1.7.1 release shipped {pinned}"
        );
    }
}

#[test]
fn test_the_packages_ahead_of_the_release_stay_where_they_are() {
    let locked = locked_packages(LOCKFILE);
    for &(name, pinned, released) in AHEAD_OF_THE_RELEASE {
        assert_ne!(pinned, released, "{name} is at its released version: move it to the line");
        let versions = locked_on_the_line_of(&locked, name, pinned);
        assert_eq!(
            versions,
            [pinned],
            "{name}: the legacy leg resolves {versions:?}, pinned at {pinned} (released {released})"
        );
    }
}

#[test]
fn test_the_lockfile_parser_reads_packages() {
    let lock = "[[package]]\nname = \"revm\"\nversion = \"27.1.0\"\nsource = \"registry\"\n\n\
                [[package]]\nname = \"revm\"\nversion = \"40.0.3\"\n";
    assert_eq!(locked_packages(lock), [("revm", "27.1.0"), ("revm", "40.0.3")]);
    assert_eq!(compatibility_line("27.1.0"), "27");
    assert_eq!(compatibility_line("0.18.14"), "0.18");
    assert_eq!(compatibility_line("2.0.0-alpha.1"), "2");
}

/// The released Rex6 block executor holds a Mega System Transaction to block admission as it
/// holds any legacy envelope: run in a block whose data-size, KV-update or state-growth budget is
/// reached, or whose building policy the envelope exceeds, it is refused before it runs, with the
/// error the reached limit names; in a block whose limits it fits, the same transaction is
/// executed. Its exemption from per-transaction resource metering does not exempt it from block
/// packing.
#[cfg(feature = "legacy")]
#[test]
fn test_rex6_system_transaction_block_admission_is_unchanged() {
    use mega_evm_legacy::{
        alloy_consensus::{transaction::Recovered, Signed, TxLegacy},
        alloy_evm::{block::BlockExecutor, EvmEnv},
        alloy_op_evm::block::OpAlloyReceiptBuilder,
        alloy_primitives::{address, Bytes, Signature, TxKind, B256, U256},
        check_if_mega_system_transaction,
        revm::{
            context::{BlockEnv, CfgEnv},
            database::State,
        },
        test_utils::MemoryDatabase,
        BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHardfork,
        MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, SequencerRegistryConfig,
        SequencerRegistryRex6Config, TestExternalEnvs, MEGA_SYSTEM_ADDRESS,
        ORACLE_CONTRACT_ADDRESS,
    };

    const CHAIN_ID: u64 = 4326;
    const BLOCK_GAS_LIMIT: u64 = 1_000_000_000;

    assert!(check_if_mega_system_transaction(
        MEGA_SYSTEM_ADDRESS,
        0,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        MEGA_SYSTEM_ADDRESS,
    ));
    let system_tx = Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(CHAIN_ID),
                nonce: 0,
                gas_price: 0,
                gas_limit: 100_000,
                to: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            Signature::test_signature(),
            B256::with_last_byte(1),
        )),
        MEGA_SYSTEM_ADDRESS,
    );
    // A Rex6 block, the system transaction its only transaction, under `limits`.
    let run = |limits: BlockLimits| {
        let chain = MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX6)
            .with_params(SequencerRegistryConfig {
                rex5_initial_sequencer: address!("0x4000000000000000000000000000000000000004"),
                rex5_initial_admin: address!("0x5000000000000000000000000000000000000005"),
            })
            .with_params(SequencerRegistryRex6Config { rex6_min_rotation_delay: 100 });
        let factory = MegaBlockExecutorFactory::new(
            chain,
            MegaEvmFactory::new()
                .with_external_env_factory(TestExternalEnvs::<std::convert::Infallible>::new()),
            OpAlloyReceiptBuilder::default(),
        );
        let mut cfg = CfgEnv::default();
        cfg.spec = MegaSpecId::REX6;
        cfg.chain_id = CHAIN_ID;
        let block = BlockEnv {
            number: U256::from(1_000),
            timestamp: U256::from(1_800_000_000),
            gas_limit: BLOCK_GAS_LIMIT,
            basefee: 0,
            ..Default::default()
        };
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        let ctx = MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), limits);
        let mut executor = factory.create_executor(&mut state, ctx, EvmEnv::new(cfg, block));
        executor.apply_pre_execution_changes().expect("the Rex6 block starts");
        executor.run_transaction(system_tx.as_recovered_ref()).map(drop).map_err(|e| e.to_string())
    };

    let limits =
        BlockLimits::from_hardfork_and_block_gas_limit(MegaHardfork::Rex6, BLOCK_GAS_LIMIT);
    run(limits).expect("within the block's limits the system transaction is executed");
    for (name, limits, refusal) in [
        ("data", limits.with_block_txs_data_limit(0), "Block transactions data limit reached"),
        ("KV", limits.with_block_kv_update_limit(0), "Block KV update limit reached"),
        ("growth", limits.with_block_state_growth_limit(0), "Block state growth limit reached"),
        ("gas", limits.with_tx_gas_limit(0), "Transaction gas limit exceeded"),
        (
            "encoded tx",
            limits.with_tx_encode_size_limit(0),
            "Transaction encode size limit exceeded",
        ),
        (
            "encoded block",
            limits.with_block_txs_encode_size_limit(0),
            "Block transactions encode size limit exceeded",
        ),
        (
            "DA tx",
            limits.with_tx_da_size_limit(0),
            "Transaction data availability size limit exceeded",
        ),
        (
            "DA block",
            limits.with_block_da_size_limit(0),
            "Block data availability size limit exceeded",
        ),
    ] {
        let error = run(limits).expect_err("the legacy envelope is held to block admission");
        assert!(error.contains(refusal), "{name}: {error}");
    }
}
