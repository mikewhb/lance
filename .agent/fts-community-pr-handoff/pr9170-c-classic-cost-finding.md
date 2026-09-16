# PR 9170 Plan C —— classic 劣化定位：构建配置是关键变量

日期：2026-09-16
目标：定位并修掉 **patched(`eabbcfe13`) 相对 base(`a11b817ad`) 的 ~15% 微基准劣化**

## 1. 术语对齐（避免误读）

commit 链：`a11b817ad` → `3fbdb7739` → `eabbcfe13` → `f38bff929` → `d7f3bfd1a` → `27f996a2c`

| 名词 | commit | 说明 |
|---|---|---|
| **base** | `a11b817ad` | 上游 main，reviewer 的验收基准 |
| **old** | `3fbdb7739` | PR 9170 原样（rarest-term-first）；分支名 `pr9170-rebased` / `fts/phrase-rarest-anchor` |
| **patched** | `eabbcfe13` | 待提交补丁（restore anchor skipping）；分支名 `fts/phrase-v3-squashed`，.5 文档里的 "follow-up"/"final3" 也是它 |
| cand | `27f996a2c` | patched + 两个 bench commit + `#[inline]` 实验 |

base / old 自身**没有** `wand_phrase_bench.rs`（基准由 `f38bff929` 引入），
所以本机把同一对 bench commit cherry-pick 到 base / old 上，四个 revision 共用一把尺子。

## 2. 关键发现：.5 的微基准是 thin LTO + codegen-units=1 构建

`.5` 的 `pr9170-final-benchmark-reviewer-response.md` 明确写了最终可执行文件的构建方式：

> built in `sbg-targets/mb4-final-{base,orig,patched}` with Rust 1.97.0, `--release`,
> **thin LTO, `codegen-units=1`, `opt-level=3`**, and `RUSTFLAGS='-C target-cpu=native'`.
> Cargo fingerprints agree on rustc (`18169182855791486453`), features (`[]`),
> **profile (`8681002757401828468`)**, and rustflags.

| 构建 | profile 指纹 | LTO | codegen-units | opt-level |
|---|---|---|---|---|
| 本机首轮 `sbg-targets/pr9170-*` | `13952719172529475294` | 无 | 16（默认） | 3 |
| **.5 `sbg-targets/mb4-final-*`** | **`8681002757401828468`** | **thin** | **1** | 3 |

rustc 指纹两边一致（`18169182855791486453`），**但 profile 不同**。

> ⚠️ **本机首轮的结论无效**：首轮用无 LTO / cgu=16 构建，测出 patched 相对 base
> 在 classic 上持平（0.995–0.998×）。那不是 .5 的那套构建，不能用来否定 .5 的 +16%。
> 必须用 thin LTO + cgu=1 重测。

（注：.5 自己两份文档给出的 profile 值互相矛盾 —— `pr9170-handoff.md` 写
`1783587453833569552`，`pr9170-final-benchmark-reviewer-response.md` 写
`8681002757401828468`，而 plan §4.4 要求"确认为 `1783587453833569552`"。
以 reviewer response 那份（`8681002757401828468`，含明确构建参数）为准。）

## 3. 首轮（无 LTO）已经排除的东西

这些结论与构建配置无关，仍然有效：

### 3.1 patched 在 classic 路径上没有额外算法开销

`perf stat`，`early_hit_n16384` / classic 路径（整个 bench 进程，三个 revision 同参数）：

| rev | instructions | branches | branch-misses |
|---|---:|---:|---:|
| base | 315,779,200 | 58,507,474 | 0.40% |
| old | 315,632,238 | 58,467,474 | 0.39% |
| patched | 315,580,294 | 58,460,608 | 0.39% |

patched 比 base **少 0.06% 指令、少 0.08% 分支**。→ 不是"多做了工作"。

### 3.2 PR 原样的灾难在本机复现得更强，且 patched 完全修掉

7 批次，revision 顺序随机，p50（ns）：

| fixture | base | old | patched | old/base | patched/base |
|---|---:|---:|---:|---:|---:|
| `early_hit_n2048` | 7190 | 7200 | 7151 | 1.001× | 0.995× |
| `early_hit_n16384` | 56329 | 56359 | 56198 | 1.001× | 0.998× |
| `disjoint_n16384` | 56370 | **393859** | 56208 | **6.99×** | 0.997× |
| `strided_miss_n16384` | 1360946 | 395020 | **168276** | 0.290× | **0.124×** |

（.5 记录：disjoint base 91,863 / old 436,203 = 4.75×；strided base 1,365,006 /
old 436,374 / patched 224,444。方向上完全一致，本机 disjoint 劣化更严重。）

### 3.3 `27f996a2c` 的 `#[inline]` 是无效改动

cand 与 patched 在所有 fixture 上差 ≤0.1%。按 plan 的纪律应记为"未验证/无效假设"，
不要并入待提交补丁。

### 3.4 已排除：`#[cfg(test)]` 计数器污染基准

