# lead-stream block 核：窗末把 lead 同步到 max(follower.doc)

日期：2026-09-12  
施工基线：**干净 HEAD** `bench/community-fts-stack` @ `07b651fc1`（`perf(fts): admit leftover AND score-first once a floor exists`）  
对照：同会话官方 LTO。`lance-f03a2783c24f` 是 SBG **引擎 id**，不是 git SHA。上一刀 JSON 不能当本刀 HEAD。本刀 **必须** 在 `/tmp/sbg-lead-stream-block-leap-head` 重跑 HEAD，刀本身编进 `/tmp/sbg-lead-stream-block-leap`，两套二进制互不覆盖。钉核：**一个逻辑 CPU `taskset -c 32`**（物理核 16 的一条 SMT；不要 `taskset -c 0`，不要 `32,33`）。  
机器上可能有并发 cargo / 别的 SBG。Bench 前看 `loadavg` 和 CPU 是否空闲；**默认 paired 第二轮**。单条对照噪声带 ±6%。  
Lucene / Tantivy：`../lucene`、`../tantivy`。**只引 Lucene** `BlockMaxConjunctionBulkScorer` 窗末 `lead.advance(maxOtherDoc)`。Tantivy `block_wand_intersection` 窗末是 `doc = window_end + 1`，**不是**本刀先例（2026-09-12 独立复核 REFUTE）。  
相关：[fts-and-lead-stream-floor-score-first.md](./fts-and-lead-stream-floor-score-first.md)、[fts-and-lead-stream-score-first-block.md](./fts-and-lead-stream-score-first-block.md)

本文是实现契约。独立设计检视 2026-09-12：**APPROVE-WITH-CHANGES**，无 P0，三条 P1 已吸收（见文末）。编码以本修订为准。

---

## 一句话

不改 posting 存储，不改 Auto 选核，不改堆空 3× 门，不按「稀/密交集」加 if。`use_block` 窗走完 `and_lead_stream_score_first_block` 之后，用 follower 游标的 `max(current_doc_id())` 把下一窗的 `target` 从 `win_end+1` 抬到 `max(win_end+1, max_other_doc)`。逐篇路径已经有的 `Leap → target=next`，接到 block 核出口。

主证据是 `+los +angeles +daily +news`（进门后 +17%）。不是 AVERAGE，不是 Hamlet 再砍一刀，不是打到 Lucene 2583。

---

## 为什么（窗出口，不是选核）

三刀已经把 Lucene BMC 搭到「堆满后进同一只 score-first 核」：

```text
leftover ≥3 → 最短开车
非短语且 (3× 或 地板>0) → 一块 lead 上 score-first
窗末 lead.advance(max(other.docID()))  ← 本刀
```

进门刀自己的设计检视已把这一步登记为 Lucene 缺的半截（P1.3），本刀是兑现预告，不是事后编故事。稀和密 **不是** 进门条件。compact 之后 `max(follower.current_doc_id()) > win_end` 就是稀，`≤ win_end` 就是密（leap 变 no-op）。无 overlap 阈值。

Lucene（load-bearing，不是和外环 `windowMax+1` 重复）：

```199:205:/data/arrow/code/lucene/lucene/core/src/java/org/apache/lucene/search/BlockMaxConjunctionBulkScorer.java
    int maxOtherDoc = -1;
    for (int i = 1; i < iterators.length; ++i) {
      maxOtherDoc = Math.max(iterators[i].docID(), maxOtherDoc);
    }
    if (lead.docID() < maxOtherDoc) {
      lead.advance(maxOtherDoc);
    }
```

外环 `windowMin = max(lead.docID(), windowMax+1)`。follower 已在 500、lead 还在 128 时，没有 inner advance 就会把 129–499 走完。

Tantivy 窗末永远 `doc = window_end + 1`。不要写进「为什么」。

权威尺子（进门刀 `rerun-cpu32/`，`taskset -c 32`）：

| query | 进门前 HEAD | 进门后 | 本刀预期 |
|---|---:|---:|---|
| LA news | 6106 | 7140 | 明显快于 7140；中位回到 ~6106，不承诺 Lucene 2583 |
| `+hot +springs +south +dakota` | 1778 | 1924 | 同形状，+8% 收回附近 |
| `+american +academy +of +child +and +adolescent +psychiatry` | 1950 | 2036 | 同形状第三证人 |
| Hamlet AND | 46786 | 27902 | 单条 ±6% 内，不许吐回 −40% |
| walk / kids / university | 已在 3× 核 | 持平 | 单条 ±6%（本机短语对照曾 ±4–6% 噪声） |
| AVERAGE | 2094 | 2050 | 不回归；LA 整段只占 ~1µs |

