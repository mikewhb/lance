# Phase 1 code review — merged

Date: 2026-09-13  
Input: Tier-1 port diff + unit tests

## Verdict

**REJECT Tier-1 port** — fair SBG @ core 32: all **+1.3%**, IU **+8.9%** (>+5pp gate). **`wand.rs` reverted to `f5ebb59a0`.**

## Evidence

- [x] Fair A/B: `tier1-base-fair` vs `tier1-cand` — see `bench/tier1-bench-analysis.md`
- [x] 943/943 counts identical
- [x] Bisect: **f63** alone IU **+5.8%**; b570-only on old seek **+70%** (invalid combo)
- [ ] Analysis 1775µs target — **not approached** (base 2413µs, tier1 2444µs)