`exact_phrase_scan` 循环里的 `count_anchor_steps()`、`position_cursor()` 里的
`POSITION_CURSOR_CALLS` 确实在基准（`#[test]`）里执行、生产构建里没有，
但调用次数是 O(clause) 或 O(anchor steps)，而这些 fixture 的 anchor 步数极少
（`early_hit` 1 步即命中，`disjoint` 1 步即跳完），量级为纳秒。**不是 16% 的来源。**

## 4. 下一步（进行中）

1. **用 thin LTO + codegen-units=1 重编 base 与 patched**，重跑 §3.2 的矩阵
   - `sbg-targets/pr9170-lto-base`、`sbg-targets/pr9170-lto-patched`
   - 目标：确认 +16% 是否在该配置下出现。若出现 → 劣化是**代码生成/内联产物**，
     可以定位到具体内联决策并修掉；若不出现 → 说明 .5 的数值另有来源
     （例如 CPU 27 vs 32 的差异、或未轮换 revision 顺序的系统性偏差 ——
     plan §Workstream 1.3 就要求核对历史脚本是否真的轮换）
2. 若 LTO 下复现：对比两者的内联/符号分布（`perf record` 看 `exact_phrase_scan` /
   `advance_to_at_least` / `partition_point` 的出现形态），找出 patched 在
   cgu=1 下失去的内联或新增的代码布局代价
3. `"york photo"` 1.342× 离群点：需要 engine 构建（base 与 patched 两个 do_query），
   按 plan §Workstream 4 做随机顺序 + 多批次的隔离协议

## 4a-二次更正. op-cache 解释**被推翻**；逐指令归因指向主循环的分组检查

### ❌ 我先前的"op_cache_miss 3.5×"是**计数器误读**

补测 `de_src_op_disp.*`（取指来源分解）后发现三个构建**完全相同**：

| | de_src_op_disp.all | .op_cache | .decoder | .loop_buffer |
|---|---:|---:|---:|---:|
| base | 352.8M | 348.5M（**98.8%**） | 4.37M | 0 |
| P0（坏） | 353.9M | 349.5M（**98.8%**） | 4.37M | 0 |
| P1（好） | 353.0M | 348.7M（**98.8%**） | 4.29M | 0 |

**op cache 在坏构建里同样服务 98.8% 的 op** → 前端取指不是原因。
我之前用的 `op_cache_hit_miss.op_cache_miss`（base 就有 366M，≈ 退役指令数）**不是"未命中"**，
是计数器语义理解错误。**该行结论作废。**

### ✅ 逐指令归因（537 条指令，固定周期采样，样本数正比于 cycles）

`perf record -e cycles -c 20000`，12 轮；总样本 base 16,831 / P0 23,121（1.374×）。
增量分布：`seek_packed_doc_positions` **+5,733（占总增量 91%）**，`decompress_to` +468（7.4%）。

对该函数做逐指令归因（两侧**归一化后 537 条指令序列完全一致**）：

| Δ百分点 | base% | P0% | 指令 |
|---:|---:|---:|---|
| **+13.78** | 1.13 | **14.91** | `jne`（循环回边） |
| **+11.70** | 0.40 | **12.10** | `mov %r12d,%eax` |
| **+7.79** | 10.79 | **18.58** | **`cmp (%rdi),%rbp`** ← 每位置一次的加载比较 |
| +2.69 | 0.14 | 2.83 | `mov 0x10(%rdi),%rbp` |
| +2.36 | 1.86 | 4.22 | `add %esi,%edx` |
| +1.45 | 4.09 | 5.54 | `jb` |

`cmp (%rdi),%rbp` 对应源码里的 `if *unpacked_group_idx != Some(group)`——
**它在每个位置都执行，但 group 每 `BLOCK_SIZE`(=128) 个位置才变一次**。

### ✅✅ 由逐指令归因得到的**真正修复**：把分组检查提出逐位置循环

`seek_packed_doc_positions` 原来每个位置都做 `index / BLOCK_SIZE` 与
`*unpacked_group_idx != Some(group)`，而 group 每 128 个位置才变一次。
改成 **group 外层 / 位置内层**（分支 `probe/group-outer-loop`，commit `a2c76cbc0`）：

| 构建 | cycles | instructions | load_not_complete | 热函数 mod64 |
|---|---:|---:|---:|---:|
| base | 77.21M | 356.68M | 32.1M | 0 |
| P0（坏） | 104.23M | 356.54M | 48.2M | 48 |
| P1（对齐） | 77.92M | 356.51M | 32.4M | 0 |
| **GOL** | **56.91M** | **252.93M（−29%）** | **23.3M** | 32 |

壁钟（7 批次，p50）：

