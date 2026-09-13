# Community FTS → 分析追平 + PR 整合执行计划

Date: 2026-09-13  
Status: **Tier-1 port REVERTED** — fair SBG shows +1.3% all / +8.9% IU regression; see `bench/tier1-bench-analysis.md`  
工作树：`bench/community-fts-stack` @ `b7dcb4207`  
整合分支：`bench/community-fts-stack-for-pr` @ `b7dcb4207`
目标 PR 分支：`bench/community-fts-stack-for-pr`（整合专用，本计划批准后创建）  
参考分析树：`perf/fts-top10-analysis` @ `3d1189834`  
社区尺子：[docs/fts-community-stack-baseline.md](../../docs/fts-community-stack-baseline.md)  
相关：[docs/fts-compound-term-leaf.md](../../docs/fts-compound-term-leaf.md)、[../fts-iu-r3-community-port/contract.md](../fts-iu-r3-community-port/contract.md)（**superseded for HEAD** — that contract pins `e3a8aecd0`; stack HEAD is **`f5ebb59a0`** + Phase 1 wand ports）

**Supersedes** [docs/fts-community-pr-triage.md](../../docs/fts-community-pr-triage.md) §3 five-PR boxing → this plan’s **PR-A / PR-B (+ optional PR-C)** split. Lead-stream / phrase R5 stay on bench branch, not in PR-A/B.

**Canonical bench CPU pin:** `taskset -c 32` (historical `taskset -c 0` in older TermLeaf notes is not comparable).

---

## 1. 目标（两件事，顺序固定）

| 序 | 轨道 | 产出 | 成功判据 |
|---|------|------|----------|
| **1** | **追分析（FTS 叶子）** | 社区缺且 **有 SBG/profile/diff 三证** 的 wand/maxscore commit | HEAD AVERAGE 向分析 1775µs 靠近；943 dump 不变；无 shape >+5pp |
| **2** | **PR 整合** | 从轨道 1 完成后的 HEAD 拉 `bench/community-fts-stack-for-pr`，整理 **2 个 reviewable PR** | 每 PR rust diff **≤2000 行（soft）**；主 claim 场景明确；943/943 + shape gate |

**不在本计划内（明确排除）**

- DataFusion 路径税（`95a86b3ec` TaskContext 等）— 用户要求扣 DF 地板，不纳入 FTS PR。
- 分析 df 门 `iu_tight_cost_gate`（85k / `must≤2×should`）— 生产文档禁止，需另开设计，**不进 PR-A/B**。
- `b792526a8` / `c128a0637` addends slab — hack，已关闭。
- lead-stream 全链、phrase R5、H2A 等 — 已在栈上，**不重排进 PR-A/B**（PR 只包下表 commit）。

---

## 2. 当前状态快照

### 2.1 相对分析 gap（扣 DF 后差值不变）

| 指标 | 社区 HEAD | 分析 `3d1189834` | gap |
|------|----------:|-----------------:|----:|
| TOP_10 AVERAGE | ~2,418µs | 1,775µs | **+643µs（1.36×）** |
| IU 40 avg | ~4,660µs | 2,956µs | +1,704µs/query |
| union 301 avg | ~1,953µs | 1,588µs | +365µs/query |

证据：`.agent/fts-community-pr-handoff/summarize_shapes.py` @ `termleaf-upper-bench` cand；分析 @ `docs/fts-next-optimizations.md` §6。

### 2.2 栈上已有、PR 整合时会用到的 commit

| commit | 内容 | 行数（约） |
|--------|------|----------:|
| `fd1d4b829` | 5b iu_tight | +590 |
| `a489e15a7` | TermLeaf + identity unwrap + `docs/fts-compound-term-leaf.md` | +860 |
| `e36fd6489` | Phase 2 same-block scan | +34 |
| `37938f943` | Slice 1 collect_confirmed_window | +276/−48 |
| `7df4df621` | Slice 2 ReqOpt window collect | +422 |
| `a4e6469d1` | Slice 3 WandCursor window floor | +29/−2 |
| `f5ebb59a0` | TermLeaf conservative upper | +90/−1 |

