# leftover 核：probe 显示本窗 lead 分剪不动时，先交 id 再打分

日期：2026-09-12  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `867ed042b`（`perf(fts): leap lead-stream AND windows to the farthest follower`）  
对照：同会话官方 LTO。`lance-f03a2783c24f` 是 SBG **引擎 id**，不是 git SHA。上一刀 JSON 不能当本刀 HEAD。本刀 **必须** 在 `/tmp/sbg-lead-stream-h2a-head` 重跑 HEAD，刀本身编进 `/tmp/sbg-lead-stream-h2a`，两套二进制互不覆盖。钉核：**一个逻辑 CPU `taskset -c 32`**。  
单条对照噪声带 ±6%。Bench 前看 loadavg；默认 paired 第二轮。  
Lucene：`../lucene/.../BlockMaxConjunctionBulkScorer.java` 只作「堆满后仍可先交后分」的政策对照，**不** port `nextDocsAndScores` 向量化。  
归因：`.agent/fts-slow-query-attribution/report.md`（H2a 活；H1/H2b/H3 死）。  
直方图：`.agent/fts-and-lead-stream-h2a-lead-bm25/hist.err`（未提交 `LANCE_DIAG_AND`，已撤回）。  
相关：[fts-and-lead-stream-floor-score-first.md](./fts-and-lead-stream-floor-score-first.md)、[fts-and-lead-stream-block-leap.md](./fts-and-lead-stream-block-leap.md)

本文是实现契约。独立设计检视 2026-09-12：**REJECT**（P0：草稿里的 `cannot_compete(0, others)` 在混合窗上会变成 no-op）。P0/P1 已吸收（见文末）。编码以本修订为准。

---

## 一句话

不改 posting 存储，不改 Auto 选核，不拆 3×，不把 wiki-128 4+ 送回 bulk / leapfrog。只改 **已经进** block 核的 leftover 窗：先用 `HEAP_FILL_BATCH`（16）篇 score-first 探本窗 lead BM25 的地板剪枝率；若 `< 1/4`，本窗剩余 **先 compact followers，只给交上的篇打 BM25**。3× 窗（`score_first == true`）整窗仍 score-first，不探。

主证据是 `+los +angeles +daily +news` 地板刀 +17%（6106→7140）。不是 AVERAGE，不是打到 Lucene 2583，不是 2 词 leapfrog。杀开关：`+care +a +lot`、Hamlet AND、`+the +book +of +life`、walk。

---

## 为什么（核身体，不是进门）

地板刀让 leftover 在 `threshold > 0` 后进同一只 score-first 核。归因（官方 LTO + `LANCE_DIAG_AND`，已撤回计数）：

| | score_first | lead_bm25 | lut_skip | floor_skip | compact_out | 地板刀 Δ |
|---|---:|---:|---:|---:|---:|---|
| LA 4 词 leftover | 0 | 76896 | **0** | **11.5%** | 134 | **+16.9%** |
| care 3 词 leftover | 0 | 55628 | 0 | **58%** | 50 | **−17.9%** |
| Hamlet 6 词 leftover | 0 | 731679 | 9% | **92%** | 123 | **−40.4%** |
| book of life 4 词 leftover | 0 | 88814 | **~53%** | 混合偏高 | — | 地板刀在赚 |
| walk 3× | 1 | 35733 | 0.6% | **91%** | 84 | −2.1% |

LA 付了 7.7 万次 lead BM25，只交上 134 篇。care / Hamlet / book-of-life 同样付 lead BM25，但地板（或 LUT）剪掉大半 follower seek，所以净赚。

草稿曾用 `cannot_compete(0.0, others_block_max)` 当每窗开关。独立检视 REJECT 是对的：Exclusive 下 `lead_score ≥ 0`，**只要本窗有一次 floor_skip，就有 `followers_alone_lose == true`**，整窗留在 score-first。混合窗（`others < T < others+max_lead`）正是 leftover 维基的主体，0.0 公式救不了那 88.5%。

