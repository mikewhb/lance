# lead-stream 进门：堆满后不看 3×（核已经在）

日期：2026-09-12  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `50f95778f`（`perf(fts): score lead-stream AND blocks after the heap fills`）  
对照：同会话官方 LTO。`lance-f03a2783c24f` 是 SBG **引擎 id**（path-dep 指纹），不是 git SHA。上一刀 JSON 不能当本刀 HEAD。本刀 **必须** 在 `/tmp/sbg-lead-stream-sf-floor-head` 重跑 HEAD，刀本身编进 `/tmp/sbg-lead-stream-sf-floor`，两套二进制互不覆盖。钉核：**一个逻辑 CPU `taskset -c 32`**（物理核 16 的一条 SMT；不要 `taskset -c 0`，也不要 `32,33`）。  
Lucene / Tantivy / 社区 / 分析树：`../lucene`、`../tantivy`、`/data/arrow/code/wt-fts-pr-base` @ `17a59357b`、`perf/fts-top10-analysis` @ `3d1189834`（只当政策对照，不 port 词数桶、不改磁盘、不改 block 核身体）。  
相关：[fts-and-lead-stream-score-first-block.md](./fts-and-lead-stream-score-first-block.md)、[fts-and-lead-stream-general.md](./fts-and-lead-stream-general.md)、[fts-next-optimizations.md](./fts-next-optimizations.md) §3.2

本文是实现契约。独立设计检视 2026-09-12：**REJECT**，无 P0，五条 P1。P1.1–P1.5 已吸收（见文末）。编码以本修订为准。

---

## 一句话

不改 posting 存储，不改 Auto 选核，**不改** `and_lead_stream_score_first_block` 的身体。只改 lead-stream 进这只核的门票：堆空仍要 `n≥3 && second ≥ 3× lead`；**`threshold > 0` 之后不再看 3×**。短语永远不进核。

这是收政策，不是新核。主证据是 3× 打不开的 leftover AND（Hamlet、`+los +angeles +daily +news`）。不是 AVERAGE 1.0×，不是 Hamlet 打到 Lucene，不是堆空时拆掉 3×。杀开关：`+care +a +lot`（943 集 `queries.txt:539`）。

---

## 为什么（进门，不是内环）

上一刀已经把 Lucene `scoreWindowScoreFirst` 形状写进 `and_lead_stream_score_first_block`。进门仍是静态的 `and_score_first_for`：

```text
use_block = 非短语 && n≥3 && second ≥ 3× lead    // 搜开头算一次，整段不变
```

堆满只影响 **已经进核之后** 一次吃 16 篇还是整块（`score_first_batch_len`），不决定能不能进核。所以 Hamlet / LA news / `+care +a +lot` 全程逐篇；walk / university 全程 block。

Lucene `BlockMaxConjunctionBulkScorer` 的轴是地板，不是列表比：

| 阶段 | Lucene | 我们现在 |
|---|---|---|
| 堆空 | `scoreDocFirstUntilDynamicPruning`（先交 id） | 3× 关 → 逐篇；3× 开 → block（16 篇切，为了抬 Exclusive 地板） |
| 堆满 | 总是 score-first；**没有 3×** | 仍看搜开头那一次 3× |

分析树 @ `3d1189834` 留 3× 的书面理由（`wand.rs` 当时的 `and_score_first_for` 文档）点名的就是本刀要放进核的人群：

> Score-first pays an exact lead BM25 on every rare-list doc. That only
> beats per-doc seek when the second clause is long enough that dropping
> lead docs avoids a lot of follower `advance` — the same 3× shape as IU
> promotion. Balanced leftover AND (`+care +a +lot`, 4-term city names)
> still seeks almost every lead doc and the extra scores regress.

这段说的是 **「打了 lead 分却仍然 seek」** 的浪费，不是「堆状态无关」。地板=0 时 Exclusive 剪枝永不触发，打分+seek 是纯加价，所以堆空必须留 3×。地板>0 之后，`lead_score + others_block_max` 可以丢掉一部分 seek；长短接近时块上界很宽，丢掉的可能很少——那正是本刀要量的，不是事先当成已经证伪。Hamlet 分析树约 2× Lucene（~42101，会话回忆，仓库里没有分析树 results.json 可钉）和本 HEAD `taskset -c 32` 46505 都说明 3× 关着的 leftover 没被那棵树收掉。本刀越过分析树、对齐 Lucene **堆满后的进门**。不承诺 Hamlet 1.0×。

