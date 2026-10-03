# IU（intersection_union）代码框架分析：与 main 已有机制的重叠、可简化点与收敛方案

> 分支：`fts/reqopt-window-leftovers`（HEAD `a20371e4b`）
> 基线：`ecb388f73`（main 侧 2026-09-01，即本分支 fork 点）
> 测量环境：本机 Zen4、`/dev/shm/sbg` 索引、943 条 SBG TOP_10、单核、取多轮 min
> 本文是架构分析文档，不含行为变更；所有性能数字均可按附录命令复现。

---

## 0. 现状一句话

IU 形状（`1 个 MUST + N 个 SHOULD`）对 Lucene 10.3.0 已从 **1.50x 收敛到 1.15x**，
最坏查询从 **5.2x 收敛到 2.39x**；但仍有 4–5 条查询停在 2.0–2.4x。
本文回答：**除了 skew 之外，IU 相关代码里还有哪些可以简化、或直接复用 main 已有机制？**

---

## 1. IU 查询的生命周期：它横跨两个引擎层

IU 查询（例：`solar +panel rebates`，MUST=`panel`，SHOULD=`solar`+`rebates`）
随 heap floor 上升会**在语义上退化成交集**。这决定了它天然横跨两层引擎：

```
 heap floor（第 10 名分数）
   ▲
   │  A 段：floor 还低                    B 段：floor 越过 MUST 全局上界
   │  「MUST 自己就能进堆」                「MUST 单独没希望 → 退化成交集」
   │
   │   ┌─────────────────────────────┐    ┌─────────────────────────────┐
   │   │ 自研 iu_tight 内核 (362 行)   │    │ main 已有的 AND 内核 (3 种)    │
   │   │  MUST 块驱动                 │───→│  bulk SIMD / lead-stream /   │
   │   │  + SHOULD 逐窗批量解码 (C2)   │提升 │  classic leapfrog            │
   │   └─────────────────────────────┘    └─────────────────────────────┘
   │
   └────────────────────────────────────────────────────────────────→ 扫描进度
```

**关键认识：IU = 「有一个必选子句的 OR」。**
A 段必须保留 ReqOpt 语义（MUST 必须命中，SHOULD 任意命中即可）；
B 段在语义上就是纯交集（或“交集 + 剩余 optional 加总”）——
**B 段可以直接落在 main 已有的 AND 内核上**，这是最大的可复用点。

---

## 2. 证据：SHOULD 探测是我们唯一落后的地方

对照实验（同机、min-of-30，逐步加 SHOULD 子句）：

```
查询                        lance      lucene    比值
+panel（仅 MUST）            368 µs     1205 µs   0.31x   ← 我们快 3.3x
+panel solar（+1 SHOULD）    1087 µs    1248 µs   0.87x
solar +panel rebates（+2）   1767 µs    1204 µs   1.47x

每加一个 SHOULD 的边际成本：   +700 µs    +0~43 µs
```

`+panel` 单独跑我们**快 3.3x**（368 vs 1205µs，26679 个 MUST 候选）；
每加一个 SHOULD 我们固定多付 **~700µs = 26679 候选 × 27ns 的逐候选 posting seek**，
而 Lucene 的 optional 评估几乎免费。**差距 100% 来自 SHOULD 探测方式**，与 MUST 驱动本身无关。

（对照组：`small +business grants` 我们 0.29x——MUST=181k 高频、SHOULD 更高频，
我们的窗口批量解码占优。说明不是内核本身慢，而是「SHOULD 密度 × MUST 窗口跨度」的组合。）

---

## 3. 重叠地图：本分支新增 vs 基线已有机制

基线 `ecb388f73` 已存在的机制（`git grep` 确认）：
`ReqOptScorer`、`ShouldMaxScoreScorer`、`maxscore_search`、
**`score_single_essential_upto` + `merge_optional_freqs` + `take_docs_one_block_upto`**
（MaxScore 内层窗口：解码一个子句的块、对其他子句 two-pointer）、
`WandCursor`、`bulk_and_search`。

