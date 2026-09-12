# lead-stream 内环：堆满后一块 lead 上 score-first（不改选核）

日期：2026-09-12  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `a659f1fe8`（`perf(fts): use lead-stream for leftover Auto AND of 3+ clauses`）  
对照：同会话官方 LTO。`lance-f03a2783c24f` 是 SBG **引擎 id**（path-dep 指纹），不是 git SHA `f03a2783c`。上一刀 JSON 不能自动当本刀 HEAD。本刀 **必须** 在 `/tmp/sbg-lead-stream-sf-block-head` 重跑 HEAD，刀本身编进 `/tmp/sbg-lead-stream-sf-block`，两套二进制互不覆盖。  
Lucene / Tantivy / 社区 main：`../lucene`、`../tantivy`、`/data/arrow/code/wt-fts-pr-base` @ `17a59357b`（只当选核/内环对照，不 port 词数桶、不改磁盘）。  
相关：[fts-and-lead-stream-general.md](./fts-and-lead-stream-general.md)、[fts-and-auto-n-buckets.md](./fts-and-auto-n-buckets.md)、[fts-next-optimizations.md](./fts-next-optimizations.md) §3.2、[fts-next-optimizations-production.md](./fts-next-optimizations-production.md)

本文是实现契约。独立设计检视 2026-09-12：**REJECT**，无 P0，四条 P1。P1.1 书面驳回（`+care +a +lot` 在 943 集）；P1.2–P1.4 已吸收。编码以本修订为准。

---

## 一句话

不改 posting 存储，不改 Auto 选核。lead-stream 在 **非短语、n≥3、second ≥ 3× lead** 时，把逐篇 park/seek 换成 Lucene `scoreWindowScoreFirst` / Tantivy `block_wand_intersection` / 分析树 `and_lead_stream_score_first_block` 那只内环：一块最短列表上先打精确 BM25，再只对幸存 buffer apply follower。

主证据是 `+walk +the +line` / `+university +of +washington`。不是 Hamlet，不是 AVERAGE 1.0×，不是再给社区发明一次 score-first。

---

## 为什么（内环，不是调度）

上一刀已经让维基 128 捆 leftover ≥3 词走 lead-stream。选核对齐了「最短开车」。开车之后仍是一篇一篇 `park_lead_at` + `document_key` + `seek_lead_followers`。3× 只给这条逐篇循环加了一个「先打 lead 分再 seek」的 `if`。

社区 `wt-fts-pr-base` 已经有 score-first 理念，但落点不是这条路：

| 位置 | 做什么 | 维基 A1 leftover |
|---|---|---|
| `#9030` bulk 交核 | 频次 LUT（`score(freq, dl=0)`）跳过再 `geq` follower；窗被**最密块**切开 | 均衡 2/3 在用。停词 3 词被 32× 拿出来了 |
| `#9033` classic 密窗 | lead **精确** BM25 再 skip follower | 只要 4\|5 × 256+impacts；Auto 下被 `#9030` 抢走 |
| 当前 lead-stream | 逐篇 park/seek；3× 时逐篇先打 lead | walk / university / 128 捆 4+ 在这 |

Lucene `BlockMaxConjunctionBulkScorer`：堆空 `scoreDocFirstUntilDynamicPruning`；堆满后窗跟 **lead impact**，`nextDocsAndScores` 一批精确 lead 分，buffer 上 `filterCompetitiveHits` + `applyRequiredClause`。没有词数桶，没有 3×。

Tantivy `block_wand_intersection`：最短当 lead，128 篇窗，块上界不够整窗跳；窗内批量 lead BM25，只对幸存者 seek。

分析树 `perf/fts-top10-analysis` @ `3d1189834` 在 lead-stream 里写了 `and_lead_stream_score_first_block`（进门仍是 3×、短语仍走逐篇）。walk 2299µs / 1.39× Lucene；当前 3690 / 2.42×。university 3363 / 1.09× vs 当前 5441 / 2.00×。Hamlet AND 两边都约 2× Lucene——3× 打不开，这刀也不该承诺 Hamlet。

