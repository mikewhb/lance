# Analysis catchup contract (Phase 1)

Date: 2026-09-13  
Branch: `bench/community-fts-stack`  
Base before port: `f5ebb59a0`  
Analysis reference: `perf/fts-top10-analysis` @ `3d1189834`

## Scope

Port Tier-1 wand commits with SBG evidence, adapted to community split modules (`wand_lead_stream.rs`, `wand_maxscore.rs` unchanged; logic lands in `wand.rs`).

| Order | Upstream | Mechanism | Files |
|------:|----------|-----------|-------|
| 1.1 | `f63d18dcd` | `tail_scratch` + in-place Or `seek` | `wand.rs` |
| 1.2 | `b5703fcb7` | Keep `up_to` across Or `seek` | `wand.rs` |
| 1.3 | `8f3a4f99c` | `essential_split_is_stale` + window reopen | `wand.rs` `maxscore_search` |

**Excluded:** `41e73d1ad`, `4cf28da10` (optional Tier-2; not in this delivery).

## Correctness gates (each commit)

1. `cargo test -p lance-index` — all pass
2. `cargo clippy -p lance-index -- -D warnings`
3. 943/943 TOP_10 dump bit-identical vs previous step (when bench run)
4. SBG shape gate: no shape **>+5pp** regression vs previous step

## Tests added

- `wand_cursor_advance_matches_sequential_next_across_blocks` (`f63`)
- `essential_split_staleness_tracks_the_current_floor` ×6 (`8f3a`)
- `maxscore_reopens_the_window_once_the_floor_invalidates_the_split` — winners only; reopen counter covered by unit tests (community `seed_floor` routes 4-clause shape through single-essential)

## Rollback

Any gate FAIL → `git revert` the port commit; do not amend pushed commits.

## Delivery

Single squash commit on `bench/community-fts-stack`:

```
perf(fts): port analysis Tier-1 wand seek and maxscore window reopen

Ports f63d18dcd, b5703fcb7, 8f3a4f99c into community wand.rs.
```

## Bench

Command: `.agent/fts-community-pr-handoff/bench-addend-run.sh lance <target> .agent/fts-community-pr-handoff/bench phase1-tier1 3 32`  
Artifact: `.agent/fts-community-pr-handoff/bench/phase1-tier1/` + `summarize_shapes.py`