```
本分支新增 (~880 行)                       基线 (main 侧) 已有
────────────────────────────             ────────────────────────────
wand_iu_tight.rs (362)
 ├ 主循环 + C2 窗口批量解码   ←─ 形似 ─→   score_single_essential_upto
 │  (MUST 块 + SHOULD 归并)              + merge_optional_freqs         ①可部分复用
 │                                       (MaxScore 内层窗口)
 ├ promoted 交集 (75 行)     ←─ 形似 ─→   and_bulk_search (2-way SIMD)   ②可复用
 │  (MUST + 提升的 SHOULD 都必命中)       classic / lead-stream
 └ required_shoulds 成本门   ←─ 重复 ─→   BulkAndMode::enabled_for       ④可合一
compound.rs
 ├ 路由三重门 (~80 行)       ←─ 重复 ─→   (三处独立的成本倾斜判据)        ④可合一
 ├ ReqOptScorer 提升/交集                ReqOptScorer（基线已有，只加了提升）
 └ leaf_admits_term_leaf     真新，无对应
wand_term_leaf.rs (222)     真新，无对应（刻意精简的叶 scorer）
```

---

## 4. 四个可简化点（按「减代码 + 收益」排序）

### (a) 提升后直接交给 main 的 AND 内核 —— 唯一「减代码 + 可能提性能」

```
现在：promoted() 手写 lead 驱动交集（~75 行，裸 PostingIterator 上第三个 leapfrog 副本）
改为：1 MUST + 1 SHOULD 提升后 = 纯 2 子句 AND，且无剩余 optional
      → Wand::new(Operator::And, [must, promoted_should]) → and_bulk_search
        （2 子句 SIMD 归并是 main 里最快的 AND 路径）
1 MUST + N SHOULD (N≥2)：提升后仍剩 N-1 个 optional，才需要保留手写路径
```

- **直接打剩下的 >2x 查询**：`+global markets`、`electric +vehicles` 都是 1+1 形态。
  `promoted` 目前对每个 lead doc 做 `iu_tight_seek_eq(&mut must, doc)`——正是 §2 点名的
  逐候选 seek 模式；`and_bulk_search` 的 2 子句内核是 SIMD 块流，理论上更快。
- **实现陷阱（review 发现，必须先解决）**：
  1. **floor 无法带过去**：`Wand::search` 自带内部 collector，`Wand::new` 把 `threshold`
     硬置 0.0 且无公开 setter；`collect_iu_tight` 的活 floor（`Cell`）会丢，
     需要 threshold-seeding API 或接受重新填充的成本。
  2. **耗尽的 posting 会被 `Wand::new` 静默丢弃**（`current_doc_id()==None` 跳过）：
     提升时 promoted SHOULD 可能已耗尽（`iu_tight_required_shoulds` 不检查游标状态），
     1+1 的 AND 就退化成 MUST 单子句走 → **错结果**。必须显式守卫。
- **可行的部分**：游标状态（`index/block_idx/current_doc`）随 iterator 按值移动 ✓；
  3x 成本门保证被提升的 SHOULD 最稀有，`Wand::new` 的 cost 排序会把它放进 lead ✓；
  2 子句 f32 加法可交换，位一致 ✓。
- **验证**：943 位一致门禁 + IU 40q A/B + 上述两个陷阱的专项测试。

### (b) SHOULD 并集窗口界复用 `ShouldMaxScoreScorer`（基线已有）

ReqOpt/内核里对 SHOULD 并集的界是手写的；基线这个 scorer 已实现「并集 + 全局界」。
纯减行数（~20–30 行），需确认全局界不会让 C2 的窗口剪枝失效（C2 用的是逐窗界，更紧）。

### (c) 三套路由门合一