另一半 Lucene 本刀 **故意不 port**：`scoreWindowScoreFirst` 结束会把 lead `advance` 到 `max(other.docID())`，下一窗 `windowMin = max(lead.docID(), windowMax+1)`（`BlockMaxConjunctionBulkScorer.java:199-205` 与 `:105`）。我们的核退出永远 `target = win_end + 1`，上一刀写过「正确、略慢，不要声称与 `skip_lead_docs` 等价」。3× 时密 follower 很少跳出本块；偏斜→1 时 follower 缺口更容易跨过 `win_end`，逐篇路径的 `target = next` 正是今天 balanced leftover 便宜的原因。把 `max_other_doc` 从核里传出去是下一刀的布局/跳窗，本刀不改核身体。因此 Hamlet / LA news / `+care +a +lot` 的通过线承担「进了一只不会 leap 出窗的核」的全部风险。

权威尺子（上一刀 `rerun-cpu32/`，同一对 LTO，`taskset -c 32`）：

| | HEAD a659 | 刀 50f95778f | 含义 |
|---|---:|---:|---|
| AVERAGE | 2104 | 2107 | 内环没动 AVERAGE |
| `+walk +the +line` | 3612 | 2746 | 3× 已换核 |
| Hamlet AND | 46711 | 46505 | 3× 关着 |
| `+care +a +lot` | 5620 | 5661 | 3× 关着 |
| `+los +angeles +daily +news` | 6159 | 6226 | 3× 关着 |

本刀若只动进门，walk 应持平；钱只可能来自 3× 关着的 leftover 在堆满之后。Hamlet 对 AVERAGE 贡献约 50µs，就算打到 Lucene 20663 也只省 ~25µs AVERAGE。**不要**用 AVERAGE 当本刀是否成功的主证据。

---

## 做 / 不做

### 做（本刀全部）

1. `and_lead_stream_search`：进核从静态一次改成 **每个 window 开头** 判定：

   ```text
   score_first = and_score_first_for(...)          // 仍是 3×，堆空用
   每窗：
     use_block = 非短语 && (score_first || self.threshold > 0.0)
   ```

   堆空留下的只是 **cost ratio**。`n≥3` 已经由 `Wand::search` 的 lead-stream 入口保证（`num_clauses >= 3`），不要在本函数再写一条假装在守门的 `num_lists >= 3`。

   `raise_to_shared_floor` 在窗头已经跑过：Boolean 父节点若已经抬了地板，本叶从第一窗就可以进核。孤立 TOP_10 的共享地板从 0 起，前几窗仍 3× 门。`use_block` 只允许 `false → true`（`threshold` 单调不降）；不要写回逐篇。

2. 同一窗内堆刚满：**走完当前逐篇循环**，下一窗再进核。不要做「窗中途切核」第二套交接。`score_first_batch_len` 不动：3× 且堆空仍 16 篇切；堆满或 `limit == usize::MAX` 仍整块。

3. BM25 cache 与 `score_*` buffer reserve 按 **核达得到** 准备，不要「凡非短语就建 slab」：

   ```text
   搜开头先 raise_to_shared_floor 一次（与窗头同一函数，幂等）
   kernel_reachable = 非短语 && (score_first || heap_can_fill || threshold > 0)
   ```

   COUNT / `limit == None` 且 3× 关且没有共享地板：核进不去，禁止 `exact_bm25_addend_slab`（`O(num_docs)` 的 `f32` 页，分析树 `3d1189834` 在 balanced 3-AND 上写过「pollutes the classic seek loop」）。共享地板第一窗、以及 top-k 堆会满：要准备，否则进核时 cache 是空的。短语不准备。

4. `AND_SCORE_FIRST_COST_RATIO = 3`、`and_score_first_for` **原样保留**。禁止堆空时无条件 score-first。

5. `Wand::search` Auto 选核 **一行不改**。`and_lead_stream_score_first_block` 的剪枝/打分/收割 **一行不改**。

6. 单测：见落点。dump 943 bit-identical；官方 LTO 通过才留 commit（不通过则 commit+revert）。

进门（相对现在只改这一层）：

```text
不偏斜 且 (2|3 或 256 宽) → 一对                         // 不动
Auto leftover ≥3
  短语                       → lead-stream 逐篇           // 不动
  非短语 且 (3× 或 地板>0)   → lead-stream **block** 核   // 本刀：地板也能进
  非短语 且 3× 关 且 地板=0  → lead-stream 逐篇           // 堆空 balanced
2 词偏斜                     → leapfrog                    // 不动
```

真正新换核的：3× 打不开、但 top-k 堆会满的 leftover AND（Hamlet、`+los +angeles +daily +news`、以及堆满后的 `+care +a +lot`）。walk / university / kids 本来就 3×，应几乎不动。COUNT / `limit == None` 且没有共享地板：地板起不来，3× 关着的仍全程逐篇。

### 不做

