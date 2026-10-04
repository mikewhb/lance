# IU PR 收敛计划：复用 merge_optional_freqs、合并收集入口、去冗余状态

> 状态：步骤 0-5 已完成（2026-10-04），步骤 6（943 dump + bench）待跑。
> 分支：`fts/iu-reuse-convergence`（自 `fts/iu-reuse-main`/`49d7e90e0` 切出），5 个新提交：
> `38dce4132` fmt+注释、`c7a004204` merge 复用、`be7563b77` IuCollector、
> `558ece25b` sticky_floor/fork 删除+边界测试、`e8171cde2` 懒频率对齐。
> 门禁现状：inverted 943 测试全过、clippy `-D warnings` 干净、fmt --check 干净。
> 基线：origin/main = 社区 main `7b97db782`；本 PR 已含 12 个提交（IU K=1 内核、C2、promotion、TermLeaf、K≥2 multi 内核、floor 剪枝、metrics、`LANCE_FTS_IU_TIGHT` 门控）。
> 日期：2026-10-04
>
> 执行中的修正（相对下方 §1 判定）：
> - 步骤 5 **不需要新增 freq 物化访问器**——`PostingIterator::doc()`（wand.rs:1160）
>   本身就是懒物化入口（freq==0 时自动 `ensure_compressed_block_ptr`）；只把 multi 的
>   对齐从 `next()` 换成 `next_doc_id(…,true)` + `current_doc_id()`，`doc()` 留给打分。
> - 对抗评审修正：merge 候选耗尽时提前 return，lead 游标可停在块内 stay 点——
>   promoted 的 lead seek 守卫是承重的，**不能**与 fork 一起删（已保留）。
> - 步骤 2 的「promotion 边界测试」在步骤 4 补齐：块边界 promotion、尾块耗尽
>   promotion、无 promotion 正常收尾、next_doc_id 懒物化计数（共 4 条新测试）。

---

## 0. 输入：外部检视意见（原文要点）

对 `49d7e90e0` 对比 origin/main 的独立检视（同时核对了架构文档及 `my/fts/classic-array-lead`）：

1. **SHOULD 批量匹配直接复用 `merge_optional_freqs`**（wand_maxscore.rs:109）：给定有序候选 docs，批量获得某条 optional posting 的频率，未命中填零。可删 `should_hits`/`scratch`/第二套归并逻辑；现缓冲大小取决于 doc-id 窗口内 SHOULD 命中数而非 MUST chunk 文档数，稀 MUST × 稠 SHOULD 时临时缓存可能很大。保留浮点累加顺序，补 promotion 游标边界测试。
2. **合并两套 IU 收集入口**（compound.rs:4727/4809）：提取 postings、检查分数、映射、插入、floor、overflow、metrics 全套重复。统一入口按 MUST 数量分支调用内核，不强泛化两内核、不新增 collector trait。
3. **重评 `collect_confirmed_window` 公共协议**（compound.rs:306 起，200+ 行框架面）：可尝试把窗口缓存留在 `TopKCollector::collect_mapped` 内。需要性能验证，不能机械删除；要保留 doc-local pruning / two-phase confirmation / 负分 / COUNT 契约。IU 内核不依赖该协议。
4. **清除两处冗余游标状态**：`WandCursor::sticky_floor`（wand.rs:6275，与 threshold 重叠）；`promoted` 的 `fork_from_start`（wand_iu_tight.rs:353，块消费后游标已在 start_doc 之后）。后者需先补整块/尾块/耗尽/边界测试。
5. **multi 对齐复用懒频率解码**：现有 `next_doc_id`/`current_doc_id` 可先只对齐 doc id，确认后再取频率。另：compound.rs:4724 "rarest required clause drives" 注释与代码不符。
6. **classic-array-lead 借鉴不替换**：arrayed 路径在 `Wand::search` 非 `Wand::next`；required-only TopK / floor 下推会漏 optional 进堆文档。IU 内核与 TermLeafScorer 保留。
7. **其他**：`.agent` 532 行提 PR 时移出；fmt 未过；性能替换需 A/B。