---

## 做 / 不做

### 做（本刀全部）

1. 只改 `use_block` 分支在核返回之后的窗出口（`wand_lead_stream.rs` 约 534–538 行）。  
   同一窗的 16 篇切片全部跑完后，**读一次**（follower 游标单调，running max 等于最后一次）：

   ```text
   max_other_doc = max( lead[i].current_doc_id()  for i in 1..n  if Some )
   ```

   用 `current_doc_id()`，不要 `doc()`（后者可能为 freq=0 去解压块）。`lead[0]` 不计入。  
   窗结束且未耗尽：

   ```text
   target = max(win_end + 1, max_other_doc.unwrap_or(0))
   ```

   实际推进仍走下一圈已有的 `lead[0].next(target)`。核内不要另写 lead `advance`。

2. follower `current_doc_id() == None`：不计入 max。核已经因耗尽返回 true 时，整段搜索结束。

3. 块上界整窗 skip（`wide_max < threshold`）和 peek 空窗：**仍** `target = win_end + 1`。不是因为落后 follower 会错跳（落后只降低 max，`max(win_end+1, ·)` 保底）。限制是为了对齐 Lucene 在 `maxWindowScore < minCompetitiveScore` 时提前 return、以及把 diff 留在一个出口。单调 `win_end` 下套 max 在 skip 路径也正确，本刀不扩展。

4. `and_lead_stream_score_first_block` 的打分/剪枝/收割身体不改。不要改返回值类型。

5. `#[cfg(test)]` 计数 `lead_stream_block_leaps`：窗出口 `max_other_doc > win_end + 1` 时 +1。只证明 leap 发生，不当正确性。允许本机对 LA news 用**未提交**的一次性计数核对「leap 有没有打着」；禁止 `LANCE_DIAG_*` 产品路径。

6. 单测 + dump 943 + 官方 LTO。见落点与通过线。

### 不做

- 改 `Wand::search` Auto 选核、`AND_SKEW_RATIO`、`AND_SCORE_FIRST_COST_RATIO`、堆空 3×。
- 短语进 block 核。
- `overlap` / 词数 / 城市名 / named query 门。
- 把 LA news 送回逐篇。
- 给块上界 skip 套 `max_other_doc`。
- 改 BM25、数据布局、`win_end` 定义、分析树词数桶、`and_diag` / `LANCE_DIAG_*`。
- 规划税、MaxScore 收核、5b / IU / ReqOpt。

---

## 正确性约束

- Exclusive 地板、query-order `f32`、dump bit-identical：与进门刀相同。
- `max_other_doc = D` 来自某个 follower 在 compact 里 `next(target)` 或已经 `cur > target`。该列表在 `< D` 没有未处理的命中。跳过 `[win_end+1, D)` 的 lead 不会漏交。落后的 follower 不抬 max，只少跳。
- peek 不移动 `lead[0]`：`next(target)` 在下一窗头执行，等价于 Lucene 的 consume-then-advance。
- 单调：`target` 只增。`win_end` 随 target 严格不减。落后 follower 不能造成漏跳过的命中。
- 块路径当前窗仍会走完再 leap（和 Lucene 一样，比逐篇中途 `Leap` 少跳当前窗剩余）；本刀不补窗中 leap。

---

## 代码与测试落点

| 项 | 位置 |
|---|---|
| 窗出口 | `wand_lead_stream.rs` `and_lead_stream_search` 的 `use_block` 分支 |
| 调度 / 核身体 | `Wand::search` **不改**；`and_lead_stream_score_first_block` 打分循环 **不改** |
| 现有测 | 3× / 短语 / unskewed `limit=10` 与 `limit=None` / 共享地板：rows/kth 仍 = classic |
| 新测 | n=4 leftover、lead 两块、follower 在第一块之后才有下一命中、共享地板>0（整块一次进核）：Auto `score_first_blocks==1` 且 `block_leaps==1`，rows/kth = Off。无 leap 时应为 2 块 |
| 点名 | `cargo test -p lance-index --lib` 上述测；各 &lt;1s |

---

## 工作模式