### 2.3 相对分析仍缺、本计划轨道 1 要 port 的 commit

| commit | 机制 | 社区代码证据（未合） | SBG 尺子 |
|--------|------|----------------------|----------|
| `f63d18dcd` | WAND seek 不每 doc 重分配 | 无 `tail_scratch`；Or `seek` 仍 collect 到 fresh `Vec`（`wand.rs:6243–6265`） | **−133µs** AVERAGE（3334→3201） |
| `b5703fcb7` | Or seek 保留 `up_to` 窗 | Or `seek` 仍 unconditional `up_to=None`（And 分支除外） | **−171µs**（3201→3030） |
| `8f3a4f99c` | MaxScore 地板升高重开窗 | `wand.rs` `maxscore_search` 无 `essential_split_is_stale` | **−93µs** mix；union **−10.9%** |
| `41e73d1ad` | 偏斜 2-clause 不进 bulk | `enabled_for` 仍 `2\|3` 无条件（`:301`） | AVERAGE **−10µs**；离群 query 15×→1.4× |
| `4cf28da10` | MaxScore stranded 推进 | wand 无 stranded 路径 | liveness fix，无 AVERAGE 账 |

来源：`fts-primitive-cost/conclusion.md`、`fts-1.20x-floor-and-union/final/analysis.md`、现场 `rg`/`git show`。

**预期**：Tier 1 三刀在分析树 **−397µs**；社区 TermLeaf 已剥 MUST 侧 WAND 税 → 叠到 HEAD 合理预期 **−250~350µs**（须本机重 bench，不得写死进 PR 描述）。

---

## 3. PR 整合目标（轨道 2，在轨道 1 + 分支创建之后）

从 **`origin/main` 最新**（或 maintainer 指定 base）rebase 后，只提交下面两包。**不得**把轨道 1 的 wand micro-opt 与 PR-A/B 混在一个 PR（review 边界清晰）。

### PR-A：`iu_tight` + TermLeaf posting 叶

| 项 | 值 |
|----|-----|
| **commits** | `fd1d4b829` + `a489e15a7`（含 `docs/fts-compound-term-leaf.md`） |
| **rust 行数** | ~**1,269** insert rust-only（590+860−doc；含 doc ~1,450 total，soft ≤2000 ✅） |
| **硬依赖** | PR-A 必须先于 PR-B；`a489` import `iu_tight_search` |
| **合并前 blocker** | `wand_iu_tight.rs` f32 **`next_up` 加宽**（`docs/fts-perf-commits-review.md` §3） |

**主 claim（写进 PR 描述）**

| 场景 | 预期 | 会话证据 |
|------|------|----------|
| IU leftover 四条 | **−65~79%** | `a489` @ `.agent/fts-compound-term-leaf/note.md` |
| IU 40 shape | **−64.3%** | 同上 |
| `customer +service phone number` | **−95%**（iu_tight 换核） | `fd1d4b829` triage |
| TOP_10 AVERAGE | **−11%** | 同上 |
| union / phrase / intersection | **±1.5%** | gate |

**Guard**：943/943 dump bit-identical；任一 shape **>+5pp → 不 merge**。

### PR-B：ReqOpt 窗 collect + TermLeaf upper（叠 PR-A）

| 项 | 值 |
|----|-----|
| **commits** | PR-A + `e36fd6489` + `37938f943` + `7df4df621` + `a4e6469d1` + `f5ebb59a0` |
| **相对 PR-A 新增 rust** | ~+850 行 compound/wand（soft ≤2000 ✅） |
| **硬规则** | **`7df4df621` 必须与 `f5ebb59a0` 同 PR**（Slice 2  alone IU **+4%**；加 upper **−7.4%** vs Slice2+3 栈） |

**主 claim**

