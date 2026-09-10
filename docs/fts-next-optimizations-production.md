# 收核后上生产：叠在社区 FTS stack 上加什么

日期：2026-09-09  
父文档：[fts-next-optimizations.md](./fts-next-optimizations.md)（方向：少特化、按偏斜和地板走路）  
社区尺子：[fts-community-stack-baseline.md](./fts-community-stack-baseline.md)（`15d48f91e` 上 Wikipedia SBG TOP_10 / TOP_100；后面每个 PR 用这份比）  
本文只回答：社区 stack 已经占了哪几条路、我们还往上加哪几条、从哪棵树 port、收核后大概多少行。不重复 Wikipedia 全量 bench 表。

**上生产的 merge base 是社区，不是现在这棵 `perf/fts-top10-analysis`。**  
我们分支只当实现参考：从社区 checkout，把 `#9033` / `#9030` 叠上去，再把下面 6 条 port 过去。不要把本分支的 `4–5–6` Auto、2-essential SoA、`u16` 字典当要合进去的现状。

---

## 0. 社区基线（已经有的，不要再写、不要打架）

对照树：

| | SHA / 分支 |
|---|---|
| 当前工作区（社区基线） | `bench/community-fts-stack` @ `15d48f91e` |
| 社区 `origin/main`（建对照树时） | `468f6e892` |
| 实现参考 | `perf/fts-top10-analysis` @ `3d1189834` |

Wikipedia SBG 数字见 [fts-community-stack-baseline.md](./fts-community-stack-baseline.md)（A1 TOP_10 AVERAGE **3,911µs / 2.64×** Lucene）。后面每个 PR 用那份 JSON，不要和分析分支的 1.16× 横比。

社区已经合入、或本对照树已 cherry-pick 的 FTS 层。`#9033` / `#9030` 都不是 `origin/main` @ `468f6e892` 的祖先；GitHub open/merged 本文不写死。

| PR | SHA | 状态（相对 `origin/main` @ `468f6e892`） | 占哪条路 |
|---|---|---|---|
| `#7624` | 更早 | 已在 main | AND 恰好 2/3 词走 `and_bulk_search` |
| `#7629` | `f868f511e` | 已在 main | 量化分区字节 norm + 256 格 `norm_k_cache` |
| `#8749` | 更早 | 已在 main | 复合路径短语：先打分，位置放 `matches()` 里确认 |
| `#9031` | — | **已在 main** | 堆未满时推迟 AND 块解码 |
| `#9032` | — | **已在 main** | 常驻统计 / 预热 scorer |
| `#9033` | `e5658f895` | 对照树已 cherry-pick；不在上述 main | 经典 `next_and_candidate` 里的 4/5 词密窗 score-first（见下：默认叶子 Auto **走不到**） |
| `#9030` | `15d48f91e` | 对照树已 cherry-pick；不在上述 main | 4+ 现代 block-256 进**同一只** bulk；`floor == 0` 时两两交 id |

社区 `Auto` 的 `enabled_for` 仍是「恰好 2 或 3」。宽 AND 不改这张表，而是在 `Wand::search` 里多一刀：

```text
Auto && n ∈ {2, 3}                              → and_bulk_search          (#7624)
Auto && n ≥ 4 && 全是现代 block-256 + impacts   → 同一只 and_bulk_search   (#9030)
  floor == 0   → merge_window_docs_pairwise（SIMD/标量两两交 id，只给幸存者回填 offset）
  floor > 0    → 现有 bulk 打分 + lead freq LUT 剪枝
其余（含偏斜停词、非现代 4/5）                  → 经典 leapfrog（无 #9033）
```

`#9033` 的 `score_first_and_enabled` 自己就要 And + 全 compressed + `!has_grouped_terms` + 全是 block-256 + impacts（`wand.rs` 构造处）。这和 `#9030` 的 bulk 加刀是同一组谓词；score-first 还多要 `token_id` 互异、`!use_scorer_upper_bound`、`query_weight` 有限非负、`doc_norm` 可用、`threshold > 0`、密窗成立。所以它是 bulk 门的**真子集**。落点 `is_score_first` 在 `next_and_candidate`，只有 `and_bulk_search` 没接手才到得了。

因此 **`#9033` + `#9030` 同时在时，默认叶子 `Wand::search`（`LANCE_FTS_BULK_AND` 未设 = Auto）里 4/5 词 score-first 是死路**——不是「Off / 非现代块」：非现代 4/5 进不了 score-first，只走无剪枝的经典 leapfrog。叶子上要走到，只有 `LANCE_FTS_BULK_AND=off`。`with_bulk_and_mode` 是 `#[cfg(test)]`，不是产品开关。

复合 `WandCursor` 不走 `Wand::search` / bulk，4/5 现代 AND 叶子仍可能走到 `#9033`。那不是 SBG 叶子 AND 的默认路径，也不当成「社区已经占住的一条默认叶子路」来护。

社区**没有**：lead-stream、稀有词预填 floor、MaxScore 窗内 SoA / 两指针 optional、短语叶子「分不够就不对位置」、ReqOpt 升格、IU tight、精确长度（非 256 量化）的 dense `f32` addend。