```text
1. 编码          相对干净 HEAD；cargo fmt。
2. 测试 ∥ review 编码一结束就并行：
                   - cargo test -p lance-index --lib <点名>
                   - cargo clippy -p lance-index -- -D warnings
                   - 独立 agent 代码检视（未提交 diff；intent 用本文「做/不做」）
3. 收口          测试红或 review P0/P1 → 先修。P2 不挡 bench。
4. dump          943 条相对本 HEAD row+f32 bit-identical。
5. bench         官方 LTO；同会话重跑 HEAD。钉空闲逻辑核（默认 32）。
                 开始前记录 loadavg；CPU 被占则等或换核。
                 **默认做 paired 第二轮**（同一对二进制），第一轮只当预跑。
                 单条对照噪声带 ±6%；tag / AVERAGE 不回归。
                 leap 在 LA news 上没打着（计数或完全不比 7140 快）→ revert。
                 leap 打着了、LA 只收回 ~5% → **留下**（剩余是 3× 要挡的打分税，不是本刀证据）。
6. 提交          通过 → commit。
                 不通过 → 仍然 commit，立刻 revert。
```

独立 **代码** review 的高后果 claim：

- 选核一行不改；3× 堆空门不改；短语不进核。
- `max_other_doc` 只来自 `lead[1..]`，只在跑过 compact 的窗出口使用。
- 块上界 skip 仍 `win_end+1`。
- hit / kth 与 classic 一致。
- 不改存储 / 一对门 / 5b / IU / ReqOpt。

---

## Bench 与提交

尺子：Wikipedia SBG A1 `TOP_10`，**一个空闲逻辑 CPU**（默认 `taskset -c 32`；被占则换核并记录），warmup 60s，10 iters，hack 全关，**不要** `LANCE_FTS_BULK_AND`，**LTO** `do_query`。  
HEAD：`CARGO_TARGET_DIR=/tmp/sbg-lead-stream-block-leap-head`。刀：`/tmp/sbg-lead-stream-block-leap`。不要编进 `/tmp/sbg-community`、`/tmp/sbg-lead-stream-sf-floor`。  
HEAD 二进制可以拷贝当前 `07b651fc1` 的 LTO（`/tmp/sbg-lead-stream-sf-floor/release/do_query`，若确认就是该 SHA），但 **必须本会话重跑** JSON。  
对照：`.agent/fts-and-lead-stream-block-leap/head-run/results.json`。dump 基线：`.agent/fts-and-lead-stream-floor-score-first/knife-all.json`。  
产物：`.agent/fts-and-lead-stream-block-leap/`。

通过（µs 相对 **本会话 HEAD**，tag ±3%，单条对照 ±6%；**以 paired 第二轮为准**）：

- 943/943 dump
- intersection / union / phrase **不回归**
- AVERAGE **不回归**
- walk / university / kids / griffith ±6%
- Hamlet AND **不回归**（不许吐回进门刀的 −40%）
- `+care +a +lot` 不回归
- **`+los +angeles +daily +news` 快于本 HEAD**。期望回到 ~6106。不要求 Lucene 2583。
  判定拆开：leap 没打着 → revert；leap 打着、只收回噪声带内的一点 → 留下。
- `+hot +springs +south +dakota` 与 `+american +academy +of +child +and +adolescent +psychiatry` 不回归；期望同向收回

不通过：**commit + revert**。LTO-off / `taskset -c 0` / 高 load 且未做第二轮 / 混会话 JSON 不当通过线。

---

## 开干检查单

- [x] 设计和目标已经过独立 agent 检视；P0/P1 已吸收或书面驳回（见下）
- [x] HEAD 仍是 `07b651fc1`
- [x] 编码 → 测试∥review → dump → LTO（核空闲）→ 异常复核 → commit 或 commit+revert
- [x] `.agent/fts-and-lead-stream-block-leap/note.md` 写下 SHA、loadavg、dump、表、review

---

## 独立设计检视（2026-09-12）

Verdict：**APPROVE-WITH-CHANGES**（无 P0；三条 P1）。

| P1 | 处理 |
|---|---|
| 1. 「skip 路径套 max 会错跳」理由错误 | **吸收**。落后 follower 只降低 max。限制改成对齐 Lucene 早退 + 缩小 diff。 |
| 2. 单条 ±3% / 「停在 6800 就 revert」落在本机噪声带 | **吸收**。单条 ±6%；默认 paired 第二轮；leap 没打着才 revert，打着只收回 5% 则留下。 |
| 3. 缺第三条同形状证人 | **吸收**。表里加上 `+american +academy +of +child +and +adolescent +psychiatry`。 |

P2 已写进正文：`current_doc_id()`、窗后读一次、新测精确块数。P2 不挡 bench。