相对分析树的 350µs AVERAGE 里，AND 内环大约对应 intersection 那一格（~70µs），不是 IU / 规划税。

---

## 做 / 不做

### 做（本刀全部）

1. `and_lead_stream_search`：`phrase_slop.is_none() && and_score_first_for(...)` 时走新的一块一趟，而不是逐篇 `park`。
   - 堆空 **且** `limit` 能填满（`limit != usize::MAX`）：同一 lead 块按 **16 篇** 一批喂进 block 核，好让 Exclusive 地板尽快起来（对齐 Lucene 先 doc-first 填堆，不是 port `scoreDocFirstUntilDynamicPruning`）。
   - 堆满，或 `limit` 填不满（`None` / `usize::MAX`）：这块剩下的一次吃完。COUNT / 无界收集不得被 16 篇切片。
2. `and_lead_stream_score_first_block`（新函数，留在 `wand_lead_stream.rs`）：
   - 频次 LUT 先滤（现成 `freq_cannot_beat`）。
   - 幸存 lead 打 **精确** BM25（优先 `exact_bm25_addends` / 量化 `norm_k`；否则 `lead[0].score`）。
   - `lead_score + others_block_max` 进不了堆 → 不 seek。
   - 按 clause 1..n-1 对 buffer `next(target)`，命中则累加该 clause 精确分，用 `others_bounds[clause..]` 做后缀上界再剪。
   - 可见性用 `document_key_for_doc_id`（交完再问，不要每篇先 `park`）。
   - 总分 `score_contributions_in_query_order`。非短语收割只用 `candidates.rejects_score`（与 `insert` 的入堆判定同字节），**不要**叠 `exclusive_score_cannot_beat_floor`（现逐篇非短语路径也没有这层）。`update_threshold` 与现循环相同。
3. 3× 门 **原样保留**（`AND_SCORE_FIRST_COST_RATIO = 3`）。短语、2 词、second &lt; 3× lead：仍走现在的逐篇循环。
4. 打分辅助复用现成的：`exact_bm25_addends`、`bm25_doc_weight_with_norm`；需要的话把 `wand_maxscore.rs` 里的 `bm25_tf_from_caches` 升成 `pub(super)`。不要 port 分析树 `bulk_clause_bm25` / `and_diag` / `LANCE_DIAG_*`。
5. 单测：3× 成立时 Auto 走 block 核、hit/floor 与 Off/On 一致；3× 不成立时仍逐篇 lead-stream；短语仍逐篇。dump 943 bit-identical；官方 LTO 通过才留 commit。

进门（相对现在只多这一层，选核不动）：

```text
不偏斜 且 (2|3 或 256 宽) → 一对                         // 不动
Auto leftover ≥3
  非短语 且 second ≥ 3× lead → lead-stream **block** 核   // 本刀
  其余（短语 / 偏斜不够）     → lead-stream 逐篇           // 不动
2 词偏斜                     → leapfrog                    // 不动
```

真正换内环的：停词 3+ 且已经在开车的查询（`+walk +the +line`、`+university +of +washington`、`+time +for +kids`）。Hamlet / `+care +a +lot`（在 943 集，queries.txt:539）/ 专名 4 词 3× 打不开，应几乎不动。3× 留下是因为 balanced leftover 几乎每个 lead doc 都要 seek（[fts-next-optimizations.md](./fts-next-optimizations.md) §3.2），不是因为某一条 named query。

### 不做

- 改 `Wand::search` Auto 选核、`AND_SKEW_RATIO`、`enabled_for`、`auto_wide_modern`。
- 改 `AND_SCORE_FIRST_COST_RATIO`（堆满后无条件 score-first 是下一刀；`+care +a +lot` 回归过）。
- 短语进 block 核；分析树 `phrase_pair_prune`。
- 给 128 捆 n≥4 开一对；把 walk 送回社区 bulk。
- 扩 `#9033` 的 4\|5 × 256 门到 lead-stream。
- 分析树 4–5–6 词桶、`min_cost≥500_000`、2-ess SoA、`u16` 字典、`and_diag`。
- Widen 5b / 85k、floor 写入 MUST、5a promote、ReqOpt skip、规划税。

