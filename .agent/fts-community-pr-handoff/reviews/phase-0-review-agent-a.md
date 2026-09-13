# Phase 0 review — Agent A

Date: 2026-09-13  
Reviewer: independent agent (not implementer)  
Inputs:
- `.agent/fts-community-pr-handoff/execution-plan.md`
- `docs/fts-community-pr-triage.md` (skim)
- `docs/fts-compound-term-leaf.md` (skim)
- `.agent/fts-iu-r3-community-port/contract.md` (skim)
- Repo verification @ `bench/community-fts-stack` HEAD `f5ebb59a0`

## Verdict

APPROVE WITH CHANGES

## P0 (block merge / next phase)

*(none)*

## P1 (should fix before PR / before Status → APPROVED)

- **execution-plan.md:143,287** — Phase 0 review artifact path is singular (`reviews/phase-0-review.md`) while §6 requires **two independent agents**. Align with dual-file naming (`phase-0-review-agent-a.md` / `phase-0-review-agent-b.md`) or document merge rule into one file.

- **execution-plan.md:57 vs wand.rs:6229–6249** — Tier-1 gap evidence cites `wand.rs:6229` for `mem::take` in `seek`; line 6229 is `fn seek`, actual `std::mem::take` on head/tail is at **6243** and **6249**. Update line refs so Phase 1 port diffs land in the right hunk.

- **execution-plan.md:9 + contract.md:4–5** — Related link points at `fts-iu-r3-community-port/contract.md`, which still pins branch @ **`e3a8aecd0`** (Phase 3 reverted) and lists Slice 0 baseline there. Current stack HEAD is **`f5ebb59a0`** (Slices 1–3 + TermLeaf upper committed). Add an explicit supersession note in the execution plan (or refresh contract) so Phase 1 agents do not bench against `e3a8aecd0`.

- **execution-plan.md:224 vs docs/fts-compound-term-leaf.md:152** — Bench pin diverges: plan §5 mandates `taskset -c 32` (or doc pin,全程一致); TermLeaf contract still says `taskset -c 0`. Name the canonical pin once in the execution plan and state which older doc numbers are historical-only.

- **execution-plan.md:73–127 vs docs/fts-community-pr-triage.md:75–84** — Triage proposes **5 PRs** (iu_tight and TermLeaf separate; lead-stream as PR5). Plan repackages into **PR-A/B (+ optional PR-C)** and excludes lead-stream from PR-A/B (§1 L25). Add a short “supersedes triage §3 boxing” paragraph so reviewers do not open conflicting PR shapes.

- **execution-plan.md:39** — Gap table cites `.agent/fts-community-pr-handoff/summarize_shapes.py` @ `termleaf-upper-bench` cand; handoff dir has the script but **no** `bench/` JSON subtree yet. Fine for Phase 0 planning; note that Phase 3–4 gates require fresh JSON under handoff or linked external roots before merge claims.

## Evidence checked

- [x] git HEAD / branch — `f5ebb59a0` on `bench/community-fts-stack` (matches plan L6)
- [x] Tier-1 “not yet ported” code markers (plan §2.3):
  - `tail_scratch` — **absent** in `wand.rs` (confirms `f63d18dcd` not landed)
  - `seek` clears window — `wand.rs:6230` `self.up_to = None;` (confirms `b5703fcb7` not landed)
  - `essential_split_is_stale` — **absent** in `wand_maxscore.rs` (confirms `8f3a4f99c` not landed)
  - bulk `enabled_for` — `wand.rs:299–301` `Auto => matches!(num_clauses, 2 | 3)` (confirms `41e73d1ad` not landed)
  - `seek` realloc path — `wand.rs:6243–6249` `std::mem::take` on head/tail (confirms `f63d18dcd` scratch reuse not landed)
- [x] Stack commits listed in plan §2.2 — present on branch (`37938f943`, `7df4df621`, `a4e6469d1`, `f5ebb59a0`, `a489e15a7`, `fd1d4b829` ancestor)
- [x] TermLeaf semantics vs `docs/fts-compound-term-leaf.md` — `TermLeafScorer::set_min_competitive_score` no-op @ `wand.rs:6156–6162`; comment @ `wand.rs:5980` matches “never uses competitive floor to skip blocks or drop documents”
- [x] PR-A blocker still open — `wand_iu_tight.rs:86–101` naive `f32` sum (`should_ub_sum`, `must_score + should_ub_sum`); no `next_up` widening; aligns with `docs/fts-perf-commits-review.md` §3 and plan §3 PR-A blocker
- [x] PR-B Slice 2 + upper binding — consistent with contract Slice 2 measured IU **+4.0%** vs Slice 1 (`contract.md:206–207`) and plan §3 PR-B hard rule (`7df4df621` + `f5ebb59a0`)
- [ ] git diff stat vs 2000-line soft limit — N/A Phase 0 (no PR diff yet)
- [ ] 943/943 dump — N/A Phase 0
- [ ] summarize_shapes all shapes within ±5pp — N/A Phase 0
- [ ] Bench numbers from this phase rerun — plan correctly marks gap table as session/cand evidence (§2.1 L39); Phase 1 must re-bench

## Notes

**Strengths**

- Two-track ordering (analysis catchup → PR integration) is clear and matches maintainer constraints (no DF floor, no 85k gate, no addends slab).
- Phase 1 port order `f63 → b570 → 8f3a` with per-commit gates and revert discipline is actionable; cherry-pick prohibition vs split modules (`wand_lead_stream.rs` / `wand_maxscore.rs`) matches `contract.md` tree-divergence analysis.
- PR-B bundling of Slice 2 + TermLeaf upper is well justified by measured IU regression when upper is omitted.
- Exclusions (lead-stream chain stays on bench branch, not PR-A/B) are explicit; only needs cross-link to triage to avoid confusion.

**Consistency skim**

| Topic | execution-plan | companion doc | Match |
|-------|----------------|---------------|-------|
| TermLeaf no floor skip | §6 L245 | compound-term-leaf.md §做/不做 | ✅ |
| Slice 2 alone regresses IU | PR-B §3 | contract.md Slice 2 +4.0% | ✅ |
| iu_tight f32 defect | PR-A blocker | fts-perf-commits-review.md §3 | ✅ |
| Current HEAD | f5ebb59a0 | contract.md e3a8aecd0 | ❌ stale contract |
| PR boxing | PR-A/B (+C) | triage 5 PRs | ⚠️ intentional; needs callout |

**Recommendation:** Absorb P1 edits into `execution-plan.md`, set Status → **APPROVED**, then proceed to Phase 1 `analysis-catchup-contract.md` authored against **current** HEAD `f5ebb59a0`, not `e3a8aecd0`.