`#7629` 已经给量化分区做了字节 norm + 256 格 `norm_k_cache`。第 6 条补的是 **精确长度分区**，不是再做一张量化表。

---

## 0.1 上生产怎么开工

```text
git fetch origin
git checkout main && git pull

# #9031 / #9032 已在 main，不必再 pick
# #9033 / #9030 若尚未合入：
git cherry-pick e5658f895   # #9033
git cherry-pick 15d48f91e   # #9030

# 然后从 perf/fts-top10-analysis @ 3d1189834 往上 port 第 1–4、5a、6 条
# 不要把那棵树整支 rebase / merge 过来
```

从我们分支**不要 port**：

- `BulkAndMode::enabled_for` 的 `4–5` 永不 bulk / `6+` 且最短 ≥ 50 万
- `complete_maxscore_two_essential_soa`
- `u16` 长度字典 / AVX gather 打分
- 所有 `LANCE_HACK_*`、`LANCE_DIAG_*`、`LANCE_FTS_AND_SCORE_FIRST` 当产品开关

4 词差不多长的现代 AND 走社区 bulk，是**他们的回归**。我们回归的问题是「偏斜门有没有把这条从 bulk 里抢出来」，不是再写一只 N 路 SIMD。

---

## 0.2 叠上去之后的选路

```text
社区（stack 之后）
  差不多长的宽 AND     → bulk
  floor == 0           → 两两交 id
  floor > 0            → 现有 bulk 打分

我们往上加
  偏斜很大             → 先 BMC 前移（解压前调现成 level0 界）；第二只核用窗开销证明
  follower ≫ lead      → score-first（+care +a +lot 偏斜不够，必须仍逐 doc）
  稀有 OR              → 预填 floor
  短语叶子             → 先打分再对位置（任何 slop）；slop=0 另有稀对预筛
  Boolean              → ReqOpt 升格（5a，已回滚）；IU tight（5b，真界）
  精确长度分区         → 每 partition 一张 dense f32 addend
```

---

## 0.3 上生产清单（后面 §1–§6 的编号对这张表）

**上生产的是下面 6 条，而且是收核之后的形态，不是现在树上每一条 `if`。**  
第 7 条（DataFusion 规划税 / 跳过 optimizer）这轮不做，见父文档 3.5。

BM25 公式和 dump 保持 bit-identical。

| # | 条 | 上生产？ | 主要文件 | 叠在新基线上大约 | 社区 stack 有没有 |
|---|---|---|---|---:|---|
| 1 | 稀有词预填 floor | 是 | `wand.rs` | ~130 | 无 |
| 2 | 偏斜 AND（代价比出门 + lead-stream） | **已量** | `wand_lead_stream.rs` | ~400 | `#9030` 无偏斜门；无 lead-stream |
| 3 | MaxScore 一只窗 | 是（删 2-ess） | `wand.rs` | ~550–700 | 有 `maxscore_search`，无 SoA / 两指针 |
| 4 | 短语叶子先打分再对位置 | 是 | `wand.rs` | ~200 | `#8749` 只盖复合 `WandCursor`；叶子 / lead-stream 仍先对位置 |
| 5a | ReqOpt 升格 | 是 | `compound.rs` | ~150 | 有 `ReqOptScorer` |
| 5b | IU tight | **已量** | `wand_iu_tight.rs` + `compound.rs` | ~350 | 无 |
| 6 | 精确长度 dense `f32` addend | 是 | `documents.rs` | ~80 | 有 256 格量化 cache，无精确长度 slab |
| 7 | 规划税 | **这轮不做** | — | — | — |

1–4 + 5a + 6 收核后大约 **+1,600–1,800 行生产**（另加测试）。第 2 条 BMC 前移已回归；lead-stream 已按 32× `max/min` 落地并量过（§2），不是分析树的 4–5–6 词数表。

这是叠在 **main + `#9033` + `#9030`** 上的增量，不是相对旧 `#7624`-only main。现在整棵 `perf/fts-top10-analysis` 倒排相对旧 main 约 +6,500 行（含测试和死路）；差出来的大半是 2-ess、词数表、IU hack、字典、诊断，**不要**当生产 diff。

**清单按「合进社区的核」排。PR 顺序同时看补丁是否简单/基础，以及它自己那类查询上能不能看见效果。** 父文档 §6.4：Lucene ≥1.5ms 的 222 条上 A1 已是 1.15×；剩下的是便宜查询上 ~250μs 产品路径地板，第 7 条这轮不做。叶子核能合进社区，产品税是另一条线——reviewer 拿 §6.4 质问这张表时，答案就是这句话。

落地：社区 `wand.rs` 已约 10.3k 行。`rust/AGENTS.md` 要求大块新逻辑抽子模块（`#9030` 自己就新建了 `wand_intersection.rs`）。1/2/3/4/5b 不要继续堆进同一个万行文件。

**5b 已量。** 第 2 条 BMC 前移已回归；lead-stream **已量**（见 §2）。回归范围：偏斜路径**有没有把社区的宽 AND bulk 抢没**。

### 代码依赖

**没有硬依赖**：任何一条单独合进去都正确、dump 仍 bit-identical。没有「必须先落地 A，B 才能编译 / 才能剪枝」的类型或 API 链。