直方图（同一 index，未提交计数，已撤回；`lose0_skip1=0` 验证了单调性）：

| query | 0.0 会切的 docs | probe16&lt;1/4 会切的 rest docs | 含义 |
|---|---:|---:|---|
| LA | 14101 / 76896（**18%**） | **53938** / 76896（70%） | 0.0 付不满 ±6%；probe 才打在混合低剪窗 |
| care | 1141 / ~55k（2%） | 6903（12%） | 切的是低剪窗；58% 剪枝质量仍 score-first |
| Hamlet | 640 / 805k（0.08%） | 5139（0.6%） | −40% 几乎不动 |
| book of life | **0** | **0** | LUT 已在工作，probe 全 keep |
| hot springs | 6232（48%） | 10574 | 地板刀同号变慢，本刀应帮忙 |
| walk | （3× 不探） | — | 整窗 score-first |

0.0 公式单独留下会在 LA 上失败通过线（预期信封 ~0.2ms ≪ 427µs 噪声带）。本刀锁 **窗内 probe**。`1/4` 落在已测空隙里（LA 窗质量在 &lt;25%，care/Hamlet/book-of-life 质量在 ≥50%），不是 named query。`16` 复用已有 `HEAP_FILL_BATCH`。

禁止用 `n==4` / named query / `AND_SKEW_RATIO` 当进门。

---

## 做 / 不做

### 做（本刀全部）

1. `use_block` 谓词 **不改**：`phrase_slop.is_none() && (score_first || threshold > 0.0)`。

2. `score_first == true`（3×）：整窗仍走现成 `and_lead_stream_score_first_block`。不探、不切 intersect-first。

3. leftover（`!score_first && threshold > 0`）的 `use_block` 窗：
   - 先对 lead buffer 的前 `HEAP_FILL_BATCH` 篇走 score-first（LUT 仍先剪；probe 分母是实际打了 BM25 的篇，不是 LUT 丢掉的）。
   - `probe_n == 0`（前 16 全被 LUT 剪掉）→ 剩余仍 score-first（没有剪枝率证据，不要切）。
   - `probe_skip / probe_n >= 1/4` → 剩余 score-first。
   - `probe_skip / probe_n < 1/4` → 剩余 **intersect-first**：
     - lead id/freq 仍来自已有 buffer；`freq_cannot_beat` 仍可丢掉 id。
     - **先**按 id `next(target)` compact 所有 follower。compact 过程 **禁止** `cannot_compete(0, remaining)`：partial=0 时会把混合窗整窗丢掉。
     - 只给 compact 后的篇算各子句 BM25（含 lead），再 `score_contributions_in_query_order` + `insert`。
   - 窗末 leap 仍在 caller，两条核之后都做。

4. LUT：`score(freq, 0)` 长度 0 上界 **不收紧**。

5. `and_lead_stream_score_first_block` 返回本批 `lead_bm25` / `floor_skip`，供 leftover 判剩余走哪条。不要环境变量、不要 TLS 诊断留在产品路径。

6. 单测 + dump 943 bit-identical + 官方 LTO。通过才留 commit。

### 不做

- 改 `Wand::search` Auto 选核、`AND_SKEW_RATIO`、`enabled_for`、`auto_wide_modern`。wiki-128 4+ **不回** bulk / classic leapfrog。
- 拆堆空 3×；`AND_SCORE_FIRST_COST_RATIO` 改数值。
- 3× 窗改成 intersect-first（walk / university / kids 必须仍 score-first）。
- 短语进 block 核。
- 为 LA / care / Hamlet 加 named 例外。
- 只用 `cannot_compete(0.0, others_block_max)` 当开关（草稿 P0；直方图证明 LA 只救 18%）。
- 本刀再压 `clause_bm25` / 新 SIMD `nextDocsAndScores`（`norm_k` / `exact_addends` 已在核里；那是下一刀）。
- 2 词 skewed leapfrog 加块上界（另开）。
- IU / OR / 两阶段 / DataFusion 规划税。
- 改 BM25 `k1/b`、稳定倒排格式、对外 FTS API。

