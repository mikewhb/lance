# 打分 AND：≥3 词默认短的开车（不改存储对齐 Lucene / Tantivy）

日期：2026-09-11  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `a489e15a7`（`perf(fts): use posting cursors for compound 1-term leaves`）  
对照：同会话官方 LTO HEAD。现成 `.agent/fts-compound-term-leaf/results.json` 就是 `a489e15a7` 那次；开干时若 `/tmp/sbg-community/release/do_query` 已被别的工作覆盖，**先重跑 HEAD** 再打本刀。  
Lucene / Tantivy 源码：`../lucene`、`../tantivy`（只当选核对照，不 port 内核、不改磁盘）。  
相关：[fts-and-auto-n-buckets.md](./fts-and-auto-n-buckets.md)、[fts-and-lead-stream-review.md](./fts-and-lead-stream-review.md)、[fts-compound-term-leaf.md](./fts-compound-term-leaf.md)、[fts-next-optimizations-production.md](./fts-next-optimizations-production.md)

本文是实现契约。**开干前不要编译、不要跑 SBG**（机器上另有占 CPU 的活）。

---

## 一句话

不改 posting 存储。打分 AND 向 Lucene `BlockMaxConjunctionBulkScorer` / Tantivy `block_wand_intersection` 对齐：**≥3 词默认短的开车**（窗跟 lead 自己的块）。一捆一对只留给已经量过的 FOR 例外（均衡 2/3、256 捆宽 AND）。**不要**给维基 128 捆 4+ 开一对。

---

## 为什么（对齐的是选核，不是 COUNT 密交）

Lucene TOP_SCORES、Tantivy TOP_K：最短当 lead，圈数跟最短列表走。bitset / N 路按块交只出现在 **COUNT**，门是最短 ≥ 全集 1/32，**不是**词数、也不是列表彼此长短比。

Lance 现在：

```text
不偏斜 且 (2|3 词 或 256 宽) → 一捆一对
偏斜 且 ≥3 词                 → 短的开车
其余（维基 128 捆、4+、不偏斜） → 一篇一篇问   ← Hamlet 在这
```

Hamlet 强制一对曾 −32%，但仍是 COUNT 密交那条路；4+ 全开一对 32/38 变慢。Lucene 的 Hamlet 快，是 **最短那份批处理 + 堆满后 score-first**，不是 N 路按最密块切窗。

本刀只改 Auto 里「谁接手 leapfrog 剩下的 ≥3 词」。一对门不动。

---

## 做 / 不做

### 做（本刀全部）

1. `Wand::search` Auto：`num_clauses >= 3` 且没被一对接走 → `and_lead_stream_search`。删掉这条上的 `is_skewed &&`。
2. 更新 `wand.rs` / `wand_lead_stream.rs` 注释：短的开车是 ≥3 词打分 AND 的默认，不再写成「只给偏斜」。`AND_SKEW_RATIO` **仍只**用来让偏斜 2/3 离开一对。
3. 单测：128 捆、长短接近、n=4 和 n=6，Auto 走 lead-stream、`On` 仍 bulk、三模式 hit/floor 与 classic 一致。
4. dump 943 相对刀前 HEAD bit-identical；官方 LTO 通过才留 commit。

改完后的门：

```text
不偏斜 且 (2|3 词 或 256 宽 + impacts) → 一捆一对     // 不动
≥3 词                                   → 短的开车     // 本刀：去掉必须 ≥32 倍
其余（2 词偏斜）                         → 一篇一篇问   // 不动
```

真正换核的只有：**维基 128 捆、≥4 词、不到 32 倍**（Hamlet、`+the +book +of +life`、`+los +angeles +daily +news` 等约 19 条 AND，以及同形状短语）。`walk` 本来就开车。均衡 2/3 仍先被一对接走。

落点（约 `wand.rs` 2676）：

```rust
// 现在
if mode == BulkAndMode::Auto && is_skewed && num_clauses >= 3 { lead-stream }

// 本刀
if mode == BulkAndMode::Auto && num_clauses >= 3 { lead-stream }
```

一对分支仍在前面，`use_bulk` 谓词一行不改。

### 不做

- 改磁盘 / UNARY bitset / `intoBitSet` / 换块长。
- 给 128 捆 n≥4 开一对；`LANCE_FTS_BULK_AND=on` 当产品默认。
- 分析树 `n>=6 && min_cost>=500_000`、按词数分桶。
- 撤均衡 2/3 一对、撤 `#9030` 256 宽一对。
- 2 词偏斜改开车（缓冲摊不回来；`auto_keeps_classic_leapfrog_for_skewed_pair` 保持）。
- 本刀改 `AND_SCORE_FIRST_COST_RATIO`（Lucene 堆满后总是 score-first；那是下一刀，Hamlet 几条差不多长时 3 倍门打不开）。
- Widen 5b / 85k、floor 写入 MUST、`boost==1` 剥 Scale、5a promote、ReqOpt skip。