| fixture | base | P0（坏） | **GOL** | GOL/base | GOL/P0 |
|---|---:|---:|---:|---:|---:|
| `early_hit_n16384` | 70,188 | 101,128（1.441×） | **46,369** | **0.661×** | 0.46× |
| `disjoint_n16384` | 69,898 | 100,918（1.444×） | **46,009** | **0.658×** | 0.46× |
| `sparse_hit_n16384` | 70,199 | 101,127（1.441×） | **46,029** | **0.656×** | 0.46× |
| `late_hit_n16384` | 81,428 | 118,277（1.453×） | **53,728** | **0.660×** | 0.45× |
| `mc4_hit_late_n2048` | 16,429 | 23,229（1.414×） | **10,760** | **0.655×** | 0.46× |
| `strided_miss_n16384` | 1,374,167 | 216,776（0.158×） | **158,986** | **0.116×** | 0.73× |

**GOL 比 base 快 34%，比坏版 patched 快 2.2×，且未使用任何对齐指令**
（热函数落在 mod64=32）。

### GOL 的 e2e 口径（全量 943 条）

| engine | median-of-p50 µs | 几何平均比 | 胜出条数 |
|---|---:|---:|---:|
| base | 1179 | 1.000× | 222 |
| nofix（patched） | 1178 | 0.990× | 346 |
| **GOL** | 1177 | **0.989×** | **375** |

- **TOP_10 差异 0 条**（943/943 三引擎逐位一致）
- 最差 nofix/base = 1.072× → 无离群点
- ⚠️ **口径**：GOL 的收益集中在位置解码密集的 phrase 查询（microbench 0.46× vs P0、
  0.66× vs base），全量 943 条里被大量无位置的 union/intersection 摊薄到 ~1%。
  对外表述必须写清这一点，不能说"全量提升 34%"。

### 独立检视结论（对抗式审查，`general-purpose-1`，12 分钟）

**✅ 语义等价，未发现分歧**，且是证明而非断言：

- 映射证明：`index ∈ [group·128,(group+1)·128)` ⇒ `first_in_group = index−group·128`；
  位置 `index..group_end` 对应偏移 `first_in_group..first_in_group+(group_end−index)`，
  且 `group_end ≤ (group+1)·128 ⇒` 最大偏移 ≤ 127 → **不越界**；
  `group_end > index` → 内层区间非空不倒置；`index` 严格前进 → **可终止**；
  `Σ(group_end−index)` 望远镜求和 = `full_end−start`，与原 `start..min(end,packed)` 一致；
  `(group+1)·128 ≤ packed ≤ total_deltas` → **无溢出**；
  错误抛出的分组/位置与原实现相同 → 出错时 `dst` 前缀状态一致。
- **穷举暴力验证**：全部 **36,361,101** 个 `(total≤600, start, end)` 组合，
  比对发出的 `(group, offset)` 序列 → **零分歧 / 零越界 / 零死循环**。
- 既有随机测试 `test_packed_position_doc_seek_matches_block_decode`（`encoding.rs` 测试）
  已覆盖中途开始、最后不完整分组、纯尾、缓存复用；该模块 2 个测试通过。
- 残余风险（无法排除）：跨调用的缓存有效性依赖调用方在 `position_block_idx` 变化时
  重置 scratch（`wand.rs:1238-1255`，当前正确）；未来若有调用方跨 block 复用 scratch，
  两个版本会**同等**出问题（不是本次改动引入的）。

**⚠️ 方法论修正（重要）**：我插的计数器位于**计时热循环内部**（每位置一次 TLS 写），
所以 **`cnt-*` 构建的耗时不可用于性能结论**。实测插桩开销：
`clean_gol` 46,269 ns vs `cnt_gol` 100,297 ns = **2.17×**。
本文件所有性能数字均来自**无插桩**构建（`probe-gol` / `pr9170-lto-*`），`cnt-*` 仅用于取计数。

**ℹ️ 表述修正**：`BLOCK_SIZE = 128` 时 `/`、`%` 本就会被强度削减成移位，
所以 commit message 与下文的"removes a division"**说过头了**——真正被外提的是
`unpacked_group_idx` 的**加载比较**与分支。若 GOL 要正式提交，commit message 需改写。

**补充的边界测试（已实现，commit `b3320b0ab`）**：新增
`test_seek_packed_doc_positions_boundary_ranges`，覆盖

1. ✅ 空区间（`0..0`、分组边界上、tail 内）→ 不发位置、不报错
2. ✅ 文档起点恰在分组边界（形状 `[128, 72, 63]`）且 scratch 仍缓存着上一个分组
3. ✅ 文档跨越分组边界（形状 `[200, 63]` 的 doc 0）
4. ✅ 文档中途开始并延伸进 varint tail（形状 `[200, 63]` 的 doc 1）
5. ✅ `start > end` 与 `end > total_deltas` 的校验错误

**关键**：该测试在**原实现（nofix）上也通过** → 它 pin 的是两版共享的语义，
不是把 GOL 的行为写死，因此是合格的语义保持回归测试。
`cargo test -p lance-index --lib` → **1538 passed / 0 failed**。

检视提出的另外两条**经分析不可达**，故未写成测试（写了会变成假测试）：