进门（相对现在只改这一层）：

```text
不偏斜 且 (2|3 或 256 宽)     → 一对                         // 不动
Auto leftover ≥3
  短语                         → lead-stream 逐篇             // 不动
  非短语 且 3×                 → score-first 核               // 不动
  非短语 且 leftover 且地板>0
      前 16 篇 score-first 探剪枝率
      ≥ 1/4 或 probe_n==0      → 剩余 score-first             // Hamlet / care / book-of-life
      < 1/4                    → 剩余 intersect-first         // LA / 多数 hot springs
  非短语 且 3× 关 且地板=0     → 逐篇                         // 堆空 balanced，不动
2 词偏斜                       → leapfrog                      // 不动
```

---

## 正确性约束

- Exclusive：`threshold == 0` 不剪。intersect-first 只许 **多 seek**，不许凭更松上界丢掉会进堆的篇。交上之后用精确分 `insert`。score-first 用 `lead+others_block_max` 提前丢掉的篇，精确分也不可能过 Exclusive 地板，所以 intersect-first 不会多 INSERT。
- compact 时不得用 lead=0 的 `cannot_compete` 当硬过滤。
- 公开分数仍是 query-order `f32` 累加，与现 score-first / classic 同 tie。
- 窗边界：probe 和剩余都在本窗 lead buffer 上做完再 leap；不要窗中途切回逐篇。
- 短语：`use_block` 仍 false。
- dump：943 条相对本 HEAD row+f32 bit-identical。

---

## 代码与测试落点

| 项 | 位置 |
|---|---|
| 判别 | `wand_lead_stream.rs` leftover `use_block` 窗：probe 16 后看 `floor_skip/lead_bm25` |
| 新路径 | 同文件：`and_lead_stream_intersect_first_block`（id compact，事后 BM25） |
| 计数 | `#[cfg(test)] lead_stream_intersect_first_blocks`；leap 计数仍在 caller |
| 不改 | `wand.rs` `Wand::search` Auto；`AND_SKEW_RATIO`；`and_score_first_for` |
| 现有测 | `auto_unskewed_legacy_wide_and_uses_score_first_with_shared_floor`：断言进了核（`score_first_blocks + intersect_first_blocks ≥ 1`），rows/kth = Off。**不要**再要求 `score_first_blocks≥1` 当唯一神谕 |
| 现有测 | `auto_score_first_block_leaps_over_gapped_follower_lead_blocks`：leap 仍发生 |
| 现有测 | `lead_stream_phrase_matches_classic_and_bulk`：短语 `score_first_blocks==0` 且 `intersect_first_blocks==0` |
| 新测 | leftover 4 词、共享地板>0、follower 权重大、probe 低剪：`intersect_first_blocks≥1`；rows/kth = Off |
| 新测 | leftover 4 词、共享地板>0、follower 权重小、probe 高剪：`intersect_first_blocks==0` 且 `score_first_blocks≥1`；rows/kth 一致 |
| 新测 | 3× 形状地板>0、即使 follower 上界很宽：`intersect_first_blocks==0` |
| 点名 | `cargo test -p lance-index --lib` 上述；各 &lt;1s |

`score_first_blocks` / `intersect_first_blocks` 只证明路径。正确性是 rows/kth bits。

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
5. bench         官方 LTO；必须同会话重跑 HEAD。钉 taskset -c 32。
6. 提交          通过 → commit。
                 不通过 → 仍然 commit，立刻 revert（留下这次尝试和数字）。
