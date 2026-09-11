# Compound 1-term 叶子：posting 近似，不上 Wikipedia IU 核

日期：2026-09-11（独立检视后修订）  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `db7fb151e`（`Revert "perf(fts): skip ReqOpt windows before exact MUST advance"`）  
对照树：`perf/fts-top10-analysis` @ `3d1189834`（A1 TOP_10 **AVERAGE** 1.16× Lucene，不是 leftover 四条）  
相关：[fts-floor-propagation-eli5.md](./fts-floor-propagation-eli5.md)、[fts-next-optimizations.md](./fts-next-optimizations.md)、[fts-next-optimizations-production.md](./fts-next-optimizations-production.md)

本文是实现契约。工作区里未提交的 `WandCursor` posting-approx / ReqOpt group-skip **不是**社区现状，不能当本刀起点。本刀相对干净 HEAD 写。

---

## 一句话

复合树上的 **1-term Match 叶子改成瘦 posting 游标**（只 GEQ id，`score()` 再解 freq）。ReqOpt 仍只存 `F`。**不**把 leftover 用 df 门塞进 `iu_tight_search`。**不**移植分析树 `TermLeafScorer` 的 `skip_dead_windows` / `should_emit`。

---

## 分数语义（这一层只有这一条）

`MUST + SHOULD`：`S(d) = R(d) + O(d)`。`F` 是堆底。

| 做法 | 是否安全 |
|---|---|
| 父节点：窗 `BMC(R)+BMC(O) < F` 则跳 | 就地消费，要 |
| 父节点：窗 `BMC(R) < F` 则 optional 临时必有（求交） | 就地消费，要 |
| 把满额 `F` 写成 MUST 的 `threshold` 并按它跳块 / 丢 doc | **不要** |
| 求交阶段：某个 SHOULD **单独**必有，且明显更便宜 → 换 lead | 5b 已有；门是偏斜 |

Lucene `ReqOptSumScorer` 只存 `F`，leapfrog 走 posting 近似，`score()` 另算。

---

## 分析分支 leftover ~1.2× 的真正原因（检视后改写）

官方 SBG A1 TOP_10 min µs（分析 JSON `unit16` 与 Lucene **同一次**；社区列是 lead-stream 官方 JSON，**不要**和 2862 那列横比当倍率）：

| 查询 | 分析分支 | 同 JSON Lucene | 社区 lead-stream JSON |
|---|---:|---:|---:|
| `airport +security rules` | 3602 | 2862 | 28812 |
| `+climate change impact report` | 4315 | 3486 | 29075 |
| `+water quality report` | 6427 | 5481 | 48726 |
| `college +admissions criteria` | 1005 | 1194 | 4823 |

分析树上是 **两件不同的事**，leftover 四条吃到的几乎全是 B：

### A. 通用：`box_leaf_scorer`（`3d1189834` 的 tree；引入 commit `8beaa4ffa`）

谓词：`postings.len()==1 && phrase_slop.is_none() && !has_grouped_terms()` → `TermLeafScorer`。只解释 **仍留在 compound 树** 的 1-term 孩子（进不了 tight 的 Boolean）。

### B. 非通用：df 门把 leftover 送进 `iu_tight_search`

hack 全关、`iu_tight() == None` 时：

```text
iu_maxscore() || must_cost <= 2 * min_should || must_cost >= 85_000
```

命中则 **直接** `iu_tight_search`（MUST `take_docs_one_block_upto`），**不会**走到 `box_leaf_scorer`。`security` 77k vs `airport` 56k：`77k <= 2*56k`，airport leftover 进 tight。

**没有**在分析树上关掉 tight 再量 leftover 的实验。因此 **3.6ms 不能**当作「换 posting 叶子 / 剥 Boolean」的承诺。

分析树那个 `TermLeafScorer` **不是** Lucene `TermScorer`：`sticky_floor` / `skip_dead_windows` / `should_emit` 在 `advance` 里按叶子 floor 丢 doc。ReqOpt 一旦下传 `F` 就会杀掉 `R < F ≤ R+O`。它是反例，不是模板。

社区 5b：`lead_cost * 3 < must_cost`（`IU_TIGHT_LEAD_COST_RATIO`）。有明显更稀的 SHOULD 才换核。airport 进不去是对的。生产文档：**85k 不上**，`collect_must_driven` / `LANCE_HACK_IU_*` **不上**。

---

## 干净 HEAD 上社区卡在哪

`db7fb151e` 上，复合 1-term 叶子仍是 `WandCursor`：`next`/`advance` 走 `position_next`，conjunction 热循环付 WAND 堆 + 解 freq + BM25。建树：`CompoundScorerPlan::build` 每片叶子都套 `ScaleScorer`；`BooleanScorer::try_new` 有 MUST 就先套 `RequiredConjunctionScorer`，再视情况套 `ReqOptScorer`，最后总是再包一层 `BooleanScorer`。

IU tight 在 `plan.build` **之前**取 posting（`iu_tight_leaf_indices` + `iu_tight_should_switch`）。本刀不改这扇门。

工作区曾有 1-term posting-approx（`wand_pos_next=0`）和已 revert 又堆回来的 group-skip。那些 **丢掉或不带进本刀**。本刀不把 `combined_group_skip` 写进现状或验收。

`fts-reqopt-approximation-eli5.md` 相对 **HEAD** 仍对（conjunction 付完整 advance）；相对工作区过时。

---

## 做 / 不做

### 做（本刀，相对干净 HEAD）