## 1. 逐条核实结论（对当前 HEAD 的代码验证，非转述）

- **1.1 merge 复用：接受，第一优先。** Compressed 路径块内双指针、miss 填 0；游标后置条件（stay 点/下一块首/终止）与手写循环一致，promotion 边界安全；位一致无忧（clause 序不变）。内存主张属实且更严重：现缓冲是窗口内全部 SHOULD 命中（O(窗口内稠密 df)），merge 只填候选位（chunk.len() 上界）。Plain 分支在 IU 路径是死代码（clauses_ready 已排除）。
- **1.2 合并入口：接受，改形态。** 统一入口需「拒绝并退回 leaves」的 Option 流（K=1 门槛失败要落回通用树），改动 dispatch 语义；改为抽 `IuCollector` 收集尾巴结构（collector/map_document/floor/overflow/comparisons + `hit()` 方法），两个薄入口各自构造，去重收益相同、不动 dispatch。
- **1.3 窗口协议：方向同意，本 PR 不动。** 200+ 行框架面属实、IU 内核不依赖属实；但它服务门槛失败回落的查询（恰是 5 条最差 IU 所在路径），`86d59f625` 从未孤立消融其收益；缓存挪进 collect_mapped 是转移复杂度而非删除。记为后续实验：先在 40 条 IU 套件消融窗口 drain 孤立贡献。
- **1.4a sticky_floor：接受，且发现行为差异。** main setter 是对 threshold 单调 max 单状态；现实现每次 set_min 把 threshold 重写回 `max(sticky,0)`，会抹掉 `raise_to_shared_floor` 刚抬的共享地板（下轮补回）。恢复 main 单调形式，删字段与 `apply_effective_threshold`。等价性：sticky 起始 0 只升、threshold 初始 0，恒等；共享地板交互下是改善。
- **1.4b fork_from_start：接受，先补测试。** `take_docs_one_block_upto` Compressed 后置条件 = 下一块首或 None；`last_doc` 在可见性 continue 之前更新，start_doc 后必无未消费 MUST 文档。fork+seek 可证 no-op（还省一次从头重 seek）。**SHOULD lead 的 seek 守卫保留**（merge 后 lead 可能停在块内 stay 点 < start_doc）。补四类测试：块边界 promotion、尾块 promotion、MUST 耗尽、lead 在块中。
- **1.5 懒频率对齐：方向接受，缺前提。** `next_doc_id(target,false)` 只解 doc-id 流（freq 懒填 0）；但确认后取 freq 需物化入口——现 `ensure_compressed_block_ptr` 是 unsafe 内部件，需新增小 `pub(super)` 访问器。与 bench 合并一轮 A/B。注释错误（4724）现在就修。
- **1.6 classic-array-lead：同意不替换。** 两坑核实成立。
- **1.7 fmt 属实（compound.rs 测试 import 一处）；`.agent` 文档提 PR 时移出。**

## 2. 落地顺序（每步独立提交，便于回退）

| # | 内容 | 验收 |
|---|---|---|
| 0 | 从 `fts/iu-reuse-main` 切 `fts/iu-reuse-convergence` | 分支就位 |
| 1 | fmt + compound.rs:4724 注释修正 | fmt --check 过 |
| 2 | merge 复用（删 should_hits/scratch/归并）+ promotion 边界测试 | dump 位一致；新测试过 |
| 3 | IuCollector 收集尾巴去重 | 测试过；行为不变 |
| 4 | 恢复 main 单调 setter（删 sticky_floor）；删 fork_from_start + 四类边界测试 | 测试过；dump 位一致 |
| 5 | 懒频率对齐 + freq 物化访问器 | 合成 12 条 A/B 不回归 |
| 6 | 统一验证：全量测试/clippy/fmt + 943 dump + 943 bench + 合成 bench | 无回归；预期持平或略好 |