| 场景 | 相对 PR-A / Slice2+3 栈 | 证据 |
|------|-------------------------|------|
| IU 40 | **−7.4%** | `termleaf-upper-bench` 3×3 |
| `+water` / `+climate` | **~−20%** | 同上 per-query |
| union / phrase / intersection | **−0.3% ~ −1.1%** | summarize_shapes |
| all 943 | **−1.5%** | 同上 |

**Guard**：同上 ±5pp；**禁止**单独 merge Slice 2 不带 upper。

### 两 PR 与轨道 1 的关系

```text
bench/community-fts-stack
  │
  ├─ Phase 1：port f63 / b570 / 8f3a（+ 可选 41e73 / 4cf28）
  │            → 双 agent 评审 → bench → commit(s) 留在本分支或 track/analysis-catchup
  │
  ├─ git branch bench/community-fts-stack-for-pr   ← 从 Phase 1 完成后的 HEAD
  │
  └─ Phase 3–4：在 -for-pr 上 relative to main 整理 PR-A、PR-B（squash/rebase 叙事）
                 （轨道 1 的 commit 可不在 PR-A/B diff 里，若 maintainer 要求分析追平单独 PR）
```

**默认**：轨道 1 完成后 **-for-pr 包含全部优化**；PR-A/B 只选取 fd1d4b829…f5ebb59a0 相对 **main** 的 diff。轨道 1 commit 另开 **PR-C（wand micro-opt）** 若超 2000 行则再拆。

---

## 4. 阶段划分与门禁

每个阶段：**写/更新文档 → 独立 2 agent 交叉评审（文档 + 代码）→ 通过后才进入下一阶段**。

评审产物：`.agent/fts-community-pr-handoff/reviews/phase-<N>-review-agent-a.md` + `phase-<N>-review-agent-b.md`，合并摘要 `phase-<N>-review.md`  
评审结论只能是：**APPROVE / APPROVE WITH CHANGES / REJECT**（须 file:line 或 bench 表引用）。

### Phase 0 — 本执行计划（当前）

| 步骤 | 动作 | 产出 |
|------|------|------|
| 0.1 | 维护本文 | `execution-plan.md` |
| 0.2 | **Agent 评审 ×2**（不同 session / 不同 agent；只读 plan + 引用 doc） | `reviews/phase-0-review.md` |
| 0.3 | 吸收 P0/P1 → 修订本文 | 本文 Status → **APPROVED** |

**Phase 0 出口**：双 APPROVE（或 APPROVE WITH CHANGES 且 P0 已修）。

---

### Phase 1 — 追分析（Tier 1 + 可选 Tier 2）

| 步骤 | 动作 | 产出 |
|------|------|------|
| 1.0 | 写 **`analysis-catchup-contract.md`**（逐 commit：diff 范围、测试、bench gate、回滚） | 同目录 |
| 1.1 | **Agent 评审 ×2**（contract + 引用的 `git show` 片段） | `reviews/phase-1-contract-review.md` |
| 1.2 | 在 `bench/community-fts-stack` 按序 port | 见下表 |
| 1.3 | 每 commit：`cargo test -p lance-index` + clippy + **943 dump** + 3×3 SBG | JSON under `.agent/fts-community-pr-handoff/bench/` |
| 1.4 | **Agent 评审 ×2**（代码 diff + bench 表 + dump md5） | `reviews/phase-1-code-review.md` |
| 1.5 | 失败 → **commit + revert** 该 commit，不进入 Phase 2 | |

**Port 顺序与 gate**

| 序 | commit | 文件 | Pass gate（vs 上一步） |
|---|--------|------|------------------------|
| 1.1 | `f63d18dcd` | `wand.rs` | 943/943；all ±5pp；IU 不 >+5pp |
| 1.2 | `b5703fcb7` | `wand.rs` | 同上 |
| 1.3 | `8f3a4f99c` | `wand.rs` 或 `wand_maxscore.rs`（按社区模块落点） | union 主 claim **≥−5%** 或 mix **≥−50µs**；IU ±5pp |
| 1.4（可选） | `4cf28da10` | maxscore 路径 | 943/943；无 hang 单测 |
| 1.5（可选） | `41e73d1ad` | `wand.rs` bulk 门 | 943/943；`+the +incredibles` 类 **≥−50%**；AVERAGE ±5pp |