- 改 `Wand::search` Auto 选核、`AND_SKEW_RATIO`、`enabled_for`、`auto_wide_modern`。
- 改 `AND_SCORE_FIRST_COST_RATIO` 的数值；堆空时拆掉 3×。
- 改 `and_lead_stream_score_first_block` 身体（频次 LUT、精确 BM25、后缀上界、`rejects_score`、`win_end+1` 不 leap）。那是 walk 相对分析树还差 ~450µs 的布局问题，另开。
- 短语进 block 核；分析树 `phrase_pair_prune`。
- 窗中途从逐篇切到 block（cursor / buffer 交接）。
- 给 128 捆 n≥4 开一对；把 walk 送回社区 bulk；扩 `#9033`。
- 分析树 4–5–6 词桶、`min_cost≥500_000`、2-ess SoA、`u16` 字典、`and_diag` / `LANCE_DIAG_*`。
- Widen 5b / 85k、floor 写入 MUST、5a promote、ReqOpt skip、规划税、MaxScore 收核。
- 为 Hamlet / LA news / `+care +a +lot` 再加 named 例外。

---

## 正确性约束

- 剪枝仍 Exclusive：`threshold == 0` 不剪。上界只许更保守。block 核身体不改，上一刀的约束原样成立。
- 公开分数仍是 query-order `f32` 累加，与 classic / 现逐篇 / 现 block 核同 tie。
- 中途换核只发生在 window 边界：逐篇循环把当前块走完，下一窗 `lead[0].next(target)` 与现在相同。不要把半块 lead buffer 喂给 block 核。
- 短语：`phrase_slop.is_some()` 时 `use_block` 恒 false，现成 `exclusive_score_cannot_beat_floor` + `check_positions` 保持。
- `limit == usize::MAX` 且共享地板为 0：堆填不满，`threshold` 保持 0，3× 关着不得进核。
- 共享地板 > 0：允许第一窗就进核（父节点已经有竞争性分数）。
- dump：943 条相对本 HEAD row+f32 bit-identical。进核变了，命中集合和分数字节不能变。

---

## 代码与测试落点

| 项 | 位置 |
|---|---|
| 进门 | `wand_lead_stream.rs` `and_lead_stream_search`：`use_block` 挪进 `'window`；cache / `score_*` reserve 按 `kernel_reachable` 准备 |
| 核 / 调度 | `and_lead_stream_score_first_block` **不改**；`wand.rs` `Wand::search` **不改** |
| 保持 | `AND_SKEW_RATIO=32`；`AND_SCORE_FIRST_COST_RATIO=3`；`and_score_first_for` 谓词 |
| 现有测 | `score_first_needs_three_clauses_and_a_3x_second`（谓词本身不变） |
| 现有测 | `auto_uses_lead_stream_for_skewed_and_and_matches_classic`（3×，仍 `score_first_blocks≥1`） |
| 现有测 | `auto_score_first_block_matches_classic_with_unbounded_limit`（3× + `limit=None`，仍进核） |
| 现有测 | `lead_stream_phrase_matches_classic_and_bulk`（短语仍 `score_first_blocks==0`） |
| 现有测 | `auto_keeps_classic_leapfrog_for_skewed_pair` / 31× 仍一对 |
| **改测** | `auto_uses_lead_stream_for_unskewed_legacy_wide_and`：2× &lt; 3×、`limit=10`，现在断言 `score_first_blocks==0`。本刀堆满后应 `≥1`，rows/kth 仍 = classic / On |
| 新测 | 同形状、`limit=None`：Auto `lead_stream_searches==1` 且 `score_first_blocks==0`（无地板、无 3×，不得进核） |
| 新测 | 同形状、`limit=10`、共享地板预置 &gt; 0：Auto 第一窗就 `score_first_blocks≥1`，rows/kth 与同一地板下的 Off 一致（`raise_to_shared_floor` 在 classic 也会跑） |
| 点名 | `cargo test -p lance-index --lib` 上述 `wand` / `wand_lead_stream` 测；各 &lt;1s |

`score_first_blocks` 用 `#[cfg(test)]` 计数，只证明走了核，不当正确性。

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
5. bench         官方 LTO；必须同会话重跑 HEAD。钉 `taskset -c 32`。
6. 提交          通过 → commit。
                 不通过 → 仍然 commit，立刻 revert（留下这次尝试和数字）。
