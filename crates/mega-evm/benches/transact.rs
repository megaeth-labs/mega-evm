//! Benchmarks for the `ExecuteEvm::transact()` interface.
//!
//! This benchmark suite measures the performance of transaction execution through
//! the `ExecuteEvm::transact()` interface, comparing performance across different
//! EVM specifications (EQUIVALENCE vs `MINI_REX`).
#![allow(missing_docs)]

use alloy_evm::Database;
use alloy_primitives::{address, bytes, Address, Bytes, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mega_evm::{
    test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};
use revm::{
    context::{result::ResultAndState, tx::TxEnvBuilder},
    database::{CacheDB, EmptyDB},
    primitives::{keccak256, B256},
    ExecuteEvm,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const WETH9_ADDRESS: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// WETH9 deployed runtime bytecode.
const WETH9_RUNTIME_CODE: Bytes = bytes!("6060604052600436106100af576000357c0100000000000000000000000000000000000000000000000000000000900463ffffffff16806306fdde03146100b9578063095ea7b31461014757806318160ddd146101a157806323b872dd146101ca5780632e1a7d4d14610243578063313ce5671461026657806370a082311461029557806395d89b41146102e2578063a9059cbb14610370578063d0e30db0146103ca578063dd62ed3e146103d4575b6100b7610440565b005b34156100c457600080fd5b6100cc6104dd565b6040518080602001828103825283818151815260200191508051906020019080838360005b8381101561010c5780820151818401526020810190506100f1565b50505050905090810190601f1680156101395780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b341561015257600080fd5b610187600480803573ffffffffffffffffffffffffffffffffffffffff1690602001909190803590602001909190505061057b565b604051808215151515815260200191505060405180910390f35b34156101ac57600080fd5b6101b461066d565b6040518082815260200191505060405180910390f35b34156101d557600080fd5b610229600480803573ffffffffffffffffffffffffffffffffffffffff1690602001909190803573ffffffffffffffffffffffffffffffffffffffff1690602001909190803590602001909190505061068c565b604051808215151515815260200191505060405180910390f35b341561024e57600080fd5b61026460048080359060200190919050506109d9565b005b341561027157600080fd5b610279610b05565b604051808260ff1660ff16815260200191505060405180910390f35b34156102a057600080fd5b6102cc600480803573ffffffffffffffffffffffffffffffffffffffff16906020019091905050610b18565b6040518082815260200191505060405180910390f35b34156102ed57600080fd5b6102f5610b30565b6040518080602001828103825283818151815260200191508051906020019080838360005b8381101561033557808201518184015260208101905061031a565b50505050905090810190601f1680156103625780820380516001836020036101000a031916815260200191505b509250505060405180910390f35b341561037b57600080fd5b6103b0600480803573ffffffffffffffffffffffffffffffffffffffff16906020019091908035906020019091905050610bce565b604051808215151515815260200191505060405180910390f35b6103d2610440565b005b34156103df57600080fd5b61042a600480803573ffffffffffffffffffffffffffffffffffffffff1690602001909190803573ffffffffffffffffffffffffffffffffffffffff16906020019091905050610be3565b6040518082815260200191505060405180910390f35b34600360003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825401925050819055503373ffffffffffffffffffffffffffffffffffffffff167fe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c346040518082815260200191505060405180910390a2565b60008054600181600116156101000203166002900480601f0160208091040260200160405190810160405280929190818152602001828054600181600116156101000203166002900480156105735780601f1061054857610100808354040283529160200191610573565b820191906000526020600020905b81548152906001019060200180831161055657829003601f168201915b505050505081565b600081600460003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925846040518082815260200191505060405180910390a36001905092915050565b60003073ffffffffffffffffffffffffffffffffffffffff1631905090565b600081600360008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002054101515156106dc57600080fd5b3373ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff16141580156107b457507fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff600460008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000205414155b156108cf5781600460008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020541015151561084457600080fd5b81600460008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16815260200190815260200160002060003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825403925050819055505b81600360008673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000206000828254039250508190555081600360008573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825401925050819055508273ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040518082815260200191505060405180910390a3600190509392505050565b80600360003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020016000205410151515610a2757600080fd5b80600360003373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff168152602001908152602001600020600082825403925050819055503373ffffffffffffffffffffffffffffffffffffffff166108fc829081150290604051600060405180830381858888f193505050501515610ab457600080fd5b3373ffffffffffffffffffffffffffffffffffffffff167f7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65826040518082815260200191505060405180910390a250565b600260009054906101000a900460ff1681565b60036020528060005260406000206000915090505481565b60018054600181600116156101000203166002900480601f016020809104026020016040519081016040528092919081815260200182805460018160011615610100020316600290048015610bc65780601f10610b9b57610100808354040283529160200191610bc6565b820191906000526020600020905b815481529060010190602001808311610ba957829003601f168201915b505050505081565b6000610bdb33848461068c565b905092915050565b60046020528160005260406000206020528060005260406000206000915091505054815600a165627a7a72305820deb4c2ccab3c2fdca32ab3f46728389c2fe2c165d5fafa07661e4e004f6c344a0029");

/// Calculate the storage slot for an ERC20 balance mapping.
/// Solidity mapping storage slot = keccak256(abi.encode(key, slot))
fn erc20_balance_slot(address: Address, mapping_slot: u8) -> B256 {
    let mut data = [0u8; 64];
    // Encode address (left-padded to 32 bytes)
    data[12..32].copy_from_slice(address.as_slice());
    // Encode mapping slot
    data[63] = mapping_slot;
    keccak256(data)
}

/// Helper function to create and execute a transaction.
fn execute_transaction<DB: Database>(
    spec: MegaSpecId,
    db: DB,
    caller: Address,
    callee: Address,
    value: U256,
) -> ResultAndState<mega_evm::MegaHaltReason> {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnvBuilder::new().caller(caller).call(callee).value(value).build_fill();
    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(alloy_primitives::Bytes::new());

    evm.transact(mega_tx).expect("transaction should succeed")
}

/// Helper to benchmark both specs with a database setup function.
fn bench_both_specs<DB, F>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    db_setup: F,
) where
    DB: Database + 'static,
    F: Fn() -> DB + 'static,
{
    for (name, spec) in
        [("equivalence", MegaSpecId::EQUIVALENCE), ("mini_rex", MegaSpecId::MINI_REX)]
    {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = db_setup();
                let result = execute_transaction(
                    black_box(spec),
                    black_box(db),
                    black_box(CALLER),
                    black_box(CALLEE),
                    black_box(U256::ZERO),
                );
                black_box(result)
            })
        });
    }
}