**Cherry-pick 纪律**：禁止 blind pick 整段 `3d1189834`；逐 commit 手工适配 split module（`wand_lead_stream.rs` / `wand_maxscore.rs`）。

**Phase 1 出口**：Tier 1 三 commit 全 green + 双 agent code APPROVE；记录 HEAD 新 SHA 与 AVERAGE。

---

### Phase 2 — 创建整合分支

| 步骤 | 动作 |
|------|------|
| 2.0 | 写 **`pr-integration-contract.md`**（PR-A/B commit 列表、rebase base、行数预算、cherry-pick 顺序） |
| 2.1 | **Agent 评审 ×2** | `reviews/phase-2-contract-review.md` |
| 2.2 | `git branch bench/community-fts-stack-for-pr <Phase-1-HEAD>` |
| 2.3 | 确认 **不** 在 `-for-pr` 上继续实验性 hack |

**Phase 2 出口**：分支存在；contract APPROVE；`git log -1` 写入 contract。

---

### Phase 3 — PR-A 整合

| 步骤 | 动作 |
|------|------|
| 3.0 | **`wand_iu_tight.rs` f32 conservative `outward_f32_upper_bound` 加宽** + f32 边界单测（PR-A blocker；见 `docs/fts-perf-commits-review.md` §3） |
| 3.1 | 相对 `origin/main` rebase；整理 **1~2 squash commit**（叙事：`iu_tight` + TermLeaf） |
| 3.2 | `git diff origin/main...HEAD --stat` → 确认 **≤2000** rust 行 |
| 3.3 | 943 dump + 3×3 SBG vs **main 上 build**（非会话旧 JSON） |
| 3.4 | 写 **`pr-a-summary.md`**（claim 表 + test plan） |
| 3.5 | **Agent 评审 ×2**（diff + bench + dump + 语义：`docs/fts-compound-term-leaf.md`） | `reviews/phase-3-review.md` |
| 3.6 | 用户批准后 `gh pr create`（**不在本计划自动 push**） |

**Phase 3 出口**：PR-A URL；reviews APPROVE；bench 满足 PR-A 表。

---

### Phase 4 — PR-B 整合（依赖 PR-A merge 或 stacked PR）

| 步骤 | 动作 |
|------|------|
| 4.1 | 在 PR-A 之上叠 commit **`e36fd6489`**（Phase 2 same-block）+ Slice1–3 + upper；**单 PR 含 7df4df621+f5ebb59a0** |
| 4.2 | stat ≤2000（仅 PR-B 增量） |
| 4.3 | Bench vs **PR-A merge 基线**；IU 主 claim **−7.4%** 量级须复现 |
| 4.4 | **`pr-b-summary.md`** + **Agent 评审 ×2** | `reviews/phase-4-review.md` |
| 4.5 | 用户批准后开 PR |

**Phase 4 出口**：PR-B URL；禁止 Slice 2-only 中间态对外 review。

---

## 5. Bench 与 dump 协议（全阶段统一）

| 项 | 值 |
|----|-----|
| 命令 | A1 TOP_10；warmup 60s；10 iters min；`taskset -c 32`（或文档 pin，全程一致） |
| 二进制 | LTO `do_query`；`LANCE_HACK_*` / `LANCE_DIAG_*` **unset** |
| Dump | 943 条 `TOP_10` row + `f32` bit-identical vs 上一步基线 |
| 汇总 | `.agent/fts-community-pr-handoff/summarize_shapes.py` |
| 脚本 | `.agent/fts-community-pr-handoff/bench-addend-run.sh`（可复用） |