```

独立 **代码** review 的高后果 claim：

- 选核一行不改；block 核身体一行不改。
- 堆空且 3× 关且无共享地板：不进核。
- 堆满或共享地板 > 0：非短语 leftover 进核。
- 短语永不进核。
- hit / kth bits 与 classic 一致（oracle 测）。
- 不改存储、不放开 128 捆 4+ 一对、不改 5b / IU / ReqOpt、不删 3× 常数。

---

## Bench 与提交

尺子：Wikipedia SBG A1 `TOP_10`，**`taskset -c 32`**（一个逻辑 CPU），warmup 60s，10 iters min，全部 `LANCE_HACK_*` / `LANCE_DIAG_*` unset，**不要**设 `LANCE_FTS_BULK_AND`，**LTO** `do_query`。  
HEAD：`CARGO_TARGET_DIR=/tmp/sbg-lead-stream-sf-floor-head`。刀：`/tmp/sbg-lead-stream-sf-floor`。都不要编进 `/tmp/sbg-community`、`/tmp/sbg-lead-stream-sf-block` 或 `/tmp/sbg-lead-stream-general`。  
对照：本刀同会话 HEAD JSON（`.agent/fts-and-lead-stream-floor-score-first/head-run/results.json`）。上一刀 `rerun-cpu32/` 只作参考，不当通过线。dump 基线：`.agent/fts-and-lead-stream-score-first-block/knife-all.json`（若无，用 `.agent/fts-and-lead-stream-general/knife-all.json`）。脚本可复用上一刀 `run-sbg-topk.sh` / `compare_sbg_topk.py` / `.agent/fts-compound-term-leaf/dump_topk.py`。  
产物：`.agent/fts-and-lead-stream-floor-score-first/`（`run-sbg-topk.sh`、`results.json`、`note.md`）。

通过（全部，µs 相对 **本会话 HEAD**，tag 均值约 ±3%）：

- 943/943 dump 一致
- intersection / union / phrase 均值 **不回归**
- TOP_10 AVERAGE **不回归**（不要求变快；Hamlet 再快也只动 AVERAGE ~25µs）
- `+walk +the +line` / `+university +of +washington` / `+time +for +kids` **不回归**（3× 本来就在核里；约 ±3%）
- `+griffith +observatory` 约 ±3%（2 词，选核不动）
- **`+care +a +lot` 不回归**（杀开关；堆空仍逐篇，堆满才进核。回归 → 整刀 revert，不要加 named 例外。它不是正向通过线）
- Hamlet AND 与 `+los +angeles +daily +news`：都不回归。**正向通过线**：二者至少一条相对本 HEAD 快过 ±3% 噪声带，否则 revert。不要求打到 Lucene / 分析树，也不用 AVERAGE 代替这两条。核不会 `target=next` 跳出本块，这两条承担全部「低偏斜进核」的风险。

不通过：**commit + revert**。LTO-off / `BULK_AND=on` / `taskset -c 0` / 混会话 JSON 不当通过线。  
`+care +a +lot` 若明显变慢，或 Hamlet 与 LA news 都未快过噪声：算堆满后对 balanced leftover 仍太贵 / 政策没付账，revert。不要在本刀把 3× 加回堆满路径，也不要给 named query 开例外。

---

## 开干检查单

- [x] 设计和目标已经过独立 agent 检视；P0/P1 已吸收或书面驳回（见下）
- [x] `git status`：本刀 rust 干净；HEAD 仍是 `50f95778f`（若已前进，对照改成新 HEAD 官方 LTO）
- [x] 编码 → 测试∥review → dump → LTO → commit 或 commit+revert
- [x] `.agent/fts-and-lead-stream-floor-score-first/note.md` 写下 SHA、dump、表、review 结论

---

## 独立设计检视（2026-09-12）

Verdict：**REJECT**（无 P0；五条 P1，编码前处理）。内核机制判定为 sound：窗边界换核、`limit==None` 排除、选核不动、堆空 3× 仍在。

| P1 | 处理 |
|---|---|
| 1. 分析树注释点名 care+a+lot / 城市名 4 词会回归，契约只写「也留了 3×」 | **吸收**。原文写进「为什么」。浪费发生在「打了 lead 仍 seek」；地板=0 必发生，地板>0 才是本刀要量的。 |
| 2. 凡非短语就建 exact-addend slab，COUNT 走不到核也付 `O(num_docs)` | **吸收**。改成 `kernel_reachable = 非短语 && (3× \|\| 堆能满 \|\| 已有地板)`。 |
| 3. 引 Lucene 进门却禁止 port 它的 follower leap 出窗 | **吸收**。风险段写明 `java:199-205`；`max_other_doc` 推迟。Hamlet / LA news 通过线承担这只核的跳窗缺陷。 |
| 4. 「全部持平也留下」无法证伪 | **吸收**。Hamlet 或 LA news 至少一条快过 ±3%，否则 revert。care+a+lot 只当杀开关。 |
| 5. 共享地板第一窗进核没有测试 | **吸收**。测试表加预置地板的 3×-关 leftover。 |

P2 已写进正文：堆空活门是 cost ratio 不是再写一遍 `n≥3`；`score_*` reserve 与 cache 同谓词；`use_block` 单调 `false→true`；分析树 Hamlet 42101 是会话回忆不是钉盘数字。P2 不挡 bench。
