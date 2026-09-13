# Phase 0 review — merged (Agent A + Agent B)

Date: 2026-09-13  
Inputs: `execution-plan.md`, companion docs, repo @ `f5ebb59a0`

## Verdict

**APPROVE WITH CHANGES** — P1 items absorbed into `execution-plan.md`; Status → **APPROVED**.

## P0

*(none)*

## P1 resolution

| Item | Resolution |
|------|------------|
| Dual review naming | Keep `phase-N-review-agent-a/b.md` + merged `phase-N-review.md` |
| `f63` line cite | Fixed: `mem::take` @ seek **6243/6249** |
| `8f3a4f99c` target | Fixed: **`wand.rs`** maxscore inner loop (not `wand_maxscore.rs` alone) |
| Stale `contract.md` HEAD | Supersession note: stack @ **`f5ebb59a0`**, not `e3a8aecd0` |
| CPU pin | Canonical: **`taskset -c 32`** for Phase 1+ bench |
| Triage 5-PR boxing | PR-A/B supersedes `docs/fts-community-pr-triage.md` §3; wand micro-opt → **PR-C** |
| PR-A line budget | Rust-only ~**1,269** lines (`fd1d4b829` + `a489e15a7`) |
| `iu_tight` f32 blocker | Phase 3.0: conservative `outward_f32_upper_bound` sums |
| Bench gate language | Hard: any shape **>+5pp** FAIL; Phase 1 improvement gates separate |
| Phase 4.1 “Phase2” | Commit **`e36fd6489`**, not execution Phase 2 |

## Evidence

- [x] Tier-1 gap markers confirmed absent pre-port (`tail_scratch`, `up_to` clear on Or seek, `essential_split_is_stale`)
- [x] PR-B Slice 2 + upper binding documented
- [x] `iu_tight` f32 defect documented (`docs/fts-perf-commits-review.md` §3)