- **"中途报错且 `dst` 非空前缀"**：`group_offsets` 的扩展循环在**任何解码之前**
  就把请求范围内的所有分组走了一遍（`encoding.rs:818-822`），任何坏分组都会在扩展阶段
  报错，此时 `dst` 必然为空（入口已 `clear()`）。故该状态不可达。
- **"`push_delta` 的 `checked_add` 溢出"**：编码器只接受**升序 `u32`** 位置，
  重构出的位置必然也是 `u32` 域内，正常编码流无法触发溢出；要触发只能手工伪造字节流。
  两个版本在此路径上的行为完全相同，因此不是本次改动引入的风险。

### 最终版本（含边界测试）的微基准：p50 0.81×，但**进程间偶发**掉速

`probe-gol-final`（`b3320b0ab`，热函数 size=2797 mod64=16）：

| fixture | GOL_final/base (p50) | GOL_final/base (p95) |
|---|---:|---:|
| `early_hit_n16384` | 0.808× | 1.301× |
| `disjoint_n16384` | 0.820× | 1.323× |
| `sparse_hit_n16384` | 0.804× | 1.333× |
| `late_hit_n16384` | 0.810× | 1.363× |
| `mc4_hit_late_n2048` | 1.344× | 1.486× |

12 批次原始 p50：

```
GOL_final [57008, 56968, 56848, 56808, 56748, 56339, 57129, 100017, 57498, 56338, 57418, 72498]
base      [70129, 72439, 69989, 69367, 71009, 70199, 70379, 71958, 69908, 69489, 70259, 70137]
```

- **通常 0.81× base**（大部分批次 ~56,800 ns），但 **约 2/12 的进程掉到 base～P0 水平**
  （100,017 / 72,498）→ p95 被拉高到 1.30×
- 同一份 GOL 源码在 `a2c76cbc0` 时是**整齐的 0.66×**；仅因**新增一个 `#[cfg(test)]` 测试函数**
  改变了测试二进制布局，就变成 0.81× 且带偶发慢进程
- 这个"**进程间**偶发"说明慢态不完全是"每二进制确定"的——可能涉及 ASLR 对二进制基址
  或大页映射的随机化。**这一层未继续追**（属于纯微架构彩票，与 PR 逻辑无关）

**结论**：GOL 的**语义正确性**与**工作量削减**已被独立证明（见上），
但它的**实测加速比不适合作为对外数字**——微基准对这类改动不是可靠的门禁。
对外应引用：计数值（布局无关）+ 非测试构建的全量 e2e。

### ⚠️ 微基准数字的布局敏感性（必须随数字一起说明）

微基准跑在 **cargo test 二进制**里，所以**二进制的任何变化（包括新增一个测试函数）
都会改变测试二进制的布局**，进而可能改变微基准的绝对值——这正是本文件全篇的结论。
因此：

- **计数证据**（`SEEK_POSITIONS` / `GROUP_CHECKS` 等）与布局无关 → 是最硬的证据；
- **非测试构建的 e2e**（engine `do_query`，`#[cfg(test)]` 代码被编译掉）与测试代码无关
  → 是对外可引用的口径；
- **微基准绝对值**只在"同一次构建配置、同一 commit"下可比，跨 commit 需要重测。
  （GOL 的最终数字以 `probe-gol-final` 重测为准。）

语义等价性论证：原循环遍历 `start..full_end`；GOL 外层每轮处理一个完整 group，
内层 `first_in_group .. first_in_group + (group_end − index)` 与原循环访问的
`unpacked_group[index % BLOCK_SIZE]` 序列**逐个对应**，`previous`/`first` 的累加顺序
完全不变；`(index%128) + (group_end−index) ≤ 128` 保证不越界。
`cargo test -p lance-index --lib` **1537 passed / 0 failed**。

**这才是"从代码逻辑上修"的正解**，而且它同时解释了为什么布局会敏感：
原循环每位置多做两条指令 + 一次分支，把它推到了前端/分支预测的临界点上，
于是落点一变就从"刚好够用"掉到"不够用"。把工作减掉之后，落点不再有机会作恶。

### ✅ 确定性计数器：base 与 patched 的**动态工作量完全相同**（实证）

在两侧代码里插同一套计数器（`#[cfg(test)]`，生产构建编译掉）。
**计数值与代码布局无关**，所以即使插桩改变了落点，计数对比依然有效；也**不受 CPU 争用影响**。

`early_hit_n2048`（264 次计时调用）：

| 计数器 | base | patched | GOL |
|---|---:|---:|---:|
| `SEEK_CALLS` | 528 | 528 | 528 |
| **`SEEK_POSITIONS`** | **1,622,016** | **1,622,016** | **1,622,016** |
| `GROUP_UNPACKS` | 12,672 | 12,672 | 12,672 |
| `TAIL_DECODES` | 0 | 0 | 0 |
| **`GROUP_CHECKS`** | **1,622,016** | **1,622,016** | **12,672（1/128）** |
| `CURSOR_DECODES` | 0（base 无此计数点） | 264 | 264 |
| `ANCHOR_ITERS` | 264 | 264 | 264 |
| `BINARY_SEARCHES` | 264 | **0** | **0** |