---

## 正确性约束

- Exclusive 地板：堆未满不剪；剪枝上界只许更保守（频次 LUT 用 `dl=0`，后缀用块 max × `score_sum_upper_bound_factor`）。
- 公开分数仍是 query-order `f32` 累加，与 classic / 现逐篇 lead-stream 同 tie。
- follower `Leap`：丢掉 buffer 里 `&lt; next` 的 lead。逐篇循环还能 `target = next` 跳出本块；**压紧 buffer 的 block 核做不到这步**，未命中只是不写入下一层，外环仍走 `win_end+1`。正确、略慢，不要声称与 `skip_lead_docs` 等价。
- follower 耗尽：整段搜索结束。
- 可见性：交完再 `document_key_for_doc_id`；不可见的不入堆、不抬地板。
- 短语：本刀函数进不去；现成 `exclusive_score_cannot_beat_floor` + `check_positions` 保持。
- 一批之内地板冻结，剪得比逐篇少（保守）。`others_bounds` 在窗头算一次；未 `shallow_next` 时 `max_score_up_to` 从可能落后的 `block_idx` 往 `win_end` 扫，上界只更宽。
- 新核不需要 `#9033` 的 4\|5 × 256+impacts / distinct token_id / `doc_norm(1)` 守卫：选核已经排除 grouped terms。

---

## 代码与测试落点

| 项 | 位置 |
|---|---|
| 内环 | `wand_lead_stream.rs`：新 `and_lead_stream_score_first_block`；`and_lead_stream_search` 在 3× 且非短语时改道 |
| 调度 | `wand.rs` `Wand::search` **一行不改** |
| 可选 | `wand_maxscore.rs` `bm25_tf_from_caches` → `pub(super)` |
| 保持 | `AND_SKEW_RATIO=32`；`AND_SCORE_FIRST_COST_RATIO=3`；`and_score_first_for` 谓词 |
| 现有测 | `score_first_needs_three_clauses_and_a_3x_second` |
| 现有测 | `auto_uses_lead_stream_for_skewed_and_and_matches_classic`（应开始走 block 核，rows/kth 仍 = classic） |
| 现有测 | `auto_uses_lead_stream_for_unskewed_legacy_wide_and`（2× &lt; 3×，仍逐篇） |
| 现有测 | `lead_stream_phrase_matches_classic_and_bulk` |
| 现有测 | `auto_keeps_classic_leapfrog_for_skewed_pair` / 31× 仍一对 |
| 新测 | 偏斜 3 词、无短语、limit=10：Auto `lead_stream_searches==1` 且 `score_first_blocks≥1`，rows/kth 与 Off 一致；短语同形状 `score_first_blocks==0`；`limit=None` 仍走 block 核且 rows 与 Off 一致（整块喂入，不 16 篇切） |
| 点名 | `cargo test -p lance-index --lib` 上述 `wand` / `wand_lead_stream` 测；各 &lt;1s |

`score_first_blocks` 用 `#[cfg(test)]` 计数，只证明走了新核，不当正确性。

---

## 工作模式（开干后严格按这个顺序）

```text
1. 编码          相对干净 HEAD；cargo fmt。作者自审低级问题。
2. 测试 ∥ review 编码一结束就并行，互不等待：
                   - cargo test -p lance-index --lib <点名>
                   - cargo clippy -p lance-index -- -D warnings
                   - 独立 agent 代码检视（未提交 diff；intent 用本文「做/不做」）
3. 收口          测试红或 review P0/P1 → 先修再往下。P2 不挡 bench。
4. dump          943 条相对刀前 HEAD row+f32 bit-identical。
5. bench         官方 LTO；需要时先重跑刀前 HEAD。
6. 提交          通过 → commit。
                 不通过 → 仍然 commit，立刻 revert（留下这次尝试和数字）。
```

独立 **代码** review 的高后果 claim：

- 选核一行不改；3× / 短语仍决定走不走 block 核。
- block 核 hit / kth bits 与 classic 一致（oracle 测）。
- 剪枝不上抬（Exclusive + 保守上界）。
- 不改存储、不放开 128 捆 4+ 一对、不改 5b / IU / ReqOpt、不删 3×。