会碰到的只有两种软耦合：

```text
5a  compound.rs          与 1–4、6 无共享代码

2 BMC ──同一 and_bulk_search 循环里先后── 4 短语分后位置
        语义可叠加（先跳块，再对幸存者打分，分不够才对位置）
        互不需要对方的新函数

1 seed ──同一 maxscore_search、同一 threshold ── 3 MaxScore 窗
        seed 只在开搜写一次地板；窗循环读它跳块
        谁先合都能跑；后合的那条改的是「已经有 / 还没有早地板」的窗

6 documents.rs 查表
        所有打分循环都可以换成 array index，正确性不依赖 6
        3 的 SoA 缓冲可以先现算 addend，6 再改成查表
```

2 的偏斜门若做成「把偏斜 AND 赶出 bulk」，才会和社区 `#9030` 选路打架。BMC 前移本身不改选路。

### 各条吃哪类查询（用这个量化，不要看 AVERAGE）

Wikipedia SBG 索引是 `block_size=128`、`quantized_scoring=false`（引擎 `details.json`）。社区尺子见 [fts-community-stack-baseline.md](./fts-community-stack-baseline.md)。分析分支上的 A/B **只用来认场景和数量级**，不是这条 PR 合进社区之后的承诺。

| 条 | 细分场景 | 社区尺子上对哪一格 | 机制 | 量化时看什么 |
|---|---|---|---|---|
| **5a** | Boolean：MUST 整棵全局上界过不了堆 floor → optional **并集**变成必有，之后 `MUST ∩ (SHOULD_1 ∨ …)` | `intersection_union` 40 条，TOP_10 A1 **26.8ms / 8.53×** | `compound.rs` 升格；不是和每个 SHOULD 求交 | **IU AVERAGE** 和 `+public transit` / `+data privacy`。分析树上仅升格大约 −12%；8.53×→0.95× 主要是 **5b**。这条 PR 若 IU 几乎不动，就是 5b 的证据，不是 5a 失败 |
| **2** | 偏斜 AND：最短 lead + 停词 follower。社区 Auto 无条件 bulk 2/3，Wikipedia `block_size=128` 又走不到 `#9030` | named `+walk +the +line`；同类 `+time +for +kids`、`+university +of +washington`。**不是**均衡 2/3（那是社区 bulk 回归） | Auto：`max/min >= 32` 且 n≥3 走 lead-stream；n=2 偏斜仍 leapfrog；`second >= 3×lead` 时先打稀有分再 seek | **已量、已落地**（见 §2）。walk **−66.2%**；intersection 300 **−23.5%**。回归：ratio 31 的 3-AND 仍 bulk；`#9030` 均衡现代 4+ 仍 bulk |
| **4** | 短语：词已经齐了，但 term-BM25 进不了堆 → 不必对位置。Lance 短语分=词频 BM25，所以这是精确分不是上界。slop=0 再按当前文档词频懒解码位置：稀有词对不上就不解停词 | `phrase` 300 条，TOP_10 A1 1.94× | 叶子 / bulk 先 `exclusive_score_cannot_beat_floor` 再 `check_positions`。`#8749` 只盖了复合路径。精确短语走 `exact_phrase_positions_match` | **已量、已落地**（见 §4）。phrase 300 条相对 Item 6 **−18.0%**，再相对 Item 5b **−8.3%**（3.22ms → 2.95ms）；`"the book of life"` **−31.3%** |
| **1** | 稀有 OR：一条短 posting 自己就能撑起 top-k，但堆从 0 爬，等它填满时 `high`/`school` 已经扫完 | union 里的离群点：`niceville high school` 12.5ms / 19×；同类 `kasota stone`、`the incredibles`。**不是** `cheap hotels`（两条都长，seed 门拒绝） | 开搜只扫最短列表，第 k 大减 1 ULP 当天花板。AND 不合法，只挂 `maxscore_search` | **触发的那十几条**（分析树 12/943），不要看 union 301 均值。对照：`the movement` 不得变慢（2048 门拒掉） |
| **3** | 均衡 OR：词差不多长，seed 进不去；贵在窗里打分 / optional 补 freq / 块上界不够仍没整块 skip | union 301 条 2.21×；named `cheap hotels` 3.51×、`chicago teachers union` 3.13× | 一只窗：SoA、两指针 optional、块 skip、top2-gap。不 port 2-ess | **已量、已落地**（见 §3）。union 301 条 **−10.6%**（2.03ms → 1.81ms）；`cheap hotels` **−34.8%**、`chicago teachers union` **−37.4%**。niceville 不算这条功劳 |
| **6** | 精确长度分区上，每打一篇都现算 `bm25_doc_norm(tokens, avgdl)`。量化 256 格已有 `#7629`，这条补 128-block | **所有要打分的查询**（本机 SBG 正是 128-block） | `DocLengths` 上 `[f32; n]`，同一表达式烤一次 | 分析树在 seed 之后 union **−9.6%**，AVERAGE −42µs 且对照反向漂。**薄、铺在 943 条上**。不要当第一把刀的主证据 |

