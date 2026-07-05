# ADR-0015: EVM `block.timestamp` units and single-source gas accounting

Status: Accepted
Date: 2026-07-05

## Context

Two empirically-verified defects were found in the EVM execution path
(`revm` integration) on the live testnet. Both live in the shared executor/EVM
path (`Chain::execute_at` → `Executor` → `EvmExecutor`) used identically by
block production and block import, so both are **consensus-affecting** (they
change the state root). The fix introduces no node-local input, so it remains
deterministic across nodes.

### Bug A — `block.timestamp` was in milliseconds

The block header timestamp is `now_ms()` (milliseconds), which is correct for
the consensus slot clock (`slot_of_timestamp = ts / BLOCK_INTERVAL_MS`). But
that raw millisecond value flowed unmodified into revm's block environment, so
Solidity `block.timestamp` was 1000× too large.

Verified: `eth_getBlockByNumber` reported `timestamp = 1783237135001` (ms), and
every DeFi deadline guard — Uniswap router `ensure`, EIP-2612 `permit`,
timelocks, vesting, TWAP — of the form `require(deadline >= block.timestamp)`
reverted, because a realistic Unix-seconds deadline is ~1000× smaller than the
millisecond `block.timestamp`.

### Bug B — contract transactions double-charged gas (~2×)

The `Executor` implements the intended gas model: prepay `gas_limit ×
gas_price`, refund unused, and pay the remainder to the block producer. This
was written assuming revm does **no** gas accounting of its own.

In fact revm 8.0.0 also charges gas: `deduct_caller` subtracts
`gas_limit × effective_gas_price` from the caller, `reimburse_caller` refunds
the unused portion, `reward_beneficiary` credits the coinbase, and
`apply_state_changes` writes revm's post-gas caller balance back to our state.
Net effect: a contract create/call charged the sender
`2 × gas_used × gas_price`.

Verified: a 505993-gas deploy drained exactly 2× `gasUsed × 1 Gwei` from the
deployer (ratio 2.00). Second-order effect: an account funded to exactly the
Ethereum requirement (`gas + value`) failed **inside** revm with
insufficient-funds (the executor had already deducted once), producing a failed
receipt that consumed the full gas limit — so contract interactions required 2×
the gas balance.

## Decision

### Bug A

Convert the header timestamp from milliseconds to Unix seconds at the single
point where it enters revm: `EvmExecutor::create_evm`, where
`evm.block_mut().timestamp` is set (`self.block_timestamp / 1000`). This one
change covers both the execution path (`execute_at`, used by block
production+import) and the `eth_call` / `eth_estimateGas` simulate path, since
both build their revm block env through `create_evm`.

Explicitly **unchanged**:
- consensus slot math (`slot_of_timestamp = ts / BLOCK_INTERVAL_MS`),
- the header `timestamp` field itself,
- the undelegation/unstake maturity clocks (`executor.rs` `unlock_at =
  block_timestamp / 1000 + …` and `chain.rs` `process_mature_undelegations(…,
  timestamp_ms / 1000)`), which already divide by 1000 themselves.

A grep confirms `self.block_timestamp` has exactly one consumer that expects
milliseconds-as-is (the revm block env, now divided) plus the maturity clocks
that do their own `/1000`.

The RPC output was also corrected (non-consensus, but a tooling/explorer
convention): `eth_getBlock*` (`qfc-rpc/src/types.rs`) and the `newHeads`
subscription (`qfc-rpc/src/server.rs`) now emit `timestamp / 1000` so the JSON
reports Unix seconds, matching both the Ethereum convention and the in-EVM
value. (QFC-specific `qfc_*` miner-event timestamps are left in ms — they are
not part of the Ethereum tooling contract.)

### Bug B

Make revm **gas-neutral** so the `Executor` remains the single source of gas
accounting. In `EvmExecutor` (`create` / `call` / `static_call` builders and
the block env in `create_evm`):

- set the revm transaction `gas_price = 0`, and
- set the block `basefee = 0`.

With revm 8.0.0 this makes revm charge no gas while still performing `value`
transfers:

- `deduct_caller_inner`: `gas_cost = gas_limit × effective_gas_price = 0` → no
  gas deducted from the caller.
- `Env::validate_tx_against_state`: `balance_check = gas_limit × gas_price +
  value = value` → the caller needs only `value`, fixing the second-order
  insufficient-funds failure.
- `Env::validate_tx` base-fee check: `effective_gas_price (0) < basefee (0)` is
  false → passes, so no base-fee rejection.
- `reward_beneficiary`: `coinbase_gas_price = effective_gas_price − basefee =
  0` → the coinbase receives nothing from revm.
- `value` transfers for CREATE-with-value / CALL-with-value are handled by
  revm's inner call/create logic, independent of gas price, and are preserved.

The block producer is still paid `gas_used × gas_price` by the `Executor`
(prepay → refund-unused → pay-producer), which is unchanged.

#### Why `gas_price = 0` + `basefee = 0` instead of `disable_base_fee`

revm's `CfgEnv::disable_base_fee` (and `disable_balance_check`) fields are
`#[cfg(feature = "optional_no_base_fee")]` / `optional_balance_check` gated.
The workspace depends on revm with `default-features = false, features =
["std", "secp256k1"]`, so those fields **do not exist** without adding the
`optional_*` (or `dev`) feature to the whole workspace. Setting `basefee = 0`
makes the base-fee check pass by arithmetic (`0 < 0` is false) and setting
`gas_price = 0` zeroes both the deduction and the balance requirement, achieving
the same result with **no Cargo feature change** and no reliance on cfg-gated
API. This is a deliberate, minimal deviation from the originally-suggested
`disable_base_fee = true` approach.

## Consequences / opcode side-effects

- `GASPRICE` (`tx.gasprice`) now reads `0` inside contracts. Previously it read
  a hardcoded `1 Gwei`, which was equally non-real. Acceptable.
- `BASEFEE` now reads `0`. Previously `1 Gwei`. Acceptable.
- Both fixes change the state root and are consensus-affecting; they must be
  deployed to all nodes together (and, per the testnet-reset plan, against a
  fresh genesis) since historical blocks were produced under the old rules.

## Tests

- `executor::tests::test_contract_create_charges_gas_once` — a deploy charges
  the sender exactly `gas_used × gas_price` (1×) and the producer receives
  exactly `gas_used × gas_price`.
- `executor::tests::test_contract_create_does_not_need_double_gas_balance` — an
  account funded to exactly the Ethereum gas requirement can deploy (no 2×
  balance needed).
- `evm::tests::test_block_timestamp_is_seconds_not_millis` — a contract that
  stores `TIMESTAMP` sees `input_ms / 1000` (and not the raw ms).
- `evm::tests::test_deadline_guard_succeeds_with_seconds_timestamp` — a
  `require(block.timestamp <= 2_000_000_000)`-style call succeeds with the
  seconds timestamp; pre-fix (ms) it would have reverted.
