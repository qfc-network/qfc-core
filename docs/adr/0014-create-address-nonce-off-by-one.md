# ADR 0014: CREATE contract-address off-by-one (sender-nonce double-increment)

- Status: Accepted
- Date: 2026-07-05
- Related: `crates/qfc-executor/src/executor.rs`,
  `crates/qfc-executor/src/evm.rs`, `crates/qfc-chain/src/chain.rs`,
  `crates/qfc-rpc/src/types.rs`

## Context

A confirmed, deterministic bug: contract `CREATE` transactions deployed the
contract to the Ethereum address `f(sender, tx.nonce + 1)` instead of the
standard `f(sender, tx.nonce)`.

Proof: a deploy with `tx.nonce = 43` produced a receipt whose
`contractAddress` equalled `ethers.getCreateAddress({ from, nonce: 44 })`,
not `nonce: 43`. The offset was uniform and deterministic (not reorg- or
load-related). It broke CREATE-address prediction for every standard EVM
tool (ethers, hardhat, foundry, wallets) and every counterfactual/factory
pattern.

### Mechanism (traced and verified empirically)

`Executor::execute` ran, in order:

1. deduct gas prepayment,
2. **`state.increment_nonce(&sender)`** — an unconditional manual bump,
3. dispatch to `execute_contract_create` → `EvmExecutor::create`.

Inside revm, `evm.tx.nonce` is left at its default of `None`, so revm skips
nonce validation, reads the caller's **current** account nonce from state to
derive the CREATE address, and bumps the caller nonce itself. Because step 2
had already advanced the sender nonce from `N` to `N+1`, revm's `basic_ref`
read `N+1` and computed `f(sender, N+1)` — the off-by-one.

A throwaway probe (sender starting at nonce 5, `tx.nonce = 5`) confirmed the
full picture, which was **worse** than the report assumed:

- `contractAddress == f(sender, 6)` (off-by-one), and
- final sender nonce `= 7` — a **+2 double-increment**, not +1.

So *both* the manual pre-increment (5→6) **and** revm's own increment,
persisted by `commit_state_changes`/`apply_state_changes` via
`set_nonce(caller, 7)` on the success path, were landing. The report's
premise that the CREATE nonce delta was already +1 was incorrect for
EVM-executed tx types; only `Transfer` (a non-EVM path that never runs revm)
was +1.

`apply_state_changes` (evm.rs) only runs on the revm `Success` arm; a
reverted/halted EVM tx applies **no** state, and a `ContractCall` to a
code-less account does a bare value transfer without invoking revm at all.
So revm's nonce bump cannot be relied on as the *sole* nonce advance.

## Decision

For EVM-executed tx types (`ContractCreate`, `ContractCall`), do **not**
pre-increment the sender nonce; let revm read the pre-increment nonce
`N = tx.nonce` (yielding the Ethereum-standard `f(sender, N)` address) and
advance the caller nonce itself. After execution, finalize the sender nonce
to exactly `tx.nonce + 1`:

- On success, revm already wrote `N+1` — the finalize is a no-op.
- On revert/halt, or a `ContractCall` to a code-less account, revm wrote
  nothing — the finalize is the single authoritative `+1`.
- On an EVM-level `Err` (revm `transact()` failed), the existing
  revert-then-`increment_nonce` path restores `N` and bumps to `N+1`.

Only the sender EOA is finalized; contract/factory nonces that revm bumps
during execution are left untouched. Non-EVM tx types (`Transfer`, `Stake`,
`Unstake`, `Delegate`, validator ops, `InferenceTask`, …) keep the manual
pre-increment — their behaviour (+1) is unchanged.

The fix lives entirely in the shared `Executor::execute`, which both the
produce path and the import path reach through `Chain::execute_at`, so the
change is consensus-neutral and byte-identical on every node.

## Consequences

Invariants, proven by tests:

1. **CREATE address** == `f(sender, tx.nonce)` (matches every standard EVM
   tool), verified against both revm's `Address::create` (executor unit
   tests) and an independent RLP+keccak computation (chain convergence
   test).
2. **Sender nonce** advances by exactly 1 after any tx — `Transfer`,
   `Stake`, `ContractCreate`, `ContractCall` (to a contract *and* to an
   EOA). The previous +2 double-bump on EVM txs is removed.
3. **Sequential CREATEs** from one sender (`N`, `N+1`, …) land at
   `f(sender, N)`, `f(sender, N+1)`, … — the standard Ethereum sequence with
   no gaps or overlaps.
4. **ContractCall** and value transfers are unaffected in effect.

Tests: `crates/qfc-executor/src/executor.rs` unit tests
(`test_create_address_uses_pre_increment_nonce`,
`test_sequential_creates_follow_ethereum_sequence`,
`test_transfer_advances_nonce_by_one`, `test_stake_advances_nonce_by_one`,
`test_contract_call_to_deployed_contract_advances_nonce_by_one`,
`test_contract_call_to_eoa_advances_nonce_by_one`) and a chain-level
end-to-end test through `execute_at` + block import on two independent chains
(`crates/qfc-chain/tests/convergence.rs::contract_create_address_uses_pre_increment_nonce`).

### Note on the observed +1 discrepancy

Because the pre-existing behaviour was a +2 nonce bump for CREATE, wallets
tracking nonce via `eth_getTransactionCount` would have observed the sender
nonce jump by 2 per deploy. This fix also corrects that to +1, aligning the
account nonce with Ethereum semantics.