`disjoint_n16384`：base `ANCHOR_ITERS=528` / `BINARY_SEARCHES=528`，
patched **264 / 0** —— **patched 在扫描侧做得比 base 更少**，却慢 1.44×。

**结论 1（这就是最初要回答的问题）**：base 与 patched 解码的位置数、解包的分组数、
分组检查数**逐项相同**，扫描侧 patched 甚至更少 → **1.44× 与代码逻辑无关，
100% 是代码布局**。此前是推理，现在是实证。

**结论 2（GOL 的正确性方向）**：GOL 的 `GROUP_CHECKS` 精确降到 **1/128**
（1,622,016 → 12,672），而 `SEEK_POSITIONS` / `GROUP_UNPACKS` **完全不变**
→ GOL 只去掉了冗余的逐位置分组检查，**没有少解码任何位置、没有改变解包次数**。

### 由逐指令归因得到的真正修复方向

把主循环改成 **group 外层 / 位置内层**，把 `Option` 加载比较与除法提到外层，
内层只剩 `load → add → push`。既减少每位置的工作量，又直接消掉归因指向最大的那条指令。
（分支 `probe/group-outer-loop`，见下节实测。）

## 4a-更正. 结论收窄：这是前端 op-cache 的**布局伪影**，不是对齐规则

后续实验**推翻了"必须 64 字节对齐"**这一过窄结论：

| 构建 | `seek_packed_doc_positions` 地址 | op_cache_miss | 实测 ratio |
|---|---|---:|---:|
| base | — | 366.3M | 1.000×（基准） |
| **P0** 原始 patched | mod64 = **48** | **1,307.2M（3.57×）** | **1.44×（坏）** |
| P1 = P0 + `.p2align 6` | mod64 = 0 | 365.2M | ~1.00× |
| P2 = P0 + `--align-all-functions=6` | mod64 = 0 | — | ~1.00× |
| **P3** = P0 + 分段探针（`Instant`×2 + TLS） | mod64 = **16** | — | **~0.99×（好）** |
| **P4** = P0 + 把 SIMD 解包拆成 `#[inline(never)]` 调用 | mod64 = **16** | 377.0M | **0.984–0.994×（好）** |

**P4 的额外发现（很重要）**：拆出解包后 `seek_packed_doc_positions` 只从 2452 B 变成
2434 B（−18 B）→ 说明 **SIMD 解包本来就没有被内联**，所以 P4 并不是"减小取指足迹"起了作用，
它只是又一次把热函数挪到了 mod64=16 而已。**因此三种扰动（对齐指令 / 全函数对齐 / 探针 /
拆解包）都只是"改变落点"，没有一个是有原理的修复。**

### "跑久一点能稳定吗？" —— **不能，已实测**

25 批次（比常规多 3.5 倍），`early_hit_n16384`：

| rev | p50 中位 | p50 trimmed | vs base |
|---|---:|---:|---:|
| base | 70,039 | 70,269 | 1.000× |
| P0（坏） | 100,697 | 100,736 | **1.438×** |
| P1（对齐） | 70,108 | 70,102 | **1.001×** |

- base / P1 的 25 个批次样本范围 **68,989–76,119**；P0 为 **98,866–101,967**
  → **两组分布完全不重叠**（好组最差样本仍比坏组最好样本快 23%）
- 7 批次时是 1.430× / 0.993×，25 批次是 1.438× / 1.001× → **几乎不变**
- 跑得久只让中位数更精确，**不会把 1.43× 收敛到 1.00×**

排除"跑久会好"的四个来源：

| 来源 | 实测 |
|---|---|
| 冷启动 / warm-up | 基准已 64 次预热后才计时，取 200 次调用中位 |
| 降频 / 热漂移 | 有效频率 3.044 vs 3.061 GHz；`ls_not_halted_cyc ≈ cycles` |
| 数据地址随机（ASLR） | 每次 exec 地址变，25 批次仍稳定在 ±3% |
| 测量噪声 | A/A（同一二进制）= 1.000–1.011× |

**结论：「不稳定」发生在 build 之间，不在 run 之内。** 1.43× 是该二进制代码布局的
**稳态硬件属性**。因此要得到可信指标只有两条路：① 钉住落点（`.p2align 6`）；
② 多构建取分布。单 build 长跑两条都做不到。反过来说，.5 的 1.16× 是他们那次构建的
一个抽样值，跑得再久也只会得到更紧的 1.16×。

### 由此得到的最终判断

| 构建 | 热函数 mod64 | op_cache_miss | ratio |
|---|---:|---:|---:|
| base（3 个不同构建） | — | 366–383M | 1.000× |
| P0 | **48** | **1,310–1,315M** | **1.43×** |
| P1 | 0 | 375M | 0.99× |
| P2 | 0 | — | ~1.00× |
| P3 | 16 | — | 0.99× |
| P4 | 16 | 377M | 0.98–0.99× |

