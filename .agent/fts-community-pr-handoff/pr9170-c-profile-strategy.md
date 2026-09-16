# PR 9170 Plan C —— 本机剖析/测量策略

日期：2026-09-16
前置：`pr9170-c-local-setup.md`（环境、分支、纪律）

## 1. 为什么不能"一个 worktree 反复切 revision"

`exact_phrase_scan` 这刀要对比 4 个 revision（base / old / patched / candidate）。
如果在同一个 worktree + 同一个 `CARGO_TARGET_DIR` 里反复 `git checkout`：

- 每次 checkout 都让 `lance-index` 及其下游失效 → **4 次近乎全量的 lance 重编**
- 每次重编约 10–20 min，4 个 revision × 多批次 = 半天都花在编译上
- 而且测完还得切回来，容易出错、容易丢未提交改动

## 2. 采用的策略：一 worktree + 多 target 目录，直接调二进制

**一个 worktree（`/data/arrow/code/wt-pr9170-c`），每个 revision 一个专属 target 目录。**
编完之后 4 个测试二进制各自独立存在，测量时**直接跑二进制，不再 checkout**。

```bash
WT=/data/arrow/code/wt-pr9170-c
git -C $WT checkout <rev>
CARGO_TARGET_DIR=/data/arrow/code/sbg-targets/pr9170-<rev> \
RUSTFLAGS='-C target-cpu=native' \
cargo test -p lance-index --release --no-run
```

| target 目录 | revision | 含义 |
|---|---|---|
| `sbg-targets/pr9170-c` | `27f996a2c` | candidate（inline 假设，已建） |
| `sbg-targets/pr9170-patched` | `eabbcfe13` | patched（待提交补丁） |
| `sbg-targets/pr9170-old` | `3fbdb7739` | old（PR 原样） |
| `sbg-targets/pr9170-base` | `a11b817ad` | base（上游 main） |

**直接跑二进制**（cargo test 产物在 `<target>/deps/lance_index-<hash>`）：

```bash
BIN=<target>/deps/lance_index-<hash>
LANCE_PHRASE_POSITION_BENCH=1 LANCE_PHRASE_POSITION_BENCH_PERCALL=1 \
LANCE_PHRASE_POSITION_BENCH_SCENARIO=early_hit_n2048 \
  taskset -c 32 $BIN phrase_position_bench --nocapture
```

好处：切 revision 只发生在**编译阶段**，测量阶段零 checkout、零重编，
可以任意批次、任意顺序反复跑。

### 约束

- **编译可以并发**（不同 target 目录，cargo 锁不冲突，CPU 128 核够用）
- **测量绝对不能并发**：同一时刻只跑一个 bench，`taskset -c 32` 固定核
- **有 cargo 在跑时绝不测数据**（CPU 争用会污染计时）
  → 判据：跑 bench 前先 `pgrep -x cargo || pgrep -x rustc` 确认为空
- 磁盘：每个 target 预计 5–8G，4 个约 30G；当前剩余 87G，够用但**要盯**
  → 若不足，优先保留 base + patched + candidate，old 用同一目录重编

## 3. 测量协议（照 .5 计划 Workstream 1）

1. **先做 A/A**：base 编完后跑 2 个独立批次，patched 同样 2 批。
   用 A/A 的批次间差异定出这台机器的噪声底，再判 10–16% 是不是真信号。
2. **revision 顺序随机化**：每轮 `base/old/patched/candidate` 的顺序打乱，
   避免"先跑的占便宜"这类系统性偏差。
3. **至少 2 个独立批次**，保留**每一条原始样本**，报 **median + trimmed mean**；
   **不用 min-of-10 排序**（.5 已明确 min-of-10 不适合判定）。
4. 先只跑复现信号的最小 fixture：`early_hit_n2048`，确认后再 `early_hit_n16384`、
   `disjoint_n16384`。批量小、迭代快。

验收（.5 给的）：`early_hit` / `late_hit` / `sparse_hit` / `disjoint` 的
patched/base 估计在 A/A 变异范围内无一致回退；`disjoint` 必须远低于 old 的 4.75×。

## 4. 剖析协议（Workstream 2）

用 `perf record -e cycles -F 999 -g` 对**同一个 fixture**分别采 base / old / patched，
固定核、同一条命令，比较：

- `exact_phrase_scan`
- `PositionCursor::advance_to_at_least`
- `slice::partition_point` 及其单态化搜索循环
- 位置解码 / 缓冲区回收

本机型 `perf mem` 不可用（非 Intel，无 IBS mem 事件），但 `-e cycles` 与
`-e cache-misses` 可用（上次 FTS 任务已验证）。release 不带 debuginfo 也能量到
**符号级**，足够区分下面几个假设；暂不额外编一份 debug 构建（省一次编译）。
如需行级归因，再用 `CARGO_PROFILE_RELEASE_DEBUG=1` 单独编一份（另开 target 目录）。

## 5. 候选假设（含我读代码新加的两条）

.5 原列三条：

- **H1** 单调游标的 `current`/`next` 检查（每个 classic anchor 一次）
- **H2** follower 推导 `next_anchor` 界 + 每次失败探测查 anchor slice
- **H3** 兜底 suffix `partition_point`（密集/交错时反复走到）

读代码后**新增两条**，都值得先排除：

- **H4（测量假象，优先排除）**：`exact_phrase_scan` 循环里有
  `#[cfg(test)] { count_anchor_steps(); }`。微基准**本身就是 `#[test]`**，
  所以基准里 patched 每走一个 anchor 就多一次 `thread_local!` Cell 自增 + 分支，
  而**生产构建（非 test）里这段代码根本不存在**。
  → 它可能解释掉一部分乃至大部分"16%"。
  → 验证：临时把计数移出循环（或改用非 test 的 bench harness）编一份对比。
  若确认，.5 那张 per-call 表的 classic 一列需要重测。
- **H5（结构性开销）**：patched 为了惰性解码把游标放进 `Option<PositionCursor>`，
  于是热循环里每个 (anchor, follower) 多了 1 次 `is_none()` +
  **2 次 `as_ref()/as_mut().expect()`**（都带 panic 分支）。
  base 是先全解码再排序，循环里直接索引，没有这些。
  → 剖面上表现为 `exact_phrase_scan` 的 exclusive 时间上升、分支数上升。

`27f996a2c` 已经试的是"给 `advance_to_at_least` 加 `#[inline]`"（偏 H1），
但**没有附测量记录**，属于未验证假设，本次要补测。

## 6. 执行顺序

1. ✅ 建 worktree、编 candidate（进行中）
2. 依次编 patched / old / base（可与其它编译并发，但不要在测量时编）
3. candidate 编完先跑一次 `early_hit_n2048` per-call，**确认本机能否复现 16% 信号**
   —— 复现不了就先查环境差异（指纹、核绑、turbo），不要急着改代码
4. base 编完 → **A/A 两批**，定噪声底
5. patched 编完 → 与 base 对照，确认信号且超出 A/A
6. perf 采 base vs patched 的 `early_hit`、`disjoint`，判定 H1–H5 中哪条成立
7. 只有被剖面支持的假设才动手改；改完回到第 2 步重测
8. `"york photo"` 离群点放到最后（需要 e2e，且 .5 说尚未有独立多批次确认）

## 7. 禁止事项

- 不在远程机器起任何编译/耗时进程（用户明确要求，编译全在本机）
- 不动主仓工作树（它有别人 +145 行未提交的 `wand.rs` 改动）
- 不用 `git reset --hard` / checkout 覆盖丢弃任何未提交工作；失败假设用 `git revert`
- 测量时不并发、不编译