```

独立 **代码** review 的高后果 claim：

- 选核一行不改；3× 窗仍整窗 score-first。
- leftover 窗只按 probe 剪枝率分支，无 named if。
- intersect-first compact 不用 lead=0 上界丢掉 id。
- 短语永不进核。
- hit / kth bits 与 classic 一致。
- 不把 4+ 送回 bulk；不拆 3×。

---

## Bench 与提交

尺子：Wikipedia SBG A1 `TOP_10`，`taskset -c 32`，warmup 60s，10 iters min，全部 `LANCE_HACK_*` / `LANCE_DIAG_*` unset，**不要**设 `LANCE_FTS_BULK_AND`，LTO `do_query`。  
HEAD：`/tmp/sbg-lead-stream-h2a-head`。刀：`/tmp/sbg-lead-stream-h2a`。不要编进上一刀的 target dir。  
对照：本刀同会话 HEAD JSON。dump 基线：`.agent/fts-and-lead-stream-block-leap/knife-all.json`。  
产物：`.agent/fts-and-lead-stream-h2a-lead-bm25/`。

通过（全部，µs 相对 **本会话 HEAD**）：

- 943/943 dump 一致
- intersection / union / phrase 均值 **不回归**（约 ±3%）
- TOP_10 AVERAGE **不回归**
- `+walk +the +line` / `+university +of +washington` / `+time +for +kids` **不回归**（3×，约 ±6%）
- `+griffith +observatory` 约 ±6%（2 词，选核不动）
- **`+care +a +lot` 不回归**（杀开关。回归 → 整刀 revert，不要 named 例外）
- **Hamlet AND 不回归**（杀开关。吐回地板刀 −40% → revert）
- **`+the +book +of +life` 不回归**（杀开关。直方图 probe 全 keep；若误切 → revert）
- **正向通过线**：`+los +angeles +daily +news` 相对本 HEAD 快过 ±6% 噪声带，否则算 H2a 没付账。不要求回到 6106，不要求打到 Lucene 2583。`+hot +springs +south +dakota` 不回归即可，不要求变快。

不通过：**commit + revert**。LTO-off / `BULK_AND=on` / `taskset -c 0` / 混会话 JSON 不当通过线。

预期信封（不是通过线）：LA 收回大部分地板刀 +1034µs 里的 BM25 税；AVERAGE ~1µs，不当成功证据。

---

## 开干检查单

- [x] 独立设计检视 REJECT；P0（0.0 判别式在混合窗 no-op）用窗直方图交叉验证后改为 probe
- [x] P1：删 Backup B；leap / 计数在 caller；现有 leftover+floor 测不再死卡 `score_first_blocks≥1`
- [ ] `git status`：本刀 rust 干净；HEAD 仍是 `867ed042b`（若已前进，对照改成新 HEAD）
- [ ] 编码 → 测试∥review → dump → LTO → commit 或 commit+revert
- [ ] `.agent/fts-and-lead-stream-h2a-lead-bm25/note.md` 写下 SHA、dump、表、review 结论

---

## 检视吸收（2026-09-12）

独立 agent：**REJECT**。

**P0 吸收。** `cannot_compete(0.0, others)` 的 Exclusive 含义是对的（`wand.rs` `rejects_upper_bound`：`upper <= floor`），但它判别的是「lead 分 *永远* 剪不动」，不是「剪得够少，付 7.7 万次 BM25 不划算」。任何有一次 `floor_skip` 的窗都有 `followers_alone_lose == true`。直方图：`lose0_skip1=0`（单调性成立）；LA 只有 18% docs 落在 0-skip 窗；混合窗 `lose1_skip1` 才是 54412 次 BM25。0.0 公式按草稿编码会过杀开关、付不满正向通过线。改为窗内 16 篇 probe，阈值 `1/4`。

**P1 吸收。** (1) leftover+floor 现有测改成「进核 + rows/kth」，不把 `score_first_blocks≥1` 当路径神谕。(2) 新测覆盖低剪 → intersect-first、高剪 → score-first、3× 忽略开关。(3) 删 Backup B：`clause_bm25` 已走 `norm_k` / `exact_addends`。(4) 窗末 leap 留在 `and_lead_stream_search`，两条核都走。

**P2 不挡。** `factor(num_lists)` vs `n-1` 忽略。通过线仍是「快过 ±6%」，不写成「收回 1034µs」。

**未吸收 / 驳回。** 「在直方图出来之前不要编码 0.0」——已先量再改契约。Hamlet/care 在 probe 下不是风险主因（keep 质量 ≥50% 的窗）。
