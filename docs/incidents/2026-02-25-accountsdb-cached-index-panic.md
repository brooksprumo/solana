# Incident Analysis: AccountsDb "Bad index entry detected" panic

Date: 2026-02-25
Target build commit: `51e66219ea591dd193998a3d37dd82cfbf234cd8` (master at time of report)

## 1) Reported panic

Thread:
- `solScHandleV02` (unified scheduler handler thread)

Panic site:
- `accounts-db/src/accounts_db.rs:3874`
- assertion: `assert!(!new_storage_location.is_cached(), "{message}")`

Backtrace shape:
- `AccountsDb::retry_to_get_account_accessor`
- `AccountsDb::do_load_with_populate_read_cache`
- SVM account loader
- `Bank::do_load_execute_and_commit_transactions_with_pre_commit_callback`
- `ledger::blockstore_processor::execute_batch`
- `DefaultTaskHandler::handle` (unified scheduler pool)

Key panic payload facts:
- Pubkey: `So11111111111111111111111111111111111111112`
- Slot: `402240429`
- Storage location in index: `Cached`
- In-slot list tail includes `(402240429, AccountInfo { store_id: 4294967295, offset_reduced: 2147483647 })`

Interpretation of those values:
- `store_id = 4294967295` and `offset_reduced = 2147483647` are the cached sentinels for `StorageLocation::Cached`.

## 2) What this panic means

`retry_to_get_account_accessor()` had this sequence:
1. Read index and got `(slot=402240429, StorageLocation::Cached)`.
2. Tried to load from cache and got `Cached(None)` (cache miss).
3. Re-read index and got the same cached location for same slot.
4. Panicked, because for replay/banking (`LoadHint::FixedMaxRoot`) this is treated as impossible.

The specific invariant being enforced:
- If cache entry is gone, index should already have been switched to AppendVec (flush ordering F2 -> F3 -> F4), or the slot should be removed from index during purge.

So this is not random data corruption. It is an ordering/lifetime race where a replay read path observed a stale cached pointer while cache data had already disappeared.

## 3) Why this is surprising in replay

The panic path had `LoadHint::FixedMaxRoot`, which assumes:
- the caller (replay/banking) holds a bank and its ancestors alive during load, and
- those ancestors are not purged out from under the load.

That is exactly why this code chooses to panic instead of soft-failing in this case.

## 4) Most likely root cause

Most likely root cause: duplicate-slot/unrooted-slot purge can run while scheduler work for removed banks is still in flight, and cache/index entries for those slots are purged before all outstanding replay reads for those banks have drained.

The critical replay purge path:
- `core/src/replay_stage.rs::purge_unconfirmed_slot()`
- It does:
  1. `dump_slots(...)` from `BankForks` (collects `removed_banks` as `Vec<BankWithScheduler>`)
  2. `root_bank.remove_unrooted_slots(&slots_to_purge)` (purges AccountsDb cache/index/storage)
  3. `drop(removed_banks)` (which eventually waits/drops scheduler through `BankWithScheduler` drop path)

This ordering permits a window where:
- removed banks still have active scheduler handlers reading accounts,
- but AccountsDb has already removed slot-cache/index entries for those same slots.

That exactly matches the observed panic condition:
- replay handler thread (`solScHandleV02`) still loading account state,
- index entry still cached sentinel for slot being purged,
- cache miss at load time,
- re-read still cached -> panic.

## 5) Why this lines up with the panic payload

The panic entry showed many consecutive slots with cached sentinels up to `402240429`, then cache miss at read.
This pattern is consistent with active replay writes/loads in fresh slots and concurrent slot purge/cleanup, not with a single malformed index write.

The thread name in the panic (`solScHandleV02`) further supports that this happened inside unified scheduler execution, not RPC.

## 6) Bug location (functional)

Primary functional bug location:
- `core/src/replay_stage.rs`, in `purge_unconfirmed_slot()` sequencing around:
  - `dump_slots(...)`
  - `remove_unrooted_slots(...)`
  - `drop(removed_banks)`

Supporting invariant check (where panic is raised):
- `accounts-db/src/accounts_db.rs:3874` in `retry_to_get_account_accessor()`

The AccountsDb panic is the detector, not the root logic bug.

## 7) Proposed fix (no code yet)

Goal:
- Ensure all scheduler execution for removed banks is drained before purging their AccountsDb slot state.

Proposed sequencing change in `ReplayStage::purge_unconfirmed_slot()`:
1. Keep `dump_slots(...)` first to stop new scheduling via `BankForks`.
2. Before calling `remove_unrooted_slots(...)`, iterate `removed_banks` and call `wait_for_completed_scheduler()` on each `BankWithScheduler`.
3. After waits complete, call `root_bank.remove_unrooted_slots(&slots_to_purge)`.
4. Then drop `removed_banks` as normal.

Why this works:
- Draining schedulers closes the race between replay account loads and AccountsDb purge for those slots.
- It directly restores the assumption behind `LoadHint::FixedMaxRoot` in the panic path.

Behavioral notes:
- `wait_for_completed_scheduler()` can return `None` if no scheduler is installed; treat as no-op.
- If it returns `Some((Err(...), timings))`, log and continue purge (these slots are being discarded anyway).

## 8) Optional hardening and diagnostics

Possible hardening steps after primary fix:
- Add a debug datapoint in purge path with:
  - number of removed banks waited,
  - total wait time,
  - count of scheduler errors while waiting.
- Add a targeted test that forces:
  - duplicate slot purge,
  - in-flight scheduler account loads,
  - assertion that no `Bad index entry detected` panic occurs.
- Consider converting this specific FixedMaxRoot panic to a richer fatal message that includes whether slot was under `remove_unrooted_slots` at time of failure.

## 9) Risk and tradeoff

Tradeoff:
- Purge latency may increase because replay thread waits for scheduler completion before slot purge.

Assessment:
- Correctness is higher priority here. This path is for duplicate/unconfirmed slot handling, not hot steady-state transaction execution.
- The added wait is bounded by scheduler session completion mechanics already used in replay completion paths.

## 10) Confidence

Confidence: high on race class and likely bug location; medium-high on exact triggering sequence in your specific run (no full runtime trace was available beyond panic log).

The proposed fix aligns with existing scheduler APIs and the observed panic invariants, and is the lowest-risk correctness-first change.