- **base 在全部 3 个构建里都是好的**；patched 在 4 个构建里有 1 个坏 → patched 更容易踩到
- 观测到 **mod64=48 坏、0 与 16 好**，但没有足够证据归纳成规则（不排除是抽查偏差）
- 唯一**确定性**的做法是把热循环钉在已验证的好对齐上（`.p2align 6`）——
  因为 `.p2align` 是对节内偏移生效，节基址按 64 对齐时它等价于绝对 64 对齐，
  后续任何代码体积变化都不会再把它挪走。这就是 P1 相对 P3/P4 的真正价值。

要点：

1. **P3 在 mod64=16 下同样是好的** → "必须 mod64=0" 不成立。
2. **P0 与 P1 的代码足迹几乎相同**（热路径符号 7,800 B vs 7,928 B），
   但 op_cache_miss 差 3.57× → **不是代码体积问题**，是取指落点问题。
3. **没有任何逻辑缺陷可修**：分段探针显示 **decode 占 86–99.6%**，
   "其他"（排序 / 768B 数组 / 扫描 / 闭包）只占 0.3–13%；
   且指令数、分支数、L1D fills、LLC、有效频率、热函数机器码全部相同。
4. **排除了数据地址假说**：ASLR 每次 exec 改变堆/栈地址，而 P0 的 7 批次测量
   极稳（101097 / 101115 / …，0.1% 内）→ 若是数据地址效应必然抖动
   → 这是**每个二进制确定的代码取指布局**问题。
5. 因此：**性能对二进制布局高度敏感，P0 抽到了坏落点**。任何微小代码改动
   （插入 8 字节对齐指令、加两个 `Instant`、全函数对齐）都能翻转结果。

### 可用的对策（按可靠性排序）

| 方案 | 性质 | 状态 |
|---|---|---|
| 在热循环前 `asm!(".p2align 6")` | 让落点**确定且为已验证的好值**；不能证明机制 | 已实现并验证（tag `pr9170-align-fix`） |
| 把 op-cache 压力大的内联解包改为不内联 | **减小热循环取指足迹**，从源头降低对落点的敏感度 | 未测，值得试 |
| 重塑 follow-up 机制使其更紧凑 | 同时降低 I-cache 压力 | 未测 |
| `--align-all-functions=6` 进 `.cargo/config.toml` | 影响全仓 | 需上游同意 |
| 缩小代码增量让布局"自然"回到原处 | **不可控**，不是修复 | 排除 |
| 作为测量伪影向 reviewer 说明 | 需要给出多构建的分布证据 | 待办 |

## 4a. 根因：热解码函数的 64 字节对齐（实测证据链）

### 逐项排除

| 维度 | base | patched | 结论 |
|---|---|---|---|
| 退役指令数 | 356,725,214 | 356,588,881 | **相同（−0.04%）** |
| 分支数 | 58,666,947 | 58,630,166 | **相同（−0.06%）** |
| 有效频率（cycles/task-clock） | 3.044 GHz | 3.061 GHz | **相同** → 非降频 |
| L1D demand fills | 318,945 | 324,071 | **相同（+1.6%）** |
| LLC cache-misses | 166,026 | 165,059 | **相同** |
| `decompress_to` 代码 | 48,887 B / 9,759 指令 | 48,887 B / 9,759 指令 | **归一化后逐字节相同** |
| `seek_packed_doc_positions` 代码 | 2,452 B / 549 指令 | 2,452 B / 549 指令 | **归一化后逐字节相同** |
| `position_cursor` 源码 | 146 行 | 150 行 | 仅差 4 行 `#[cfg(test)]` 计数器 |
| `encoding.rs` | — | — | **零改动** |
| **`seek_packed_doc_positions` 入口对齐** | **mod 64 = 0** | **mod 64 = 48** | ← **唯一差异** |

`perf report` 显示时间都集中在 `seek_packed_doc_positions`（base 66.6% / patched 72.6%）。

### 成本是"每位置"的

| fixture | base p50 ns | patched p50 ns | ratio | delta |
|---|---:|---:|---:|---:|
| `early_hit_n32` | 270 | 251 | **0.930×**（patched 更快） | −19 |
| `early_hit_n256` | 1280 | 1690 | 1.320× | +410 |
| `early_hit_n2048` | 8790 | 12651 | 1.439× | +3861 |
| `early_hit_n16384` | 70058 | 100908 | 1.440× | +30850 |

线性拟合 **delta ≈ 1.88 ns/位置 ≈ 5.7 cycle/位置**，截距 ≈ 0。每位置执行约 26 条指令，
所以是**每位置的 IPC 下降**，而不是多做工作。

### 结论

**热解码函数的源码、机器码、数据、缓存行为、频率全部相同，唯一变量是它在二进制里的
64 字节对齐（0 vs 48）。** follow-up 让 `wand.rs` 变大（+516/−18），链接布局随之改变，
把这个函数推到了 48 字节偏移上，热循环跨 64 字节取指/op-cache 边界，于是每个位置多付
~5.7 cycle。

