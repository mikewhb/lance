# Phase 0 review — Agent B

Date: 2026-09-13  
Reviewer: independent agent (not implementer)  
Inputs:
- `.agent/fts-community-pr-handoff/execution-plan.md`
- `docs/fts-perf-commits-review.md` (§3 C-iu-tight / §5 recommendations)
- `.agent/fts-community-pr-handoff/reviews/README.md`
- Repo verification @ `bench/community-fts-stack` HEAD `f5ebb59a0`

## Verdict

APPROVE WITH CHANGES

## P0 (block merge / next phase)

*(none)*

## P1 (should fix before PR / before Status → APPROVED)

- **execution-plan.md:59 vs `git show 8f3a4f99c`** — §2.3 cites `wand_maxscore.rs 无 essential_split_is_stale` as the gap marker; analysis commit `8f3a4f99c` adds `essential_split_is_stale` entirely in **`wand.rs`** (+203/−4). Community tree has **no** `essential_split_is_stale` in either file (`rg` over `rust/lance-index/src/scalar/inverted/`). Gap is real; evidence column should name `wand.rs` (Phase 1.3 already does) and drop the misleading `wand_maxscore.rs` pointer so porters do not search the wrong module.

- **execution-plan.md:78–79 vs `git numstat`** — PR-A budget `~1,450 insert (590+860)` counts **total** commit stats including `docs/fts-compound-term-leaf.md` and production md. Rust-only for the same commits is **+1,269 / −44** (`fd1d4b829` +576/−23, `a489e15a7` +693/−21). Still under the 2,000 soft cap, but §3.2 / Phase 3.2 should mandate `git diff --numstat -- '*.rs'` (or equivalent) so the gate matches the stated “rust diff” rule.

- **execution-plan.md:80,194 vs `docs/fts-perf-commits-review.md:125–126`** — `next_up` widening is correctly a PR-A merge blocker (Phase 3.0), but the plan omits the **mandatory regression vector** from fts-perf Round 2 (`must_ub=2^-24`, `should_ubs=[1.0, 2^-24]`, `floor=1+2^-23` → empty-`required` early exit). Phase 3.0 should explicitly require that f32 case (or `score_sum_cannot_compete` parity) in the unit test, not generic “边界单测”. Until Phase 3.0, stack HEAD already carries `fd1d4b829` with naive sums at `wand_iu_tight.rs:86–101` — add a Phase 1 note that IU-shaped SBG gates are **perf-only** on this branch, not correctness proof.

- **execution-plan.md:135,143,155–158,199,213 vs `reviews/README.md:8–14`** — Dual-agent gates are required every phase (§4 L133, §6 L1), but artifact paths are **single-writer**: Phase 0/3/4 use one `phase-<N>-review.md`; Phase 1/2 use one contract file each. README L14 even says “Phase 0, use `phase-0-review.md` only.” Specify per-agent filenames (`phase-<N>-review-agent-{a,b}.md`) or a documented merge rule for **all** phases, not only Phase 0.

- **execution-plan.md:165–167 vs §5 L230** — Bench hard gate language is inconsistent. §5 normative FAIL is **one-sided** (`任一 shape >+5.0%`); Phase 1 port table says `all ±5pp` and 1.3 adds **improvement-required** gates (`union ≥−5%` or `mix ≥−50µs`). Clarify in §5: (a) regression ceiling +5pp for stability shapes, (b) separate improvement thresholds where a commit has a directional claim, (c) how `summarize_shapes.py` aggregates “shape”.

- **execution-plan.md:210** — “叠 Phase2 + Slice1–3” reads as **execution Phase 2** (branch creation). Intended meaning is commit `e36fd6489` (“Phase 2 same-block scan” in §2.2). Rename to “same-block scan commit” to avoid branch-phase ambiguity during Phase 4 agent review.

## Evidence checked

- [x] Phase ordering — two-track order (catchup → PR integration) and per-phase “doc → 2× agent → next phase” are stated (§4, §8); Phase 1 uniquely splits **contract** (1.1) vs **code** (1.4) dual review — Phase 3/4 lack a pre-work contract gate (only post-integration review at 3.5/4.4)
- [x] PR-A/B line budgets vs 2,000 soft — PR-A rust ~1,269 insert; PR-B incremental rust ~851 insert (`e36fd6489` 34 + `37938f943` 276 + `7df4df621` 422 + `a4e6469d1` 29 + `f5ebb59a0` 90); both under cap; plan totals slightly overstated by non-rust files
- [x] Bench ±5pp hard gate — defined §5 L230; PR guards §3 L92/L111 align on `>+5pp` regression block; Phase 1 asymmetric improvement gates need §5 cross-ref (see P1)
- [x] `docs/fts-perf-commits-review.md` iu_tight `next_up` — §3 defect confirmed live @ `wand_iu_tight.rs:86–101`; plan Phase 3.0 + PR-A blocker align with §5 rec #1; missing explicit反例 test citation (see P1)
- [x] Phase 1 port commits missing in community `wand.rs` @ `f5ebb59a0`:
  - `f63d18dcd` — no `tail_scratch` in `wand.rs` (`rg`); `seek` still `mem::take` head/tail @ `wand.rs:6243–6249` ✓
  - `b5703fcb7` — `seek` clears window `self.up_to = None` @ `wand.rs:6230` ✓
  - `8f3a4f99c` — no `essential_split_is_stale` anywhere in inverted module ✓ (lands in `wand.rs` on analysis tree, not `wand_maxscore.rs`)
  - `41e73d1ad` — `BulkAndMode::enabled_for` still `Auto => matches!(num_clauses, 2 | 3)` @ `wand.rs:299–301` ✓
  - `4cf28da10` — no maxscore stranded-advance path in FTS `wand.rs` ✓
- [ ] 943/943 dump — N/A Phase 0
- [ ] summarize_shapes ±5pp — N/A Phase 0
- [ ] git diff stat vs 2000 — N/A Phase 0 (methodology note in P1)

## Notes

Phase 1 missing-commit identification is **substantively correct**; the only material error is citing `wand_maxscore.rs` for `8f3a4f99c` while the analysis patch and port target are `wand.rs`. Tier-1 port order `f63 → b570 → 8f3a` matches analysis dependency (scratch reuse → preserve `up_to` → stale-split reopen).

PR-A/B commit lists match on-branch SHAs (§2.2). PR-B binding of `7df4df621` + `f5ebb59a0` is consistent with measured Slice-2-alone IU regression documented in `fts-iu-r3-community-port/contract.md`.

Recommend absorbing P1 into `execution-plan.md` + `reviews/README.md`, then Status → **APPROVED** and Phase 1 `analysis-catchup-contract.md` authored against HEAD `f5ebb59a0` (not contract.md’s stale `e3a8aecd0`).