`iu_tight_should_switch`（3x 成本门）、`BulkAndMode::enabled_for`（skew 比值门 + 4+ impacts 臂）、
`leaf_admits_term_leaf` 是三处独立写的「子句成本倾斜」判据。但三者常量与语义不同
（`IU_TIGHT_LEAD_COST_RATIO=3` vs `BULK_AND_AUTO_SKEW_RATIO=32` 且后者只对 3 子句生效 vs
前者是计划形状谓词）——合一至少改变一个阈值决策，**是语义变更而非机械清理，需 A/B**。

### (d) C2 的 SHOULD 归并按密度复用 `merge_optional_freqs`

基线的 `merge_optional_freqs(docs, out_freqs)` 就是 two-pointer 归并；其 Compressed 路径
（IU 门要求 `is_compressed()`，Plain 路径不可达）与 C2 一样是块连续解码、语义完全等价
（未命中记 0、游标落在窗口之后），并且把解码+归并融合成一次调用。
**这是四个点里最安全的一个：可直接复用，C2 的解码+归并 ~35 行 → 1 次调用**，需 A/B 确认不掉点。

---

## 5. 不能压缩的部分（以及为什么）

| 代码 | 行数 | 为什么是真新机制 |
|---|---|---|
| `wand_iu_tight.rs` 主循环 + C2 | ~320 | MUST 驱动 + 逐窗批量解码是 IU 专属语义（MUST 必须命中），
main 的 MaxScore 是 OR 语义（只匹配 optional 也计分），不能直接套 |
| `wand_term_leaf.rs` | 222 | 刻意去掉 WAND 机器的精简叶 scorer，`WandCursor` 无法替代
（88933c0dc 实测换叶 −43.5%） |
| 799 行测试 | 799 | 943 位一致门禁之外的正确性保障：4 个 iu_tight 语义测试、
lead-stream 三方差分、rounding 边界、可见性测试（含 wand_iu_tight.rs 内 59 行测试模块） |

**内层循环绝不能统一**：我们测到的收益全部来自特化——
bulk 的 SIMD 切片归并（稠密 2-3 子句）、lead-stream 的 follower seek（倾斜 3 子句）、
IU 的窗口批量解码（SHOULD 比 MUST 稀疏）。统一内层循环会把赢全吐回去
（window-bound 实验：稀疏 SHOULD 的块横跨上千 doc，其块最大值≈全局上界，剪不动）。

---

## 6. 框架视角：3 个正交轴，6 个实例

把 AND/IU 家族的全部实现摆在一起，只有 3 个正交轴：

```
                         候选产出
                逐 doc next()   lead 块解码   N-way 块归并
lead 选择      ┌───────────────┬─────────────┬──────────────┐
成本排序(首个)  │ classic       │             │ bulk N-way   │
按成本最稀有    │ score-first    │ lead-stream │              │
floor 触发提升  │ ReqOpt 交集    │ promoted*   │ (a) 提升后交 AND │
              └───────────────┴─────────────┴──────────────┘
矩阵未列的近亲：flat_search（AND 家族产出器）、MaxScore 内层窗口
score_single_essential_upto（§3①的孪生，OR 语义）
剪枝界（第三轴，竖切所有格子）：全局 ub / 块级 / 窗口多子句和
* promoted = iu_tight 提升后的 lead 驱动交集
```

| 实现 | lead 选择 | 候选产出 | 剪枝界 |
|---|---|---|---|
| classic leapfrog | 固定首个 | 逐 doc `next()` | 块级 |
| bulk N-way | 固定（全解码） | N-way 块归并 | 窗口多子句和 |
|   （另有 4+ 子句且全部带 impacts 的附加臂） | | | |
| lead-stream | 按成本最稀有 | lead 块解码缓冲 | 窗口多子句和 |
| score-first AND | 按成本排序 | 逐 doc | 前缀和 |
| iu_tight + promoted | floor 触发提升 | MUST 块 + SHOULD 批量解码 | 全局 ub |
| ReqOpt 交集 | floor 触发提升 | 逐 doc leapfrog | 窗口和 |