### 验证实验 1：全函数 64 字节对齐 → 劣化消失 ✅

用 `-C llvm-args=--align-all-functions=6` 重编 base / patched
（`sbg-targets/pr9170-aln-{base,patched}`，两者 `seek_packed_doc_positions` 均为 mod64=0），
7 批次、顺序随机、p50（ns）：

| fixture | aln_base | aln_base2 (A/A) | aln_patched | ratio | 未对齐时 |
|---|---:|---:|---:|---:|---:|
| `early_hit_n2048` | 8780 | 8780 | 8999 | **1.025×** | 1.439× |
| `early_hit_n16384` | 70139 | 70139 | 70399 | **1.004×** | 1.440× |
| `disjoint_n16384` | 69839 | 69578 | 69498 | **0.995×** | 1.444× |

**1.44× 的劣化完全消失。对齐假说成立。**

注意混杂：该开关对齐了**所有**函数。所以还需要验证实验 2。

### 验证实验 2：只对齐热循环（进行中）

在 `seek_packed_doc_positions` 的逐位置主循环前插入

```rust
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe { core::arch::asm!(".p2align 6", options(nomem, nostack, preserves_flags)); }
```

（分支 `fix/align-decoder`，target `sbg-targets/pr9170-alnfix`）

- 若单独对齐这一个循环就能回到 1.0× → 得到一个**最小、可上游**的修复
- 若不能 → 需要对齐整个函数入口（stable Rust 无法直接表达，需换思路）

### 修复：已验证有效 ✅

在 `seek_packed_doc_positions` 逐位置主循环前加对齐指令（commit **`3660c5f92`**，
分支 `fix/align-decoder`，`encoding.rs` +10 行）：

```rust
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe {
    core::arch::asm!(".p2align 6", options(nomem, nostack, preserves_flags));
}
```

效果（7 批次，顺序随机；`-` = 比 base 快）：

| fixture | base | patched 原始 | **patched + fix** | 原始/base | **fix/base** |
|---|---:|---:|---:|---:|---:|
| `early_hit_n256` | 1280 | 1690 | **1260** | 1.320× | **0.984×** |
| `early_hit_n2048` | 8780 | 12649 | **8760** | 1.441× | **0.998×** |
| `early_hit_n16384` | 70148 | 101097 | **69718** | 1.441× | **0.994×** |
| `disjoint_n2048` | 9080¹ | 12719 | **8820** | 1.444× | **0.971×¹** |
| `disjoint_n16384` | 69899 | 101168 | **69529** | 1.447× | **0.995×** |
| `sparse_hit_n2048` | 8910¹ | 12739 | **8820** | 1.435× | **0.990×** |
| `sparse_hit_n16384` | 70698 | 101228 | **69758** | 1.432× | **0.987×** |
| `late_hit_n2048` | 10519 | 14830 | **10280** | 1.410× | **0.977×** |
| `late_hit_n16384` | 81479 | 118147 | **81357** | 1.450× | **0.999×** |
| `mc4_hit_late_n2048` | 16700 | 23221 | **16110** | 1.390× | **0.965×** |
| `mc3_miss_mid_n2048` | 3051 | 150 | **150** | 0.049× | **0.049×**（保留收益） |
| `strided_miss_n16384` | 1374804 | 215945 | **186365** | 0.157× | **0.136×**（更好） |

¹ 小 n 的 A/A 展宽约 ±3%（<10µs 的测量有 ~300ns 量化台阶），9 批次 + A/A 对照后
这些 ±3% 全部落在 A/A 区间内 → **修复后无回退**。

对齐后 `seek_packed_doc_positions` 前 5 个循环的 mod64 与 base 完全一致
（32,32,0,16,0），第 6 个起因插入的 8 字节 padding 偏移 8 字节，但实测无影响。

工程门禁：`cargo fmt` ✅ / `clippy -p lance-index --all-targets -- -D warnings` 退出码 0 ✅ /
`cargo test -p lance-index --lib` **1537 passed / 0 failed** ✅；
另外 439 个 phrase/compound/wand 定向测试全过 ✅。

### 修复方向对比（为什么选这条）

| 方案 | 结论 |
|---|---|
| 热循环前 `asm!(".p2align 6")` | ✅ **采用**：最小（10 行）、可控、实测消掉全部劣化 |
| 对齐整个函数入口 | stable Rust 无直接属性；`--align-all-functions` 是构建开关，不适合进 PR |
| 缩小 follow-up 的代码增量以恢复布局 | ❌ **不可控**：任何后续改动都可能再次错位，不是修复 |
| workspace `.cargo/config.toml` 加 `--align-all-functions=6` | 影响面过大，需上游同意 |

## 4b. 已复现：thin LTO + cgu=1 下 patched/base = 1.44×

用与 .5 一致的构建（`CARGO_PROFILE_RELEASE_LTO=thin`、`CODEGEN_UNITS=1`、`OPT_LEVEL=3`、
`RUSTFLAGS='-C target-cpu=native'`）重建 base / patched，7 批次、顺序随机、p50（ns）：

