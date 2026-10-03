# IU 引擎覆盖矩阵:Lucene / Tantivy / Lance 的 Boolean 语义引擎是怎么覆盖「必选×可选」空间的

> 分支:`fts/iu-reuse-main`(社区 main `7b97db782` + IU cherry-pick)
> 对照源码:lucene(本地源码树为 **11.0**,`Version.java LUCENE_11_0_0`;
>   sbg 基准引擎名为 lucene-10.3.0,命名滞后)、tantivy(`/data/arrow/code/tantivy`)、
> lance 本分支与基线 `ecb388f73`
> 日期:2026-10-03

---

## 1. 问题定义

全文本检索的 Boolean 查询只有两种子句角色:**必选(MUST/FILTER)** 与 **可选(SHOULD)**。
记必选子句数为 `K`、可选子句数为 `N`,则任何 Boolean 查询都落在下面的矩阵里:

```
                 可选子句 N
                 0            ≥1
          ┌──────────┬─────────────────┐
 必选  K=0 │ (空)      │ 纯 OR            │
 子句  K=1 │ 纯 term   │ ReqOpt(1必选+可选)│
      K≥2 │ 纯 AND    │ 混合 Boolean      │
          └──────────┴─────────────────┘
```

每个格子都需要一个「产出候选 + 按 `必选必须命中、可选命中加总` 计分」的引擎。
本文回答三个问题:**Lance 现在哪些格子有快引擎?Lucene/Tantivy 怎么覆盖?缺的格子要多少代码?**

---

## 2. wand_iu_tight.rs(362 行)在做什么

它服务 `K=1, N≥1` 这个格子,主循环四步:

```
┌────────────────────────────────────────────────────────┐
│ ① 取 MUST 的一个解码块(take_docs_one_block_upto)          │
│      必选子句是唯一保证出现的,由它驱动                      │
│ ② 把每个 SHOULD 在 [窗起,窗末] 内的 doc 批量解码进缓冲       │  ← C2
│      (替代:每个候选对每个 SHOULD 做一次 seek)               │
│ ③ 逐候选:score = must_score + Σ(命中的 SHOULD)             │
│      floor 预检:must + Σ(全局ub) < floor → 跳过             │
│      on_hit(key, score) → top-k                           │
│ ④ 每块结束:must_ub < floor 且某 SHOULD 单独必需且 3x 稀有?  │
│      → promoted():切换成「稀有 SHOULD 驱动的交集」           │
└────────────────────────────────────────────────────────┘
```

**为什么 main 的两个现成引擎表达不了这个语义:**

| main 已有引擎 | 语义 | 为什么不行 |
|---|---|---|
| AND 内核(bulk/lead-stream/classic) | 所有子句必须命中 | 没有「可选命中才加总」 |
| MaxScore(OR) | 只命中 optional 也计分 | 没有「必选子句」 |
| ReqOptScorer(基线已有) | **语义完全一致** | 但通过 scorer trait 逐候选虚调用
`advance`——慢。IU −69.5%(1080.9→329.7ms)的来源正是绕开它 |

一句话:**IU 的语义卡在 AND 和 OR 之间;语义上 ReqOptScorer 已经是对的,
慢在抽象层,所以必须手写一个贴 posting 的引擎。**

---

## 3. Lance 现状:快引擎覆盖矩阵(存在一个洞)

| 格子 | Lance(本分支) | 状态 |
|---|---|---|
| K=0, N≥1(纯 OR) | `maxscore_search`(有门控:compressed、无 grouped、`USE_MAXSCORE_SEARCH`) | ✓ main 已有(有条件) |
| K=1, N=0(单 term) | `TermLeafScorer` | ✓ 本分支 |
| **K=1, N≥1(IU)** | **`iu_tight_search` 内核** | ✓ 本分支(−69.5%) |
| K≥2, N=0(纯 AND) | `and_bulk_search` / lead-stream / classic | ✓ main 已有 |
| **K≥2, N≥1(混合)** | `ReqOptScorer(RequiredConjunctionScorer, union)` | ✗ **无快引擎**——落到
基线的逐候选虚调用路径,正是 −69.5% 之前的慢形态 |

