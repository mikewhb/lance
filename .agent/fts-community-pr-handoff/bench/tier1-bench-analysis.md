# Tier-1 wand port SBG analysis

Date: 2026-09-13  
Protocol: A1 TOP_10, 943 queries, `taskset -c 32`, WARMUP 20s, NUM_ITER 10, 3 reps  
Binaries: `target-cpu=native`, thin LTO, `LANCE_HACK_*` / `LANCE_DIAG_*` unset

## Fair A/B (same build flags, same core)

| Build | Git | AVERAGE µs | vs base | IU shape (40q) |
|-------|-----|----------:|--------:|---------------:|
| **base** | `f5ebb59a0` | **2,413** | — | 4,675 avg* |
| **tier1 full** | `b7dcb4207` (f63+b570+8f3a) | **2,444** | **+1.3% FAIL** | **+8.9% FAIL** |
| Lucene (community ref) | — | 1,480 | 1.65× | — |
| Tantivy (community ref) | — | 1,889 | 1.29× | — |
| Analysis target | `3d1189834` | 1,775 | 1.38× gap | 2,956 avg |

\*IU-only quick run (40 queries, 1 rep): base-fair **4,592** µs avg.

Artifacts:
- Base: `.agent/fts-community-pr-handoff/bench/tier1-base-fair-r{1,2,3}/`
- Cand: `.agent/fts-community-pr-handoff/bench/tier1-cand-r{1,2,3}/`
- 943/943 row counts: **identical**

## Per-shape (fair median, tier1 vs base)

| shape | delta |
|-------|------:|
| union | +1.5% |
| phrase | +0.6% |
| intersection | +0.5% |
| **intersection_union** | **+8.9%** |
| all | +1.3% |
| two-phase-critic | −3.6% |

## Bisect (IU-only quick, 40 queries)

| Variant | IU avg µs | vs base-fair |
|---------|----------:|-------------:|
| base-fair (`f5ebb59a0`) | 4,592 | — |
| **f63 only** | 4,860 | **+5.8%** |
| f63 + 8f3a (no b570) | 4,794 | +4.4% |
| f63 + b570 + 8f3a (full) | 4,924 | +7.2% |
| b570 only (on old seek) | 7,832 | +70% (broken combo) |

### Conclusions

1. **Blind port of analysis Tier-1 fails community ±5pp gate** — net slower, driven by IU.
2. **`f63d18dcd` (in-place Or seek) regresses IU ~+6%** on this stack despite analysis −133µs AVERAGE claim on monolithic tree @ older stack.
3. **`b5703fcb7` requires `f63` seek shape** — b570 alone on community old seek → catastrophic IU (+70%).
4. **`8f3a4f99c` does not rescue IU** when stacked with f63; slight help vs f63-only (−1.4pp) but still net regression vs base.
5. **Still far from analysis 1,775 µs** even at base (**+638 µs**, 1.36×) — Tier-1 was only ~250–350 µs expected; actual outcome is **negative**.

## Top IU regressions (tier1 vs stale termleaf cand, r2)

| Query | delta |
|-------|------:|
| `+electric grid stability` | +33% |
| `wildfire +risk maps` | +32% |
| `+housing market trends` | +27% |
| `customer +service phone number` | +13% |

## Action taken

- **`wand.rs` reverted to `f5ebb59a0`** — Tier-1 port **not landed**.
- **`wand_iu_tight.rs` conservative UB sums kept** (PR-A blocker fix, orthogonal).

## Recommended next steps

1. **Do not merge Tier-1 as-is** — re-port `f63` with IU/TermLeaf/ReqOpt interaction testing per shape before full 943.
2. **Profile IU witnesses** (`customer +service phone number`, `+electric grid stability`) with `probe_wiki` / perf — compare `WandCursor::advance` call counts base vs f63 seek.
3. **Compare Lucene** `WANDScorer.advance` + block-max window reuse vs community split modules (`wand_lead_stream`, TermLeaf, ReqOpt window collect) — analysis gains may assume monolithic `wand.rs` without community Phase-3 collect path.
4. **Remaining gap to 1,775 µs** is not Tier-1 alone: df path tax, optional `41e73`/`4cf28`, and analysis-only gates still account for hundreds of µs (see `execution-plan.md` §2.1).
