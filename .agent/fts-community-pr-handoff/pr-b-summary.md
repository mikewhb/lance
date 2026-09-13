# PR-B summary — ReqOpt window collect + TermLeaf upper

Date: 2026-09-13  
Base: PR-A merge tip  
Branch: `bench/community-fts-stack-for-pr`

## Commits (on top of PR-A)

- `e36fd6489` — Phase 2 same-block scan
- `37938f943` — Slice 1 `collect_confirmed_window`
- `7df4df621` — Slice 2 ReqOpt window collect
- `a4e6469d1` — Slice 3 WandCursor window floor
- `f5ebb59a0` — TermLeaf conservative upper

## Hard rule

**`7df4df621` and `f5ebb59a0` ship together.** Slice 2 alone regresses IU ~**+4%**; with upper **−7.4%** vs Slice2+3 stack.

## Claims (re-bench vs PR-A baseline)

| Scenario | Expected |
|----------|----------|
| IU 40 | **−7.4%** vs PR-A |
| `+water` / `+climate` | ~**−20%** per-query |
| all 943 | **−1.5%** |
| other shapes | **±1.5%** |

## Guards

- 943/943 bit-identical
- No shape **>+5pp**

## Test plan

- [ ] SBG 3×3 vs PR-A build
- [ ] Verify TermLeaf upper before lazy score (`f5ebb59a0`)