**洞的性质**:不是缺引擎,而是缺**快引擎**。`K≥2 + N` 目前走基线 ReqOptScorer 的
通用路径(每必选子句一个 WandCursor、逐候选虚调用),与 `K=1 + N` 在 C2/内核之前
的形态相同。

---

## 4. Lucene(11.0)怎么覆盖:一个组合 scorer 吃掉整列

代码:`lucene/core/src/java/org/apache/lucene/search/BooleanScorerSupplier.java`(L150–184)。

```
纯 OR(K=0):        bulk 走 MaxScoreBulkScorer;scorer 路径走 BooleanScorer/WANDScorer
                    (视 TOP_SCORES 与 msm 而定)
K≥1 且 N≥1:         ReqOptSumScorer( req(MUST…), opt(SHOULD…) )   ← 不论 K 是 1 还是 ≥2
K≥1 且 N=0:         req(MUST…) → BlockMaxConjunctionScorer
MUST_NOT:           excl(...) 包在外面;FILTER 子句也算必选(req(FILTER, MUST,…))
minShouldMatch>0:   ConjunctionScorer([req, opt])  ← 把 optional 升级为必需(与我们的提升同型)
```

要点:
- **整列「K≥1 + N≥1」只用一个 `ReqOptSumScorer`,靠组合**:`req()` 把 K 个 MUST
  组合成一个 **plain `ConjunctionScorer`**(混合路径传 `topLevelScoringClause=false`,
  不吃 BlockMax;`BlockMaxConjunctionScorer` 只用于纯 AND 路径),
  `opt()` 把 N 个 SHOULD 组合成一个并集;K≥2 不需要新 scorer。
- `ReqOptSumScorer` 实现了 `advanceShallow` / `setMinCompetitiveScore`
  (ReqOptSumScorer.java:262/282),支持两阶段 collector 的窗口协议;
  且窗口内 `optIsRequired`(L86/285-296)会在 `reqMaxScoreBlock < minScore` 时
  **动态把 optional 升级为必需**——这是与我们「floor 触发提升」最接近的平行物
  (Lucene/Tantivy 的 msm 提升是静态查询期的,与此不同);
  但 optional 的解码仍是窗口内逐 doc(没有块级批量解码)。
- **没有** `BlockMaxReqOptSumScorer`(全树 grep 零命中)——Lucene 自己也留了这块性能空间。

**对 Lance 的启示**:`K≥2 + N` 的洞,Lucene 的解法是组合而非新内核——
把 K 个 MUST 交给已有的 AND 内核产出候选,再对 SHOULD 做归并加总。

---

## 5. Tantivy 怎么覆盖:同样是组合

代码:`src/query/boolean_query/boolean_weight.rs`(L375–500)、`src/query/reqopt_scorer.rs`。

```
SHOULD 侧先合成一个 scorer:
  N=0        → Ignored
  N≥1, K=0   → Optional(scorer_union)            ← 纯 OR
  msm == N   → Required(scorer_union) 并 promote_should_to_must  ← 与我们的提升同型
MUST 侧合成一个 scorer:
  K=1        → 该叶 scorer
  K≥2        → intersect_scorers(must…)          ← 交集组合
                (纯 term-AND 另有 TermIntersection 特化)

然后:
 (Optional(should), musts) 且 K≥1
   → RequiredOptionalScorer::new(must_scorer, should_scorer)   ← 不论 K 是 1 还是 ≥2
 (Required(should), musts)
   → intersect_scorers([must…, should])                        ← msm 语义
```

要点:
- **与 Lucene 同构**:必选侧组合成交集 scorer、可选侧组合成并集 scorer,
  中间一个 `RequiredOptionalScorer` 包住整列;`K≥2 + N` 不是洞。
- `RequiredOptionalScorer::score()` = req + (opt 命中则加 opt),
  逐 doc、无块级批量解码(`reqopt_scorer.rs:85-95`)——与 Lucene 同样留了性能空间。

---

## 6. 三家对比与取舍