第 6 条横切、薄、AVERAGE 读不出来，所以不能当第一把刀——否则 walk / union 的 delta 说不清。它仍然比第 4 条更基础（所有打分都吃表），比第 3 条简单，所以放在两把高信号刀之后、大改窗之前。

### 社区 PR 顺序（简单/基础 × 场景效果）

一条 PR 只带一只核。每个 PR 的尺子是**上一格的 JSON**，只报上表里那一列，不报 TOP_10 AVERAGE，除非那一列本来就是 AVERAGE（union / phrase / IU 标签）。大块新逻辑抽子模块，不要继续堆 `wand.rs`。

两轴怎么合成：先做「补丁小、API 现成、自己那类查询上效果尖」的；横切但薄的缓存让路；最大的窗改写放最后，此时地板和 addend 都在。

| 顺序 | 条 | 大约行数 | 简单 / 基础 | 场景效果 | 为什么是这个位置 |
|---|---|---:|---|---|---|
| 1 | **2** BMC 前移 | 几十 | 现成 `level0_doc_weight_bounds_cached`，只是解压前调用 | `+walk +the +line` 社区 10.2ms / 6.13× | **已量、已回滚**（见 §2）。不要同 PR 带 BMC 与 lead-stream |
| 2 | **1** 稀有词 seed（先留 2048） | ~130 | 一个函数 + `maxscore_search` 开搜调一次 | `niceville` 社区 12.5ms / 19.1× → **0.55ms / 0.84×**；`the incredibles` 38.7ms / 45.5× → **0.97ms / 1.14×** | **已量、已落地**（见 §1）。只报那十几条离群 OR；`the movement` 未变慢 |
| 3 | **5a** ReqOpt 升格 | ~150 | 只动 `compound.rs` | IU 40 条 8.53× 是最差标签；分析树仅升格大约 −12%，8.53×→0.95× 是 5b | **已量、已回滚**（见 §5a）。社区 IU AVERAGE **+5.8%**，0 条快 10% 以上、9 条慢 10% 以上。不要同 PR 带 5b |
| 4 | **6** dense `f32` addend | ~80 | 只动 `documents.rs` + 打分热路径查表 | 薄：分析树 union −9.6%，AVERAGE 读不出来 | **已量、已落地**（见 §6）。社区 union 301 条 **−11.3%**（2.27ms → 2.02ms）。不要拿 TOP_10 AVERAGE 当主证据 |
| 5 | **4** 先打分再对位置（任何 slop）；slop=0 按词频懒解码 | ~80–120 + ~80 | 小；叶子三处 + bulk 改序；精确短语共用一只 scan | phrase 1.94×；分析树 named **几乎噪声** | **已量、已落地**（见 §4）。社区 phrase 300 条相对 Item 6 **−18.0%**，再相对 Item 5b **−8.3%**。不是分析树的 lead-stream 稀对预筛 |
| 6 | **3** MaxScore 一只窗 | ~550–700 | 最大一块 | union 301 条 2.21×，`cheap hotels` / chicago；**量**最大 | **已量、已落地**（见 §3）。社区 union 301 条 **−10.6%**；`cheap hotels` **−34.8%**、chicago **−37.4%**。不 port 2-ess |

暂缓：seed 的 Impact 停止判据、第 7 条规划税。**5b IU tight 已量**（见 §5b）。**2 的 lead-stream 已量**（见 §2）。

---

## 1. 稀有词预填 floor

OR 在堆还没满时，先只算最短那条 posting 自己能打多少，取出第 k 大再减 1 ULP，当作整场 exclusive 门槛。`niceville` 81 篇就能让 `high` / `school` 一开始按分数跳。

界：OR 下任何 doc 的总分 ≥ 它在任一 clause 上的分量，所以最稀列表上单 clause 分数的第 k 大，是真实第 k 大总分的合法下界；exclusive 再减 1 ULP 保住并列（堆还空，卡在恰好第 k 名会把并列全剪掉）。**这个性质对 AND 不成立**——最稀列表里的 doc 未必过交。只挂在 `maxscore_search` 上，不要「顺手」推广到 AND。

社区没有这段。floor 只来自本堆装满，或兄弟 partition 的 atomic 第 k 名。`#9031` 推迟的是 AND 未满堆时的解码，不是 OR 预填。

| | 位置 | 大约行数 |
|---|---|---:|
| 门 | `SEED_FLOOR_MAX_POSTINGS=2048`、`MAX_COST_SHARE=16` | 15 |
| 核 | `seed_floor_from_sparsest_clause` | 110 |
| 挂接 | `maxscore_search` 开搜前调一次 | 10 |
| 测试 | 含「`the` 太长不预填」 | ~80 |

上生产先按现在的帽子落地。2048 / 16 没有物理理由，只是还没有更好的东西顶上：`the movement` 用 11.8 万篇预填会亏 2.4ms。过不了门就当没这刀。

**已量（2026-09-09，社区尺子 A1 TOP_10）。** 过程与 JSON 在 `.agent/fts-item1-seed/`。hit count 943/943 一致。