| fixture | lto_base | lto_base2 (A/A) | lto_patched | patched/base |
|---|---:|---:|---:|---:|
| `early_hit_n2048` | 8790 | 8790 | 12651 | **1.439×** |
| `early_hit_n16384` | 70058 | 70438 | 100908 | **1.440×** |
| `disjoint_n16384` | 69768 | 69898 | 100748 | **1.444×** |
| `sparse_hit_n16384` | 69918 | 70699 | 101007 | **1.445×** |

- **指纹逐项一致**（rustc `18169182855791486453`、profile **`1783587453833569552`**、
  features `[]`、target `6569825234462323107`）→ 结果可比
- **顺带解决 plan §4.4 的悬案**：`1783587453833569552` 就是 **microbench（thin LTO/cgu=1）**
  的 profile 指纹，应确认为它；reviewer response 里的 `8681002757401828468` 是另一套
  （engine e2e）配置
- 四个 fixture 的比值都是 **~1.44×**，且增量随 n 线性 → **每位置**的固定倍率成本

### 机制（`perf stat`，`early_hit_n16384`/classic）

| | cycles | instructions | IPC | branches | branch-misses | miss 率 |
|---|---:|---:|---:|---:|---:|---:|
| lto_base | 77.77M | 356,693,500 | **4.59** | 58,666,947 | 222,602 | 0.38% |
| lto_patched | 103.55M | 356,531,103 | **3.44** | 58,630,166 | **330,195** | **0.56%** |

- **指令数、分支数完全相同**（差 0.05%）→ **不是多做了工作**
- branch-miss **+48%**，IPC **−25%** → 停顿变多
- 无 LTO 构建下两者指令数与 branch-miss 都相同、墙钟持平 → **纯代码生成差异**
- `encoding.rs` 在两个 revision 间**零改动** → 解码函数 `seek_packed_doc_positions`
  是同一份源码；`perf record` 显示它占 71.8%(base)/79.0%(patched) 的时间
  （classic 路径被位置解码主导）

## 4c. Profile 工具策略（本机可用性已核实）

目标：解释"指令数相同但 IPC −25%"→ 属于前端/取指问题、坏预测、还是后端调度问题。

| 优先级 | 工具 | 用途 | 本机 |
|---|---|---|---|
| 1 | `perf stat -M frontend_bound_group / bad_speculation_group / backend_bound_group / retiring_group`（AMD topdown） | **直接给出停顿归属**，最适合本问题 | ✅ 有 |
| 2 | `perf stat -e op_cache_hit_miss,ic_fetch_miss,ex_ret_ops,ex_ret_brn_misp` | 判 LTO 代码膨胀导致 op-cache / I-cache 失效（IPC 降但指令数不变的经典成因） | ✅ 有 |
| 3 | **2×2 构建矩阵**：LTO×cgu（无LTO/cgu16 已有、thinLTO/cgu1 已有，再补 thinLTO/cgu16、无LTO/cgu1） | 定位是 LTO 还是 cgu 触发 | 需 2 次编译 |
| 4 | **源码消融**：把 follow-up 的改动拆开（保留惰性 `Option` 解码 / 只留 anchor-jump / 只留 advance 快路径）逐个在 LTO 下测 | **最直接通向修复**：找出哪个构造在 LTO 下贵 44% | 需多次编译 |
| 5 | `perf annotate --stdio` + `objdump -d` 对比热点函数机器码 | 看内联/布局/寄存器分配的具体差异 | ✅ 有 |
| 6 | `llvm-mca` 喂热点循环汇编，比吞吐/端口压力 | 硬件无关地复现 IPC 差 | ✅ 有 |
| 7 | `bcc/bpftrace funccount/funclatency`（uprobe） | 统计解码调用次数/耗时 | ⚠️ 需装；且 uprobe 微秒级开销会扰动 10µs 级函数，价值低 |
| — | valgrind / cargo-bloat / cargo-asm / cargo-flamegraph | 未安装 | ❌ |

暂不采用的：`perf mem`（本机型不支持）、`perf c2c`（需 Intel）。
bcc/bpftrace 在本问题里价值低（无锁争用、无 I/O、无 syscall 热点，纯用户态计算），
若第 2/3 步无法区分，再考虑用 `bpftrace` 对 `seek_packed_doc_positions` 做 uprobe 计数。

## 5. 测量基础设施（可复用）

| 文件 | 作用 |
|---|---|
| `/tmp/iu-profile/phrase_ab.py` | 多批次、revision 顺序随机、median + 20% trimmed mean |
| `sbg-targets/pr9170-{base,old,patched,c}` | 无 LTO 的四个 revision 测试二进制 |
| `sbg-targets/pr9170-lto-{base,patched}` | thin LTO / cgu=1 版本（进行中） |
| `wt-pr9170-{base,old,patched,c}` | 四个 revision 各自独立的 worktree |