| | K=1+N(IU) | K≥2+N(混合) | K≥1,N=0(AND) | K=0(OR) |
|---|---|---|---|---|
| **Lucene 11** | ReqOptSumScorer | ReqOptSumScorer(req=组合) | BlockMaxConjunction | BooleanScorer/WAND |
| **Tantivy** | RequiredOptionalScorer | RequiredOptionalScorer(组合) | Intersection | BufferedUnion/BlockWand |
| **Lance(社区 main)** | ReqOptScorer(慢) | ReqOptScorer(慢) | AND 内核 ✓ | MaxScore ✓ |
| **Lance(本分支)** | **iu_tight 内核 ✓** | ReqOptScorer(慢)✗ 洞 | AND 内核 ✓ | MaxScore ✓ |

**取舍的本质**:
- **组合(Lucene/Tantivy)**:一份代码覆盖整列,加法语义天然正确;
  代价是逐 doc 虚调用/逐 doc seek,块级批量解码的收益拿不到。
- **特化(Lance 本分支)**:实测 IU −85.3%(对社区 main)、全量 −33.6%;
  代价是每个格子一份引擎,且要自己维护 f32 位一致与剪枝正确性。

我们选择特化的依据是实测:对社区 main,IU 40q 从 1002.4ms → 147.5ms,
这个量级用组合式实现拿不到(基线 ReqOptScorer 就是组合式在 Lance 的等价物)。

---

## 7. 对 Lance 的建议:补 `K≥2 + N` 的洞,两条路

**路线 1(组合式,~100 行,推荐先做)**:
把 `promoted` 从「1 个 lead」推广成「K 个必需子句求交 + N 个 optional 归并」
——即 Lucene/Tantivy 的组合式,但用 posting 级原语
(`take_docs_one_block_upto` 解码必需侧、`merge` 归并 optional)。
原语全部现成;与方案 (a)(提升后交 AND 内核)二选一或互补。

**路线 2(特化式)——已实现并实测(2026-10-03)**:
`iu_tight_search_multi`(wand_iu_tight.rs):K 个必需子句按 cost 升序、最稀有者驱动,
seek-to-max 对齐全部必需游标,逐候选探测 N 个 optional 并按 plan 序加总
(与 RequiredConjunctionScorer + DisjunctionScorer 的加总顺序一致,f32 位一致)。
路由:`iu_tight_clause_indices` 接受 K≥1(替换 `iu_tight_leaf_indices`),
K=1 走原内核(带 promotion),K≥2 走 multi 内核。

实测(合成 2-MUST+1-SHOULD 查询 12 条,min-of-45,3 轮交错,对社区 main):
- **−82.4%**(74934 → 13190µs),**12/12 全部改善 >5%**(−54.7% ~ −84.7%),0 回归;
- 943/943 dump 位一致;936 测试通过;clippy 干净;
- 合成查询的 dump 与 main 逐字节一致(12/12)。

**结论:矩阵已无洞。** K=1+N 由原内核覆盖,K≥2+N 由 multi 内核覆盖,
纯 AND/OR 由 main 已有引擎覆盖。

**同时**:窗口批量解码(C2)可用基线 `merge_optional_freqs` 的 Compressed 路径
替换(见 `iu-framework-simplification.md` §4(d)),进一步减自研代码。

---

## 附录:验证命令与代码位置

```
# Lucene 路由
sed -n '150,184p' lucene/core/src/java/org/apache/lucene/search/BooleanScorerSupplier.java
# Lucene ReqOpt 的窗口协议
grep -n "advanceShallow\|setMinCompetitiveScore" \
  lucene/core/src/java/org/apache/lucene/search/ReqOptSumScorer.java
# Tantivy 路由
sed -n '370,500p' tantivy/src/query/boolean_query/boolean_weight.rs
sed -n '85,95p'  tantivy/src/query/reqopt_scorer.rs
# Lance 内核
sed -n '82,190p' rust/lance-index/src/scalar/inverted/wand_iu_tight.rs
# 基线机制存在性
git grep -c "score_single_essential_upto\|merge_optional_freqs" ecb388f73 -- rust/lance-index/src/
```