| query | 社区 µs | seed µs | rel | ×Lucene |
|---|---:|---:|---:|---|
| `niceville high school` | 12,515 | **552** | **−95.6%** | 19.14× → **0.84×** |
| `the incredibles` | 38,685 | **970** | **−97.5%** | 45.46× → **1.14×** |
| `kasota stone` | 4,194 | 661 | −84.2% | 16.26× → 2.56× |
| `the movement`（2048 门拒） | 1,465 | 1,480 | +1.0% | 不得变慢，未变 |
| `cheap hotels`（均衡不触发） | 1,551 | 1,479 | −4.6% | 噪声 |

union 去掉 >30% 变快的 31 条后，剩下 270 条 +1.2%。intersection / phrase 不动。种子只计可见文档；空堆时不乘 `wand_factor`、不写 shared floor。

`MAX_COST_SHARE` 的分母是全查询 cost，恰恰被想跳过的停词主导——最诱人的时候分母涨得最快，换任何常数都治不好。`Σcost/16` 在维基上往往 **大于** 11.8 万，预算不 binding，仍会打完 `movement`。

下一刀的停止判据应写成**界**，不是 cost 比例。Impact 上已有、不用解压：

- `global_max_doc_weight_cached`（`impact.rs` 207）：`the` 的全局最大 doc weight
- `level0_doc_weight_bounds_cached`（217）：每块最大 doc weight

分小块增量 seed，每块之后拿当前第 k 大和 `query_weight(the) × the 的全局/块上界` 比：越过就停（后面白打），小预算用完还远低于它就放弃（这个门槛永远跳不动 `the` 的任何一块）。`niceville` 81 篇第一小块就越过；`the movement` 在小预算处放弃，而不是打完 11.8 万。在这之前 2048 仍当安全阀。生产约 **130 行**。不改磁盘、不改公式。

---

## 2. 偏斜 AND：先做 BMC 前移，第二只核要用窗开销论证

最短当 lead。偏斜时不该在 `floor > 0` 还解压已经跳不动的停词块。

**「lead-stream 只 seek、不解压 `the`」在代码里不成立。** `CompressedState` 只缓存一块，换块就重解。经典 `next` / `next_doc_id` 和 bulk 落进哪块，都走同一个 `ensure_compressed_doc_ids_ptr`（`wand.rs` 898）。lead 按 cost 升序，窗里 walk 没有 doc 就 `break`（4209–4232），空隙里的 `the` **不会**被摊开。bulk 窗口又锚在各块边界的交上，所以 `floor == 0` 时两条路解压的 `the` **doc_id 块是同一批、同样多**。`#9031` 已经把这段能省的都省了：先交 id，有正门槛才解码 freq、建分数界；交集不足 k、floor 永远起不来的最坏情况也点过名。

`floor == 0` 还差的是每窗固定开销：bulk 每窗 `block_idx_for_doc`、两次 `partition_point`、建 `wins`、批处理簿记（4185–4241）；lead-stream 按 lead doc 循环 seek。这是常数因子，不是解压量。它可能是真的，但指向的动作是**把窗口路径削薄**，不是另写 450–500 行第二只核。真要论证第二只核，拿「每窗固定开销 × 窗数」的 profile，不要拿解压量。

TOP_10 下 floor = 0 只持续到第 10 个三词齐全的文档。`walk` 约 3.5 万篇，交集哪怕几千篇，第 10 个匹配也在极早期。所以「floor = 0 时 BMC 跳不动」是硬的，覆盖的大概是千分之几的工作量；少于 k 个匹配的尾巴 `#9031` 已经处理。

社区没做、第 2 条该打的点：

- 社区 `Auto` 的 `enabled_for` 是光秃秃的 `matches!(num_clauses, 2 | 3)`（`wand.rs` 274），**没有任何偏斜条件**。`+walk +the +line` 在社区无条件进 bulk。分析分支上的 `max/min < 32` 在社区不存在。因此偏斜门必须从 **n = 2** 起覆盖，不是只补 `#9030` 开的 n ≥ 4。`#9030` 另外让「1 稀 + 3 停词」这种 4 词偏斜也进 bulk。
- `others_block_max` 写在所有 clause 的 doc id 都解压之后（4249）。`level0_doc_weight_bounds_cached`（`impact.rs` 217，注释即 Lucene `getSkipUpTo` 的 slab）已经在 `maxscore_search` 里用了（`wand.rs` 2867–2896，同样门在 `threshold > 0`）。AND bulk 只是没在解压前调它。API 和缓存都是现成的，比原先估的还便宜。

**已量（2026-09-09，社区尺子 A1 TOP_10）。** 过程与 JSON 在 `.agent/fts-item2-bmc/`。

`+walk +the +line` 10,214µs → **11,304µs**（+10.7%，6.13× → 6.79× Lucene）。intersection 300 条平均 +6.1%；15 条 AND 慢 20% 以上，0 条快 10% 以上。hit count 一致。整窗 BMC 与现成 `and_advance_target` 重复；`the` 的 block-max 在堆满后仍经常过得了 floor，lead-LUT 剪不掉 follower，lead-first 反而给活窗加了解压。

**这刀已从 `wand.rs` 撤掉，不提交。** BMC 前移不是 lead-stream 的证据。