1. **瘦 posting 叶子**（新类型，不要分析树 `TermLeafScorer` 原样，也不要把 1-term 继续养在 `WandCursor` 的 1 元素堆里）。
   - 谓词：一条 posting、`phrase_slop.is_none()`、`!has_grouped_terms()`。
   - `advance`/`next`：只 `next_doc_id`（freq 保持 0）+ visibility/`document_key`。
   - **禁止** `sticky_floor` / `window_floor` / `should_emit` / `skip_dead_windows` / `skip_single_or_impacts`。叶子可以存 `min_competitive_score` 供浅窗上界，但 **不得**用它在近似档丢掉文档。
   - `score()`：`posting.doc()` 解 freq + 与现在相同的 BM25。
   - 多 term OR、短语、grouped、AND 叶子仍 `WandCursor` / `Wand::search`。

2. **建树去掉真正的 identity 节点**（可与叶子同一 PR，但回归归因以 dump 为准；若 dump 坏了先只留叶子）：
   - `must.len()==1`：ReqOpt.required / Boolean.driver 就是那一个孩子，不套 `RequiredConjunctionScorer`。
   - ReqOpt 已吸收 SHOULD（两边 `scores_non_negative`），且 **没有** leftover `BooleanScorer.optional`、**没有** `must_not`：`plan.build` 返回 ReqOpt，不包 `BooleanScorer`。
   - **`boost==1.0` 仍套 `ScaleScorer`（本刀不剥）。** `ScaleScorer` 在 `factor==1` 时仍做 **exclusive** floor 翻译（孩子拿到严格小于 `F` 的 raw floor）。复合 `WandCursor` 是 Inclusive。剥掉 Scale 会改变纯 MUST 的叶子 floor，不是分数乘法 identity。留给单独证明 dump 的刀。

### 明确不做

- 不放宽 5b，不加 `must<=2*should` / `85_000` / 查询名
- 不 port `collect_must_driven`、`LANCE_HACK_IU_*`
- 不把满额 `F` 写入 MUST 并用它 skip/丢 doc
- 不改 `advance_shallow` 去 `optional.advance`
- 不改 BM25 `k1/b`、不改稳定倒排格式
- 不把 identity unwrap 扩到「任意 Boolean」或剥 `RowAddressScorer`

### 硬不变量（unwrap）

- 有 `must_not` → 必须仍是 `BooleanScorer`（只有它过滤 prohibited）。
- `scores_non_negative()==false` → SHOULD 仍挂在 Boolean.optional，不能只返回 conjunction。
- 短语 / grouped 孩子不得进 posting 叶子。
- `must.len()>=2` 仍 conjunction，且 **仍不**把满额 `F` 传给单个 MUST 孩子。
- IU tight 先取 posting 再 `plan.build` 的顺序不动；airport 仍不进 5b，`customer +service` 仍进。

---

## 正确性契约

- 相对刀前 **同一干净树** 的 SBG `TOP_10` dump：**943 条** row + `f32` bit-identical。
- 1-term `score()` 与 HEAD 上 `WandCursor` 打出的 BM25 同一公式（`posting.score` + `doc_length`）。
- `boost != 1` 仍 `ScaleScorer`，`F/k` exclusive 翻译不变。

---

## 实现落点（符号，不用行号）

| 改动 | 位置 |
|---|---|
| 瘦 posting 叶子 | `wand.rs`（`TermLeafScorer` 之名可留，行为按上文禁止项） |
| 造叶 | `collect_partition_with_documents` 里现在 `WandCursor::new` 的分支 |
| 1-MUST / 直接返回 ReqOpt | `BooleanScorer::try_new`；`CompoundScorerPlan::build` 的 Boolean 臂若改为返回 `BoxScorer` |
| 单测 | 现有 `compound.rs` / `wand.rs` `#[cfg(test)]` |

---

## 测试

点名新测 + 现有 ReqOpt / Scale / IU / Boolean，各 <1s：

1. 瘦叶子 `advance` 后 freq 仍为 0，直到 `score()`。
2. grouped / `phrase_slop` / `postings.len()!=1` 仍 `WandCursor`。
3. `Boolean{must:[a], should:[b,c]}` 根是 ReqOpt，required **不是** conjunction；`must_not` 根仍是 Boolean。
4. `boost==1` **仍有** Scale；`boost==2` 有 Scale。
5. `must` 两个孩子仍 conjunction，且 `set_min_competitive_score` 不把满额 `F` 传给单个孩子。
6. `scores_non_negative()==false` 时根仍 Boolean，分数含 SHOULD。
7. 5b：airport 形状不进 tight；skew 形状仍进（用现有 IU tight 单测，不改门）。

Dump（bench 前）：943 条相对刀前 HEAD 一致。至少覆盖纯 1-MUST、多 MUST、短语、union、`customer +service`、airport、`must_not`、`boost=2`。

---

## Bench 与提交

尺子：A1 TOP_10，`taskset -c 0`，warmup 60s，10 iters min，hack 全关，**LTO 官方 `do_query`**。  
对照：本分支 **本刀之前最近一次官方 LTO JSON**。若没有新跑，先对干净 HEAD 跑一次再打本刀，不要用 lead-stream JSON 去减后面已经合入又 revert 的刀。Lucene 用 **同一次** 跑的列。

通过：

- leftover 四条墙钟下降（不承诺分析树 3.6ms / 不承诺 Lucene 3ms）
- intersection / union / phrase 均值相对 **同协议 HEAD 官方跑** 不回归（约 ±3%）
- 943/943 dump 一致

不通过：**commit + revert**。LTO-off ad-hoc 不当通过线。

---

## 工作顺序

写代码（相对干净 HEAD）→ 作者自审低级问题 → 点名单测 → 独立 agent 代码检视 → 修 P0/P1 → dump + bench → commit，或 commit + revert。