一对吃不掉 Hamlet 时，**例外**「最短也很长且彼此接近 → 一对」另开一刀，不在本刀加回去。

---

## 代码与测试落点

| 项 | 位置 |
|---|---|
| 调度 | `wand.rs` `Wand::search` Auto lead-stream 条件；文件头 `BulkAndMode` 注释 |
| 模块文档 | `wand_lead_stream.rs` 头注释（仍用 32 倍离开一对，默认核不再写「只偏斜」） |
| 保持 | `enabled_for` 仍 `2 \| 3`；`auto_wide_modern` 仍 256+impacts；`AND_SKEW_RATIO`；score-first 3× |
| 现有测 | `auto_keeps_bulk_when_cost_ratio_is_just_below_skew`（n=3 31× 仍一对） |
| 现有测 | `auto_uses_lead_stream_for_skewed_and_and_matches_classic`（n=3/6 32× 仍开车） |
| 现有测 | `auto_keeps_classic_leapfrog_for_skewed_pair` |
| 现有测 | `assert_eq!(auto_used, matches!(num_clauses, 2 \| 3))` 仍成立（n=4 Auto 用的是开车不是一对） |
| 新测 | n=4、n=6，列表差不多长、默认压缩块（128），Auto `lead_stream_searches==1` 且 `bulk_searches==0`，rows/kth 与 Off/On 一致 |
| 点名 | `cargo test -p lance-index --lib` 里上述 `wand` 测；各 <1s |

---

## 工作模式（开干后严格按这个顺序）

机器上另有占 CPU 的活结束之前：**只许读契约，不许 `cargo` / SBG / LTO。**

```text
1. 编码          相对干净 HEAD；cargo fmt。作者自审低级问题。
2. 测试 ∥ review 编码一结束就并行，互不等待：
                   - cargo test -p lance-index --lib <点名>
                   - cargo clippy -p lance-index --all-targets -- -D warnings（或仓库要求的 clippy 面）
                   - 独立 agent 代码检视（未提交 diff；intent 用本文「做/不做」）
3. 收口          测试红或 review P0/P1 → 先修再往下。P2 不挡 bench。
4. dump          943 条相对刀前 HEAD row+f32 bit-identical。
5. bench         官方 LTO；需要时先重跑刀前 HEAD。
6. 提交          通过 → commit。
                 不通过 → 仍然 commit，立刻 revert（留下这次尝试和数字）。
```

独立 review 的高后果 claim：

- Auto ≥3 且未被一对接走 → 短的开车；2/3 均衡与 256 宽仍一对。
- 2 词偏斜仍 leapfrog。
- 分数 / hit 与 classic 一致（现有 oracle + 新测）。
- 不改存储、不放开 128 捆 4+ 一对、不改 5b / IU / ReqOpt。
- 本刀 diff 不夹带下一刀（score-first 去掉 3×）。

---

## Bench 与提交

尺子：Wikipedia SBG A1 `TOP_10`，`taskset -c 0`，warmup 60s，10 iters min，全部 `LANCE_HACK_*` / `LANCE_DIAG_*` unset，**不要**设 `LANCE_FTS_BULK_AND`，**LTO** `do_query`（`/tmp/sbg-community/release/do_query`）。  
对照：同会话 `a489e15a7` 官方 LTO，不要和 compound-term-leaf 之前的 HEAD 横比。  
产物：`.agent/fts-and-lead-stream-general/`（`run-sbg-topk.sh`、`results.json`、`note.md`）。dump 脚本可复用 `.agent/fts-compound-term-leaf/`。

通过（全部）：

- 943/943 dump 一致
- intersection / union / phrase 均值相对同协议 HEAD **不回归**（约 ±3%）
- `+walk +the +line` 不回归（仍应开车）
- `+the +book +of +life` 不出现强制一对那种大幅变慢
- Hamlet AND / 短语 **不比刀前 leapfrog 慢**（不要求和强制一对的 −32% 比）
- TOP_10 AVERAGE 不回归

不通过：**commit + revert**。LTO-off / `BULK_AND=on` / 混会话 JSON 不当通过线。

Hamlet 若开车相对 HEAD 持平、也无其它回归：算对齐成功，**不要**在本刀再加一对例外。

---

## 开干检查单

- [x] 其它占 CPU 的工作已结束
- [x] `git status`：`rust/` 干净；HEAD 仍是 `a489e15a7`（若已前进，对照改成新 HEAD 官方 LTO）
- [x] 确认 `do_query` 与 HEAD 匹配，否则先编并重跑刀前官方表
- [x] 编码 → 测试∥review → dump → LTO → commit 或 commit+revert
- [x] `.agent/fts-and-lead-stream-general/note.md` 写下 SHA、dump、表、review 结论

通过线数字见 `.agent/fts-and-lead-stream-general/note.md`。对照是同会话 `head-run/results.json`，不是 compound-term-leaf 那次 JSON。LTO 编在 `/tmp/sbg-lead-stream-general`（`/tmp/sbg-community` 已被污染）。
