# PR integration contract (Phase 2–4)

Date: 2026-09-13  
Integration branch: `bench/community-fts-stack-for-pr`  
Rebase base: `origin/main` (maintainer may specify)

## Branch creation

```bash
git branch bench/community-fts-stack-for-pr <Phase-1-HEAD>
```

No experimental hacks on `-for-pr` after creation.

## PR-A

| Field | Value |
|-------|-------|
| Commits (squash narrative) | `fd1d4b829` + `a489e15a7` |
| Includes doc | `docs/fts-compound-term-leaf.md` |
| Rust budget | ~1,269 lines vs main (soft ≤2000) |
| Blocker fixed | `wand_iu_tight.rs` conservative UB sums |
| Bench baseline | `origin/main` build |

## PR-B (stacked on PR-A)

| Field | Value |
|-------|-------|
| Additional commits | `e36fd6489`, `37938f943`, `7df4df621`, `a4e6469d1`, `f5ebb59a0` |
| **Hard rule** | `7df4df621` + `f5ebb59a0` same PR |
| Incremental rust budget | ~850 lines vs PR-A (soft ≤2000) |
| Bench baseline | PR-A merge tip |

## PR-C (optional, not PR-A/B)

Tier-1 wand ports (`f63`/`b570`/`8f3a`) if maintainer wants analysis catchup separate.

## Gates

- 943/943 dump bit-identical
- Any shape **>+5pp** → FAIL
- No push / `gh pr` without user approval (execution plan §3.6)