**本轮所有「skew」任务（9175 稀有 lead、classic-AND-for-skew、稀有 lead 块流式、
required SHOULD walk、iu_tight 提升）都是这张矩阵的不同格子。**
过程教训：任务下发前先画这张矩阵，AI 会产出一个参数化实现 + 一个策略函数，
而不是 6 份特例 + 6 份重复的产出尾巴。

重复的量化（emit tail = score→phrase 确认→floor 检查→insert→threshold 更新）：

```
（全部位于 wand.rs；iu_tight 内核经 on_hit 回调产出候选，不在其中）
candidates.insert(                    ×6   ┐
check_positions(                      ×11  │ 这条「产出尾巴」在 wand.rs 的
exclusive_score_cannot_beat_floor(    ×6   ├ classic / bulk / lead-stream /
update_threshold(kth                  ×6   ┘ maxscore 各复制一份，
iter_term_freqs()                     ×3      约 150–180 行 → 可提 1 个 helper
```

---

## 7. 决议（2026-10-03，经两轮独立 agent 质证后定稿）

| 项 | 决议 | 依据 |
|---|---|---|
| push IU PR(`fts/iu-reuse-main`,7 提交) | 按指示**暂不 push**,仅本地保留 | 用户指令 |
| K≥2+N 泛化 | **搁置**,等真实负载证明需要再做;方案已定型(option c:
  `iu_tight_leaf_indices` 放宽到 `must.len()>=1` + 内核扩展 K 个 MUST 求交,~100 行,不碰 trait) | 943 基准里 K≥2+N 查询为零;
  Lance 真实过滤模式是 RowAddrMask 预过滤,不走 MUST 子句 |
| ReqOptScorer 提速改造 | **不做** | BoxScorer 无块解码接口,三条替代路线均侵入
  (下转逃生口/17 个实现方全改/构造点路由);
  且即使做了内核仍领先(预解码窗口+归并 vs 逐候选虚调用) |
| 统一内层循环 | **不做** | 实测收益(−85.3%)全部来自特化 |
| emit-tail 提 helper | **降级、暂不做**:重新量后真正逐位相同的只有
  `insert+kth+update_threshold`(~6 行 ×6 处 ≈30 行),score 计算各不相同;churn 不值得 | — |
| count-mode 合并期义务 | **已钉死**:floor=−inf 时内核与 ReqOpt 路径都必须枚举全部 MUST doc
  (count 正确性不变量),测试已加在 `fts/iu-reuse-main` | 唯一发现的合并期义务 |

已推送:`fts/reqopt-window-leftovers` @ `a20371e4b`(含 skew,供 9175 参考)。
本地保留:`fts/iu-reuse-main`(社区 main + IU,7 提交),未推送。

---

## 附录：复现命令

```bash
# 二进制（每次改动约 11 分钟）
cd search-benchmark-game/engines/lance-iu2
CARGO_TARGET_DIR=/data/arrow/code/sbg-targets/iu2 \
  RUSTFLAGS='-C target-cpu=native' cargo build --release --bin do_query

# 943 位一致门禁
python3 /tmp/iu2_harness.py dump <do_query> /dev/shm/sbg/indexes/lance-f03a2783c24f \
  /tmp/q943.txt /tmp/out.txt && diff /tmp/out.txt /tmp/dump_iu2.txt

# IU 40q / 全量 943 A/B（持久进程、逐行计时、多轮交错取 min）
python3 /tmp/iu2_harness.py bench <do_query> $IDX /tmp/iu_q.txt out.json 15 3

# Lucene 对照
/tmp/lucene_do_query.sh   # JDK21, engines/lucene-10.3.0, idx -> /dev/shm/sbg/indexes/lucene-10.3.0

# 词频（df）
printf 'COUNT\t+panel\n' | <do_query> $IDX
```

单元测试：`cargo test -p lance-index --lib inverted`（919 pass）。