**已量（2026-09-10，社区尺子 A1 TOP_10，对照 phrase lazy-decode）。** 过程与 JSON 在 `.agent/fts-skew-and-lead-stream/`。hit count 943/943 一致。独立 review：无 P0/P1。

Community Auto 对 2/3 无偏斜门，Wikipedia 又是 block-128（`#9030` 4+ bulk 从不开火），所以偏斜 3 词停在 bulk 窗开销上。这一刀只加代价比：`min_cost > 0 && max_cost / min_cost >= 32`。均衡 2/3 和现代 4+ 仍进社区 bulk；偏斜 n=2 仍 leapfrog；偏斜 n≥3 走 `and_lead_stream_search`（`wand_lead_stream.rs`）。窗内 `second >= 3×lead` 时先打稀有 BM25 再 seek follower。短语确认仍是现成的分后位置 + `exact_phrase_positions_match`，没有 `check_exact_phrase_pair`，没有词数表。

| query / 标签 | 上一刀 µs | 本刀 µs | rel | ×Lucene |
|---|---:|---:|---:|---|
| `+walk +the +line` | 10,367 | **3,503** | **−66.2%** | 2.30× |
| `+time +for +kids` | 8,375 | **2,472** | **−70.5%** | 2.12× |
| `+university +of +washington` | 12,812 | **5,089** | **−60.3%** | 1.87× |
| `+to +be +or +not +to +be` | 78,134 | 77,422 | −0.9% | 3.75×（噪声） |
| intersection 300 | 2,180 | **1,667** | **−23.5%** | 2.02× |
| `+care +a +lot` | 9,912 | 5,540 | −44.1% | 2.12×（维基上已 ≥32×，进 lead-stream；单测 ratio 31 仍 bulk） |
| phrase 300（副作用） | 2,953 | 2,541 | −14.0% | `"walk the line"` −56.2% |
| union 301 / IU 40 | | | −3.7% / −8.3% | 对照，非本刀场景 |

`On` 仍强制 bulk（parity）。均衡 `#9030` 现代 4+ 仍 bulk。Hamlet AND 几乎不动；不要拿它当主证据。

落地收成（相对分析树）：

| | 分析树（不要 port） | 本刀 |
|---|---|---|
| 选核 | 2–3 看 32×；4–5 永不 bulk；6+ 且最短 ≥50 万又 bulk | **只看 32× `max/min`**。均衡 2/3 和 4+ 现代块留给社区 bulk |
| 核 | `and_lead_stream_search` + 单独 `score_first_block` | 一只循环；`second >= 3×lead` 时先打稀有分 |
| 2 词偏斜 | leapfrog | 保持 |
| 4 词差不多长、现代块 | 分析树走 lead-stream（错） | **社区 `#9030` bulk** |

**不要改写**社区的 pairwise / `wand_intersection.rs`。2/3 路手写交仍是「差不多长」时的实现细节。

回归只守社区 bulk（2/3 均衡 + `#9030` 宽均衡），**没有额外的 `#9033` 回归**：

- 2/3/4/5/6 词、df 差不多、现代块：必须仍进社区 bulk
- 偏斜停词：lead-stream 由稀有 list 开车，follower 只 seek
- `+care +a +lot`：ratio < 32 必须仍 bulk；维基这条已经 ≥32×，进 lead-stream 是门的结果，不是词数特判

---

## 3. OR MaxScore：一只窗

社区已经有 MAXSCORE「弱词坐车」（`maxscore_search`）。我们把窗里的活做完：开车的块上界不够就整块 skip；坐车词两指针对 freq；两个开车的离得远时先把前一段当单 essential 做完（top2-gap，必须用 `top2 - top`）；打分用 SoA 缓冲。

核在 `wand_maxscore.rs`，`maxscore_search` 仍在 `wand.rs`：

| | 现在本分支 | 上生产 |
|---|---|---|
| `complete_maxscore_single_essential_soa` | ~363 | **留下，变成唯一收尾** |
| `merge_optional_freqs` | ~103 | 留下 |
| `skip_current_block` / `take_docs_one_block_upto` | ~80 | 留下 |
| top2-gap（`maxscore_search` 里） | ~40 | 留下；`top2.saturating_sub(top) >= INNER_WINDOW/2` |
| `maxscore_search` 比社区多 | ~240（含分叉） | 分叉收掉后大约 +150 |
| `complete_maxscore_two_essential_soa` | ~325 | **删，不要 port** |
| 非 SoA `complete_maxscore_single_essential` | ~259 | 能并就并，否则只留兜底 |

抽到 `wand_maxscore.rs`。skip 不得越过本窗 `upto`（下一窗的 optional remainder 可能更大）。>2 个 optional 仍走社区 `consider_candidate`。`union_buf` 就是默认 SoA，没有 `LANCE_HACK_UNION_BUF`。不要为 chicago 再开 2-ess 入口。

**已量（2026-09-10，社区尺子 A1 TOP_10，对照 Item 4 phrase）。** 过程与 JSON 在 `.agent/fts-item3-maxscore/`。hit count 943/943 一致。

union 301 条平均 2,029µs → **1,813µs**（**−10.6%**）。`cheap hotels` 1,591µs → **1,037µs**（**−34.8%**）；`chicago teachers union` 6,033µs → **3,776µs**（**−37.4%**）。intersection / phrase 均值 +0.3%。`niceville high school` +1.8%（seed 已吃掉，不算这条）。

