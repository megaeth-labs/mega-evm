# Changelog

## v1.7.2 (2026-09-30)

### Fixes

- evm: load L1 block info at block 0 on release-v1.7.2 ([#397](https://github.com/megaeth-labs/mega-evm/pull/397))

## v1.7.1 (2026-09-08)

### Features

- seal REX6 and open REX7 ([#360](https://github.com/megaeth-labs/mega-evm/pull/360))

### Fixes

- chain: schedule Rex6 on the canonical mainnet and testnet hardfork schedules ([#371](https://github.com/megaeth-labs/mega-evm/pull/371))

### Refactoring

- block: derive hardfork gating from single-source spec resolution ([#362](https://github.com/megaeth-labs/mega-evm/pull/362))

### Documentation

- publish the Rex6 activation timestamps ([#364](https://github.com/megaeth-labs/mega-evm/pull/364))
- specify compute gas accounting and pin it with a cross-spec test harness ([#358](https://github.com/megaeth-labs/mega-evm/pull/358))
- add audit-derived execution and dev-tool rules to REVIEW.md ([#356](https://github.com/megaeth-labs/mega-evm/pull/356))

### CI

- publish targets on release — crates.io and mega-evme to the registry ([#374](https://github.com/megaeth-labs/mega-evm/pull/374))
- adopt the shared release flow (candidate / settle / publish) ([#372](https://github.com/megaeth-labs/mega-evm/pull/372))
- let PR comments trigger a Claude review reconcile round ([#361](https://github.com/megaeth-labs/mega-evm/pull/361))
- point Claude actions at megaeth-labs/.github (`44116f6a10`)
- run pr-review under the CI app identity ([#357](https://github.com/megaeth-labs/mega-evm/pull/357))