**不在本计划内**：1.3 窗口协议收敛（后续实验）；classic-array-lead 替换（不做）；multi cost 门槛（等 dense×dense 反例 bench 数据）；943/合成 bench 重跑本身（floor 剪枝后欠的账，步骤 6 一并清）。

## 4.1 收敛轮净增账目（2026-10-04，诚实记账）

`git diff 49d7e90e0..HEAD --stat`：+366/−157（净 +209），分布：

| 文件 | +/− | 净 | 构成 |
|---|---|---|---|
| compound.rs | +170/−83 | +87 | IuCollector 结构体+方法样板 > 删掉的内联重复（**结构换行数，未净减**） |
| wand.rs | +140/−4 | +136 | 4 条新测试 ≈+115（预算内）；sticky_floor 删除 −15 |
| wand_iu_tight.rs | +118/−60 | +58 | merge 复用 −29，懒频率对齐 +15，边界测试相关注释/守卫 +余量 |
| 计划文档 | +95 | +95 | 不进社区 PR |

**结论**：本轮生产代码净增 ~94 行——真正的生产压缩杠杆是且仅是 §1.3 窗口收集协议
（~200+ 行），它挂在孤立消融前置上。因此**步骤 6 增补第四项实验**：窗口 drain 消融
（把 TopKCollector 的收集强制走无窗口路径做一版消融二进制），与 943 bench / 合成 12 条
同轮跑出数据，直接决定 §1.3 收不收敛——收敛则生产落回 1,200～1,400 预算带，
不收敛则按预期落 2,500～2,800 总量带（检视双方对该前提的判断一致）。

## 4. 规模预算（来自此前高阶 AI 的预期，2026-10-04 记录）

预期：**保留全部覆盖，社区 PR 最终新增约 2,200～2,600 行**（按 diff 新增行计，含注释和空行），
其中生产代码约 1,200～1,400 行、测试约 1,000～1,200 行；内部交接文档不进社区 PR。
主要压缩对象是窗口收集协议及重复循环、SHOULD 缓存归并、重复收集逻辑和冗余状态；
**测试不作为主要压缩对象**。关键前提：把窗口优化收回原 collector 后性能仍然成立；
若必须保留专用 drain 协议，总规模更可能在 2,500～2,800 行。
合理目标：约 1,300 行生产代码 + 约 1,100 行测试，且尽量不扩展公共 scorer 接口。

当前实测（HEAD `e8171cde2`，`git diff origin/main..HEAD --numstat -- rust/`）：

| 文件 | 新增 | 删除 |
|---|---|---|
| compound.rs | 1,541 | 83 |
| wand.rs | 594 | 4 |
| wand_iu_tight.rs | 466 | 0 |
| wand_term_leaf.rs | 222 | 0 |
| scorer.rs | 5 | 0 |
| **rust 合计** | **≈2,828** | **≈87** |

生产/测试拆分以 be7563b77 检查点（此前 AI 实测：生产 ≈1,609 / 测试 ≈1,094）为基准，
加上步骤 4/5 的增量（测试 +~115、生产 ±0 净）：**生产 ≈1,620 / 测试 ≈1,210**。

与预算的差距：生产超预算约 220～420 行，主要来源即 §1.3 的窗口收集协议
（trait 方法 + 默认 drain + 三处转发 + TopKCollector 双路径，约 200+ 行）。
该收敛已按本计划延后，**前置条件是先孤立消融窗口 drain 的收益**（40 条 IU 套件）；
若消融证明收益可由 collector 内窗口缓存等价实现，生产即可落入 1,200～1,400 预算带；
若必须保留专用 drain 协议，则按预期落在 2,500～2,800 总量带。

## 3. 验证命令备忘

```
RUSTC_WRAPPER= cargo clippy -p lance-index --lib --tests --no-deps -- -D warnings
RUSTC_WRAPPER= cargo test -p lance-index --lib inverted::
cargo fmt --all -- --check
# 943 dump / bench / 合成 12 条：复用 .agent/fts-community-pr-handoff 下 gate 脚本与 /tmp 复现台
```