---

## Bench 与提交

尺子：Wikipedia SBG A1 `TOP_10`，`taskset -c 0`，warmup 60s，10 iters min，全部 `LANCE_HACK_*` / `LANCE_DIAG_*` unset，**不要**设 `LANCE_FTS_BULK_AND`，**LTO** `do_query`。  
HEAD：`CARGO_TARGET_DIR=/tmp/sbg-lead-stream-sf-block-head`。刀：`/tmp/sbg-lead-stream-sf-block`。都不要编进 `/tmp/sbg-community` 或 `/tmp/sbg-lead-stream-general`。  
对照：本刀同会话 HEAD JSON（`.agent/fts-and-lead-stream-score-first-block/head-run/results.json`）。上一刀 `.agent/fts-and-lead-stream-general/results.json` 只作参考，不当通过线。dump 基线：`.agent/fts-and-lead-stream-general/knife-all.json`。脚本可复用 `.agent/fts-compound-term-leaf/dump_topk.py`。  
产物：`.agent/fts-and-lead-stream-score-first-block/`（`run-sbg-topk.sh`、`results.json`、`note.md`）。

通过（全部，µs 相对 **本会话 HEAD**，tag 均值约 ±3%）：

- 943/943 dump 一致
- intersection / union / phrase 均值 **不回归**
- `+walk +the +line` **明显快于**本 HEAD（主证据；分析树方向是 3690→~2300µs，不要求打到 Lucene 1523）
- `+university +of +washington` 不回归；期望同向变快
- `+time +for +kids` 不回归；期望同向变快（3× 应打开）
- `+griffith +observatory` / `+care +a +lot` / Hamlet AND 约 ±3%（3× 打不开 / 没换核）
- `+los +angeles +daily +news` 约 **持平本 HEAD**（3× 多半关着；不要求追回相对旧 leapfrog 的 +109%）
- TOP_10 AVERAGE 不回归

不通过：**commit + revert**。LTO-off / `BULK_AND=on` / 混会话 JSON 不当通过线。  
walk 若持平或变慢、又没有别的 named 把 AVERAGE 拉回来：算这只内环没付账，revert。不要在本刀加一对例外或拆 3×。

---

## 开干检查单

- [x] 设计和目标已经过独立 agent 检视；P0/P1 已吸收或书面驳回（见下）
- [ ] `git status`：本刀 rust 干净；HEAD 仍是 `a659f1fe8`（若已前进，对照改成新 HEAD 官方 LTO）
- [ ] 编码 → 测试∥review → dump → LTO → commit 或 commit+revert
- [ ] `.agent/fts-and-lead-stream-score-first-block/note.md` 写下 SHA、dump、表、review 结论

---

## 独立设计检视（2026-09-12）

Verdict：**REJECT**（无 P0；四条 P1，编码前处理）。内核设计本身判定为 sound。

| P1 | 处理 |
|---|---|
| 1. `+care +a +lot` 不在 943 集 | **驳回**。`queries.txt:539`，上一刀 `results.json` 有这条（5648µs）。单行 JSON 上 grep 不可靠。保留通过线；3× 的理由是 §3.2 的 balanced leftover，不是这一条 query。 |
| 2. 必须同会话重跑 HEAD | **吸收**。`lance-f03a2783c24f` ≠ git SHA。HEAD 编 `/tmp/sbg-lead-stream-sf-block-head`，刀编 `/tmp/sbg-lead-stream-sf-block`。 |
| 3. 点名 `+los +angeles +daily +news` | **吸收**。期望相对本 HEAD 持平（3× 多半关）；相对旧 leapfrog 的 2× 不在本刀范围。 |
| 4. `limit==MAX` 不要 16 篇切块 | **吸收**。堆填不满时整块一次进核。 |

P2 已写进正确性约束：`rejects_score` only、block 核不做 `target=next`、批内地板冻结、`others_bounds` 保守、`num_comparisons` 可只计过地板后的候选。P2 不挡 bench。
