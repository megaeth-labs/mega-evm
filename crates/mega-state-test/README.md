# mega-state-test

The execution-spec state-test runner of the Satin engine.
It runs Ethereum's execution-spec state-test fixtures through `MegaEvm`; the `state-test` CLI (`crates/state-test`) is its front end.
The library keeps the `state_test` import name.
It also imports the execution-spec blockchain tests through Satin's block executor ([Blockchain tests](#blockchain-tests)).

## Two modes

- **Equivalence** — a gate.
  Satin's machinery (its handler, frame lifecycle, Host and instruction table) runs priced as the fixture's own fork prices it, with every dimension of pricing only MegaETH has turned off.
  Every test either passes, is skipped for a reason the reference runner shares, or fails on a [registered deviation](DEVIATIONS.md); one failure no deviation explains fails the gate.
- **Satin** — a report.
  The same fixtures under Satin's own configuration, counted by outcome.
  Satin prices state, history and the access entries it pressed back on purpose, and logs every value movement (EIP-7708) where Osaka's fixtures log none, so almost every stateful fixture differs; the count is what CI shows, and it never fails the job.

A run executes the entries one fork defines: Osaka's in the main fixture release, Amsterdam's in the glamsterdam devnet release.
Satin's base spec is Osaka, so those are the two forks it can be configured to.

## The neutral configuration

Equivalence mode is built from test tooling in `mega-evm`, behind its `test-utils` feature, and unreachable from a default build:

- `MegaContext::with_neutral_cfg` takes every configuration field as given, the ones the Satin spec fixes included, and prices no history gas for any transaction.
- `test_utils::neutral_cfg(fork)` is that configuration for Osaka or Amsterdam: the fork's gas schedule, its EIP-8037, EIP-2780 and EIP-7708 switches, EIP-7825's execution cap and its code-size limits.
- `test_utils::neutralize_evm(evm, fork)` gives the EVM the two parts of the fork's pricing it carries: Ethereum's precompile set for the fork, not op-revm's Karst set with MegaETH's KZG price, and the fork's static opcode prices.
- SALT pricing needs nothing: without a SALT environment every bucket is minimal.
- The runtime limits are MegaETH's, so equivalence mode installs `EvmTxRuntimeLimits::no_limits()` itself rather than rely on the engine's default.

What stays Satin's is the machinery, and with it two rules no configuration can change: the instruction table's Amsterdam opcodes, and the rules revm gates on the spec id, which is Osaka's.
Both are in the registry.

## How a test is judged

As the revm fork's reference runner judges it, and more strictly where that runner is lenient:

- the post-state root and the logs hash for a transaction that executes;
- for one the fixture expects to be rejected, the exception it names, not any error, and the pre-state left as it was (`src/exceptions.rs` maps each validation error to the names that describe it);
- an expected output must be produced, not merely not contradicted;
- a fixture value that does not fit its width is a failure, not a value clamped to fit;
- a test name that appears twice in a file fails the file, rather than one test replacing the other.

Base fees go to Optimism's base-fee vault on Satin, where Ethereum burns them.
The runner takes the vault back out of the post-state only when the fee routing made it: the pre-state has no vault, and the vault holds exactly the base fee of the gas the transaction used.

The runner skips what the reference runner skips (`src/skips.rs`), for the same reasons: the EIP-7610 collision fixtures with storage, which revm cannot see, and a transaction that cannot be built when the fixture expects it to be invalid — here only when the fixture names the reason it cannot be, an invalid signature or a blob or EIP-7702 transaction without a recipient.
So the two runners execute the same population, and the executed and skipped counts equal the reference runner's.

## Running it

```bash
# The fixture releases the revm fork's `scripts/run-tests.sh` names at the pinned tag.
cargo run --release -p state-test -- --fork Osaka <main>/state_tests
cargo run --release -p state-test -- --fork Amsterdam <devnet>/state_tests
cargo run --release -p state-test -- --mode satin --fork Osaka <main>/state_tests
```

`--expect-executed`, `--expect-skipped` and `--expect-deviations` turn a full run into the pinned gate CI runs (`.github/workflows/exec-spec-satin.yml`).
`--json-outcome` prints one JSON line per test, a failure's produced hashes included, and `--trace` runs each test under an EIP-3155 tracer.

## Blockchain tests

`state-test btest` imports the execution-spec blockchain tests through Satin's block executor (`src/blockchain/`): the tests of the main release's `blockchain_tests` filled for Osaka, the release and the network the state-test gate's Osaka run uses.
The devnet release's blockchain tests are not run, as the revm fork's own runner does not run them.
A blockchain test is a chain: a pre-state, a genesis block, and blocks a node imports one after the other, several transactions to a block and several blocks to a test, some of which it must refuse.

### How a block is imported

Every block is decoded from its RLP, as a node receives it, and runs through `MegaBlockExecutor` on `MegaEvm` in equivalence mode's configuration — the neutral Osaka configuration, the fork's precompile set and static opcode prices — over a copy of the state the chain holds.
The executor makes the EIP-2935 and EIP-4788 pre-block system calls and deploys Satin's system contracts before the transactions, as it does for every block; `BLOCKHASH` reads the hashes of the chain's own blocks.
The executor requires the Satin fork's parameters, which the runner supplies with values that bind nothing on an Osaka fixture (`blockchain::chain_spec`): the loosest protocol limits a chain may carry, every limit unlimited and gas detention's caps far above the compute a transaction can spend under EIP-7825's cap, and placeholder `SequencerRegistry` roles that promote no fixture's transaction.

### How a block is judged

