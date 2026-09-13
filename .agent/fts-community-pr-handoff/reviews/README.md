# FTS community PR handoff — agent review artifacts

Each phase of [execution-plan.md](../execution-plan.md) requires **two independent agent reviews**
before the next phase starts.

## File naming

```text
reviews/phase-<N>-review.md          Phase 0 plan review
reviews/phase-<N>-contract-review.md Phase 1/2 contract review (before coding)
reviews/phase-<N>-code-review.md     Phase 1/3/4 code + bench review (after implementation)
```

For Phase 0, use `phase-0-review.md` only.

## Review template

```markdown
# Phase <N> review — <Agent label A|B>

Date:
Reviewer: independent agent (not implementer)
Inputs: <list paths: plan, diff, bench JSON, dump>

## Verdict

APPROVE | APPROVE WITH CHANGES | REJECT

## P0 (block merge / next phase)

- ...

## P1 (should fix before PR)

- ...

## Evidence checked

- [ ] git diff stat vs 2000-line soft limit
- [ ] 943/943 dump
- [ ] summarize_shapes all shapes within ±5pp
- [ ] Semantics: TermLeaf no floor skip on advance
- [ ] PR-B: Slice 2 bundled with upper bound
- [ ] PR-A: iu_tight next_up widening
- [ ] Bench numbers from this phase rerun (not stale session)

## Notes

...
```

## Independence rule

- Review A and Review B must be separate agent invocations.
- Implementer must not mark their own work APPROVE without a second reviewer.