---

## 4. 短语：叶子上先问分，再问位置

Lance 短语分数 = 各词 BM25 按**词频**求和，位置只做 gate（社区 bulk 打分回路 4479–4491）。所以「先打分」拿到的是**精确分**，不是上界，剪枝无损。Lucene `PhraseQuery` 用短语频率打分，不解位置就没有分数——这是 Lance 结构上的便宜，不是 ~200 行挂件。

两件独立的事，不要写成「整条只对 slop=0 有效」：

| | 对谁成立 | 做什么 |
|---|---|---|
| 分不够就不解位置 | **任何 slop**（分数与 slop 无关） | `exclusive_score_cannot_beat_floor` |
| 先对最稀一对 | **仅 slop=0** | `check_exact_phrase_pair`；对不上不 seek 停词 |

社区已经有 `check_positions` / `check_exact_positions_bulk`，以及 `#8749`（复合 `WandCursor` 先打分，位置放 `matches()`）。本对照树叶子经典回路和 bulk 窗已改成先打完整分，分进不了 exclusive 堆就不解位置。`WandCursor` 仍走同一只 `check_positions`。slop=0 的位置确认按当前文档词频懒解码（`exact_phrase_positions_match`）：稀有词对不上就不解停词。这不是分析树挂在 lead-stream 上的 `check_exact_phrase_pair`。

| | 文件 | 大约行数 |
|---|---|---:|
| `exclusive_score_cannot_beat_floor` + 叶子三处调用 | `wand.rs` | ~80 |
| bulk 窗里先算完整分再 `check_positions` | `and_bulk_search`（改顺序） | ~40 |
| `exact_phrase_positions_match`（slop=0 词频序懒解码） | `wand.rs` | ~80 |

生产分数合同与社区一致：完整分是堆要比的那个 query-order f32，用 `accepts_score`，堆未满（`threshold == 0`）不解。

**已量（2026-09-10，社区尺子 A1 TOP_10，对照 Item 6 addend）。** 过程与 JSON 在 `.agent/fts-item4-phrase/`。hit count 943/943 一致。

phrase 300 条平均 3,960µs → **3,247µs**（**−18.0%**）。`"the book of life"` 88,598µs → **69,747µs**（**−21.3%**）；`"to be or not to be"` −37.6%、`"american south"` −18.7%。union / intersection 均值 +0.6%（噪声）。

**已量（2026-09-10，社区尺子 A1 TOP_10，对照 Item 5b）。** 过程与 JSON 在 `.agent/fts-phrase-lazy-decode/`。hit count 943/943 一致。

phrase 300 条平均 3,221µs → **2,953µs**（**−8.3%**）。3 词 −6.4%、4 词 −24.5%、6 词 −16.1%；2 词 +1.8%（两条都要解）。`"the book of life"` 69,593µs → **47,778µs**（**−31.3%**，0.98× Lucene）；`"to be or not to be"` −16.3%；`"the news journal"` −21.1%。union / intersection / IU 均值 +0.8%～+1.6%（噪声）。

---

## 5. Boolean：MUST 不够时，SHOULD 变成必有

社区已经有 `ReqOptScorer`。我们多了两层，胖瘦差很多。`#9030` **不含** Boolean MUST 地板传播（那是另一条 `#9015`），不要指望宽 AND 补丁顺便改 Boolean。

### 5a. 升格（建议上生产）

`ReqOptScorer` 只有两个孩子：`required`，以及一侧 **optional 子树**（N 个 SHOULD 时通常是它们的**并**，`ShouldMaxScore`）。`promote_optional_if_required` 用的是 `required.global_score_upper_bound()` 对 `min_competitive_score`（只升不降）。MUST 全局上界过不了堆 floor → optional **子树**变成必有，之后 `MUST ∩ (SHOULD_1 ∨ … ∨ SHOULD_n)`，不是 `MUST ∩ 每个 SHOULD`。照字面做成「和每个 SHOULD 求交」会静默丢结果。

代码注释已经写了「floor 只升，所以这个决定是永久的」。清掉 window floor 是因为升格之后走 `position_intersection`，会 seek 出当初算窗 floor 的 shallow 区间；**不是**用窗 floor 证的升格。用窗 floor 证出来的升格不能当永久。单调全局 floor 才是。

- 文件：`compound.rs` `ReqOptScorer`
- 量：大约 **80–120 行**
- 风险低，Boolean 默认路径就能用。这是 Lucene `ReqOptSumScorer` 在 `minCompetitiveScore > maxScore(required)` 时的行为。

**已量（2026-09-09，社区尺子 A1 TOP_10，对照 Item 1 seed）。** 过程与 JSON 在 `.agent/fts-item5a-reqopt/`。hit count 943/943 一致。

IU 40 条平均 27,377µs → **28,978µs**（**+5.8%**，9.45× → 9.92× Lucene）。`+public transit` −5.9%、`+data privacy` −3.4%，都在噪声里；9 条长 IU 慢 10% 以上（最差 `+health care cost trends` +19.2%），0 条快 10% 以上。intersection / union / phrase 均值不动。