- A block the fixture expects to be valid must be accepted, and must produce the gas used, logs bloom, receipts root and state root its header carries.
- A block the fixture expects to be invalid for a `TransactionException` must be refused for one of the exceptions it names (`src/exceptions.rs`), not for any error, and leaves the chain at the previous block.
- Once every block is imported, the chain must end at the fixture's last block, holding the fixture's post-state.

A receipt of an OP transaction that is not a deposit encodes as an Ethereum receipt, so the receipts root is Ethereum's.

A Satin block adds accounts to the state an Ethereum block leaves, and the runner takes them out of the post-state before computing its root (`blockchain::SATIN_ACCOUNTS`):

- the seven contracts the executor deploys before every block: the six `MegaETH` system contracts and the EIP-7997 factory, each taken out when it holds exactly what the deploy writes;
- OP's base-fee vault, which holds the base fee Ethereum burns, taken out when it holds exactly the base fees of the blocks the chain accepted;
- OP's L1-fee and operator-fee vaults, credited nothing under the neutral configuration, taken out when they hold nothing.

An account on the list is taken out only when the fixture's pre-state does not hold it.
Any other account that differs, and an account on the list that holds anything else, moves the root, and the block fails.

### Skips

A test is skipped only for a class decided from its content before anything runs, never from how it fares, and each class is counted apart; the printed summary and the JSON summary (`skip_reasons`) give each class's reason beside its count:

- `withdrawals`: a block carries withdrawals, which an OP chain does not process;
- `blob-transactions`: a block carries a blob transaction, or expects an exception only a blob transaction raises; an OP chain has no blob transactions;
- `requests`: the test is an EIP-7002, EIP-7251 or EIP-7685 request test, or a block expects an exception about requests; an OP chain makes no requests and no post-block system call;
- `header-or-body`: a block expects only `BlockException`s for its header or body — its gas limit, base fee, blob gas fields, size, encoding, withdrawals root or hash — a consensus check a node makes before execution; a `BlockException` outside the list is compared, so the executor must refuse the block;
- `undecodable-invalid-transaction`: a block the fixture expects to be invalid carries a transaction an OP block cannot encode — an EIP-7702 transaction without a recipient, or a fee wider than 128 bits — the transactions the state-test gate skips as unbuildable;
- `create-collision-with-storage`: the state-test gate's EIP-7610 files (`src/skips.rs`), whose collisions with storage revm cannot see.

The EIP-2935 and EIP-4788 tests are not skipped: their system calls are made and compared like everything else.

### The gate

A failure is explained only by a deviation the registry already has, which lists the test, the block it fails at and the gas used, receipts root and state root Satin produces there; any other failure fails the gate.
`--expect-executed`, `--expect-skipped REASON=N` (every class) and `--expect-deviations` pin a full run, as `.github/workflows/exec-spec-satin.yml` does:

```bash
cargo run --release -p state-test -- btest <main>/blockchain_tests --expect-executed <N> \
  --expect-skipped withdrawals=<N> ... --expect-deviations
```

### The legacy state tests

The legacy `GeneralStateTests` are covered through the release's `static/state_tests`: the same tests re-filled for Osaka, which the state-test gate runs as state tests and this gate runs as blockchain tests.
The `ethereum/legacytests` repository holds no Osaka entries, and Satin's neutral configuration exists for Osaka and Amsterdam only.

## Replaying from a witness

`witness::check_replay` executes an entry on a database and environments that record every read, then replays it twice on a strict database and environments that serve exactly a witness and refuse everything else: once on the record of every database read, once on the witness a node builds from the transaction's returned state and the engine's exports, with the oracle reads the transaction recorded.
Each replay is held to the first run: the result, the state, the gas by ledger, the usage, the stop, the state changes, and the buckets and block hashes the engine exported.
The replay on the channel witness is the check a stateless validator's witness must pass, run on Ethereum's fixtures; the database-level replay shows the transaction reads nothing outside its database and environments.
An entry the engine rejects replays on the record of every database read alone: it has no returned state to build the channel witness from, and is in no block.
`check_replay` says which witnesses an entry replayed on (`Replayed`), and the fixture test counts the two apart: the entries executed and replayed on both witnesses, and the entries rejected and replayed on the database record alone.
A fixture written into the test always runs, an executed one and a rejected one; the test over the execution-spec fixtures is always ignored by default and runs only with `-- --ignored` (or `--include-ignored`) and `MEGA_STATE_TEST_FIXTURES` set, as `.github/workflows/exec-spec-satin.yml` runs it for every file of both releases in both modes:

```bash
MEGA_STATE_TEST_FIXTURES=$PWD/<main>/state_tests cargo test -p mega-state-test --release --test witness -- --ignored
```

`MEGA_STATE_TEST_SAMPLE` is how many fixture files to take, spread over the tree (300 unless set, 0 for all), `MEGA_STATE_TEST_FORK` the fork (`Osaka` unless set) and `MEGA_STATE_TEST_MODE` the mode (`satin` unless set).

## Deviations

A deviation lists the exact entries it explains: the fixture file (relative to the release's `state_tests` directory), the test, the data, gas and value indices, and the hashes Satin produces — the state root, or for a logs mismatch the logs hash and the state root.
A failure is the deviation's only when its entry is listed and it produced those hashes; any other failure is unattributed, in whatever file it is.
`--expect-deviations` requires every listed entry to fail exactly as listed, so an entry that starts to pass, fails another way or no longer runs fails the gate; the count a deviation explains is derived from its list.

A deviation added or moved updates `src/deviations.rs`, its entries taken from a `--json-outcome` run, and regenerates [`DEVIATIONS.md`](DEVIATIONS.md) with `UPDATE_DEVIATIONS=1 cargo test -p mega-state-test --lib deviations`.
