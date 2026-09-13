# PR-A summary — `iu_tight` + TermLeaf

Date: 2026-09-13  
Base: `origin/main`  
Branch: `bench/community-fts-stack-for-pr` (PR-A slice)

## Commits

- `fd1d4b829` — 5b `iu_tight` kernel
- `a489e15a7` — TermLeaf + identity unwrap + design doc

## Blocker fix (included)

`wand_iu_tight.rs`: conservative upper-bound sums via `outward_f32_upper_bound`; f32 rounding boundary test (`required_shoulds_does_not_empty_on_f32_rounding_boundary`).

## Claims (re-bench vs current main before merge)

| Scenario | Expected |
|----------|----------|
| IU 40 shape | **−64%** order of magnitude |
| TOP_10 AVERAGE | **−11%** |
| union / phrase / intersection | **±1.5%** |

## Guards

- 943/943 bit-identical scores
- No shape **>+5pp**

## Test plan

- [ ] `cargo test -p lance-index`
- [ ] `cargo clippy -p lance-index -- -D warnings`
- [ ] SBG 3×3 vs main
- [ ] TermLeaf semantics: `R < F ≤ R+O`, no floor skip (`docs/fts-compound-term-leaf.md`)