升格本身 recall 正确（只用 MUST 列表级上界，不用窗界；optional 按并集升）。墙钟回归是因为升格之后 `position()` 不再走窗内 `combined.upper` 整窗跳，改成对每个交集候选 leapfrog；社区 `ReqOptScorer` 已经有窗内临时 intersect，再切永久交集在长 IU 上更贵。分析树 −12% 含那棵树上其它 ReqOpt / 窗收集改动，不能当成这一刀叠进社区之后的承诺。

**这刀已 revert**，现场留在 git 历史上。5b 不是从这次回归开的；剩余毫秒级 2× Lucene 缺口里 IU 仍占六成，才单独上真界。

### 5b. IU tight（真界，已量）

1 个 MUST term + N 个 SHOULD term 时，不走 ReqOpt 迭代器，改走 `iu_tight_search`：MUST 开车，列表级上界证明某个 SHOULD_i 单独也必有（`max(MUST) + Σ_{j≠i} max(SHOULD_j) < floor`）之后，把最便宜的那条升成求交 lead。入口和升格共用相对代价门 `lead * 3 < MUST`，没有 85k df 常数，没有 `LANCE_HACK_*`。不把 optional **并集**升成必有（那是 5a）。

| | 位置 | 大约行数 |
|---|---|---:|
| `iu_tight_search` / 真界 / promote | `wand_iu_tight.rs` | ~300 |
| 选路 + 相对代价门 | `compound.rs` | ~80 |
| `range_score_upper_bound` / `collect_must_driven` | — | **不上** |
| `LANCE_HACK_IU_*` / `IU_TIGHT_MIN_MUST_COST = 85_000` | — | **不上** |

**已量（2026-09-10，社区尺子 A1 TOP_10，对照 Item 3 MaxScore）。** 过程与 JSON 在 `.agent/fts-item5b-iu-tight/`。hit count 943/943 一致。

IU 40 条平均 28,559µs → **11,467µs**（**−59.8%**，9.94× → 5.91× Lucene）。`customer +service phone number` 97ms → **4.9ms**（−95%，0.44× Lucene）；`small +business grants` −96.6%。IU >10% 更快：**20**；>10% 更慢：**0**。intersection / union / phrase 均值 −1.1%。TOP_10 AVERAGE 3,585µs → **2,836µs**（−20.9%）。

没吃到的 IU（`+water quality report` 等）是 MUST 相对最稀 SHOULD 不够 3×、或没有单独必有的 SHOULD，仍走 ReqOpt。不要为这些再加绝对 df 常数。剩余毫秒级 2× 里 Hamlet AND 仍贵；`+university +of +washington` 已随 lead-stream 下来。

---

## 6. 长度 addend：精确分区 dense `f32`（已定）

128-block / 非量化分区没有 256 格量化表。`bm25_doc_norm(tokens, avgdl) = K1*(1 - B + B*tokens/avgdl)`（`scorer.rs`）是 `(tokens, avgdl)` 的纯 `f32` 函数；`doc_weight_cache_key()` 就是 `avgdl.to_bits()`。按这个 `u64` 缓存在 partition 的 `DocLengths` 上，再读是**同一表达式的同一次求值**，不是近似 bit-identical。社区精确分区现在每 doc `scoring_num_tokens` 再算一遍（`and_bulk_search` 约 4399 行）。

社区已经给 **量化** 分区做了 `scoring_norms()` + `norm_k_cache`（`#7629`）。第 6 条只补 **精确长度** 分区。

- **要**：`[f32; num_docs]`，`get(doc)` 就是 `values[doc]`，`OnceLock` 挂在 `DocLengths` 上
- **不要**：`u16` 码 + 小字典、AVX gather 打分路径（关键路径多一次相关载入，换 2 字节/doc；1M doc 才 4MB。本分支测过没墙钟 / 轻微回退，不要 port）
- **不改**磁盘索引、不改公式

约 **80 行**，`documents.rs`；打分热路径（MAXSCORE / bulk AND / seed floor）改成查表。

**已量（2026-09-09，社区尺子 A1 TOP_10，对照 Item 1 seed）。** 过程与 JSON 在 `.agent/fts-item6-addend/`。hit count 943/943 一致。

union 301 条平均 2,274µs → **2,018µs**（**−11.3%**）。`cheap hotels` −3.7%、`chicago teachers union` −4.0%。不要拿 TOP_10 AVERAGE 当主证据。

---

## 和父文档怎么对读

| 想看什么 | 文档 |
|---|---|
| 为什么少特化、下一刀顺序 | [fts-next-optimizations.md](./fts-next-optimizations.md) |
| Auto 不该按 2/3/4/5/6 切；社区 `#9030` 占了宽 AND | [fts-and-auto-n-buckets.md](./fts-and-auto-n-buckets.md) |
| 社区 stack 已有什么、我们 port 什么、行数 | 本文 |
| 社区基线 Wikipedia TOP_10 / TOP_100 | [fts-community-stack-baseline.md](./fts-community-stack-baseline.md) |
| 分析分支 Wikipedia TOP_10 / TOP_100 | 父文档第 6 节 |