/// Benchmark empty transaction (call with no value or data).
fn bench_empty_transaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("empty_transaction");
    bench_both_specs(&mut group, CacheDB::<EmptyDB>::default);
    group.finish();
}

/// Benchmark simple ether transfer between existing accounts.
fn bench_simple_ether_transfer(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_ether_transfer");
    bench_both_specs(&mut group, || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1000))
            .account_balance(CALLEE, U256::from(100))
    });
    group.finish();
}

/// Helper to execute WETH9 transfer for a given spec.
fn execute_weth9_transfer(
    spec: MegaSpecId,
    calldata: &Bytes,
) -> ResultAndState<mega_evm::MegaHaltReason> {
    // Set up WETH9 contract with 1000 WETH balance for CALLER
    let caller_balance = U256::from(1000) * U256::from(10).pow(U256::from(18));
    let balance_slot = erc20_balance_slot(CALLER, 3); // WETH9 uses slot 3 for balances

    let db = MemoryDatabase::default()
        .account_code(WETH9_ADDRESS, WETH9_RUNTIME_CODE)
        .account_storage(WETH9_ADDRESS, balance_slot.into(), caller_balance)
        .account_balance(CALLER, U256::from(10).pow(U256::from(18))); // 1 ETH for gas

    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);

    let tx =
        TxEnvBuilder::new().caller(CALLER).call(WETH9_ADDRESS).data(calldata.clone()).build_fill();
    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(Bytes::new());

    let result = evm.transact(mega_tx).expect("transaction should succeed");

    // Assert transaction executed successfully
    assert!(result.result.is_success(), "WETH9 transfer should succeed");

    result
}

/// Benchmark WETH9 ERC20 transfer.
fn bench_weth9_transfer(c: &mut Criterion) {
    let mut group = c.benchmark_group("weth9_transfer");

    // Transfer amount: 100 WETH
    let transfer_amount = U256::from(100) * U256::from(10).pow(U256::from(18));

    // Encode transfer(address,uint256) call
    // Function selector: 0xa9059cbb
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]); // transfer selector
    calldata.extend_from_slice(&[0u8; 12]); // padding for address
    calldata.extend_from_slice(CALLEE.as_slice()); // recipient address
    calldata.extend_from_slice(&transfer_amount.to_be_bytes::<32>()); // amount
    let calldata = Bytes::from(calldata);

    for (name, spec) in
        [("equivalence", MegaSpecId::EQUIVALENCE), ("mini_rex", MegaSpecId::MINI_REX)]
    {
        group.bench_function(name, |b| {
            b.iter(|| {
                let result = execute_weth9_transfer(black_box(spec), black_box(&calldata));
                black_box(result)
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_empty_transaction,
    bench_simple_ether_transfer,
    bench_weth9_transfer
);
criterion_main!(benches);