**Hard gate**：任一 shape **>+5.0%**（时间加权 sum median-of-3-rep）→ **FAIL**，revert 或不得 merge。

**Soft gate**：PR diff **>2000** rust 行 → 必须拆 PR 或写 maintainer 例外说明。

---

## 6. 双 Agent 交叉评审规范

1. **独立性**：两次评审须不同 agent 会话；评审者不得是 implementer 同一 turn 链。
2. **输入包**（每个 Phase 最小集）：
   - 本 phase 的 contract/summary md
   - `git diff` stat + 关键 file:line
   - bench JSON 路径 + summarize_shapes 输出
   - dump 对比结论（943/943 或 FAIL 列表）
3. **必查项**：
   - 语义：`R < F ≤ R+O`（TermLeaf 禁止 floor skip doc）
   - `iu_tight` f32 加宽（PR-A）
   - Slice 2 无 upper 不 ship（PR-B）
   - 主 claim 数字是否来自 **本 phase 重跑**（禁止沿用旧 session 当 merge 证据）
4. **输出**：`reviews/phase-N-review.md` 模板见 [reviews/README.md](./reviews/README.md)。

---

## 7. 风险与回滚

| 风险 | 缓解 |
|------|------|
| Slice 2 无 upper 导致 IU +4% | PR-B 强制绑定 `7df4df621`+`f5ebb59a0` |
| `iu_tight` f32 丢结果 | PR-A 前必须 next_up + 单测 |
| wand port 在 split module 冲突 | Phase 1 逐 commit；禁止 pick `3d1189834` 单体 |
| rebase main 后数字漂移 | 每 PR 相对 **当前 main** 重跑 bench |
| PR 超 2000 行 | PR-C 拆 wand micro-opt；PR-A/B 不塞 f63/b570/8f3a |

**回滚**：任一 phase bench FAIL → `git revert` 该 phase 引入的 commit；不 amend 已 push commit。

---

## 8. 执行顺序总览

```text
Phase 0  execution-plan.md ──► 2× agent review ──► APPROVE
   │
Phase 1  analysis-catchup-contract ──► 2× review ──► port f63→b570→8f3a (+opt)
         ──► bench/dump ──► 2× code review ──► commits on community-fts-stack
   │
Phase 2  pr-integration-contract ──► 2× review ──► branch community-fts-stack-for-pr
   │
Phase 3  PR-A (fd1d4b829+a489+next_up) ──► bench ──► 2× review ──► gh pr (用户触发)
   │
Phase 4  PR-B (+ Phase2 + Slice1-3 + upper) ──► bench ──► 2× review ──► gh pr
```

---

## 9. 当前动作（规划阶段，不做整合）

- [x] 撰写 `execution-plan.md`（本文）
- [x] Phase 0：发起 **2 个独立 agent** 评审本文 → `reviews/phase-0-review*.md`
- [x] Phase 0 P0/P1 修订 → Status **APPROVED**
- [x] **然后** Phase 1 contract + 实现
- [x] Phase 1 完成后 `git branch bench/community-fts-stack-for-pr`
- [x] Phase 3–4 整合文档（`pr-a-summary.md` / `pr-b-summary.md` + reviews）
- [ ] SBG 3×3 + 943 dump（Phase 1 gate — run `bench-addend-run.sh` before PR merge claims）
- [ ] 用户批准后 `gh pr create`（不自动 push）

---

## 10. 引用索引

| 主题 | 路径 |
|------|------|
| PR 分箱 triage | `docs/fts-community-pr-triage.md` |
| TermLeaf 契约 | `docs/fts-compound-term-leaf.md` |
| Phase 3 port | `.agent/fts-iu-r3-community-port/contract.md` |
| 分析 gap / Tier 1 | 会话分析 + `fts-primitive-cost/conclusion.md` |
| f32 缺陷 | `docs/fts-perf-commits-review.md` §3 |
| Upper bound bench | `/data/arrow/code/sbg-targets/termleaf-upper-bench/` |
