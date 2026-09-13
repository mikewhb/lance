# Phase 3 review — merged

Date: 2026-09-13  
Input: `pr-a-summary.md` + `wand_iu_tight.rs` diff

## Verdict

**APPROVE WITH CHANGES** — re-bench vs **current main** before opening PR.

## Evidence

- [x] f32 conservative sums in `iu_tight_required_shoulds` and per-doc prune
- [x] Boundary test prevents `Some(Vec::new())` false termination
- [x] TermLeaf + iu_tight dependency order preserved
- [ ] Fresh SBG vs main — pending

## P1

- Do not merge Slice 2 without upper (N/A for PR-A scope).
