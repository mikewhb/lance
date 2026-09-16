# PR 9170 Plan C —— 本机环境搭建记录

日期：2026-09-16
本机：21.130.254.33（Zen4 / EPYC 9754，128 核，rustc 1.97.0）
来源：`.5`（21.6.162.5）的 `.agent/fts-iu-profile/pr9170-c-followup-profile-plan.md`

## 1. 动工前的本机快照（**务必保留，用于回滚对照**）

| 目录 | 分支 | HEAD | 干净？ |
|---|---|---|---|
| `/data/arrow/code/lance-distributed-foyer`（主仓） | `bench/community-fts-stack` | `17b5e9205` | ❌ **有未提交改动** |
| `/data/arrow/code/wt-fts-analysis` | `wt/analysis` | `3d1189834` | ✅ |
| `/data/arrow/code/wt-fts-pr` | `fts/maxscore-inner-window` | `8c9c27ab4` | ✅ |
| `/data/arrow/code/wt-fts-pr-base` | （detached） | `548280ff1` | ✅ |

### ⚠️ 主仓的未提交改动（别人的活，不要动）

```
 M docs/fts-reqopt-window-eli5.md                +5 -1
 M rust/lance-index/src/scalar/inverted/wand.rs  +145
```
已抢救备份为：`UNCOMMITTED-wand-and-docs-2026-09-16.patch`（186 行）。
**本次工作全程不碰主仓工作树。**

顺带记录：主仓 HEAD 已从 `cfcd7490c` 前进到 `17b5e9205`
（`perf(fts): prune phrase confirmations with exact term bounds`，只改 `compound.rs`）。
该 commit 正是把 `+"the who" +uk` 从 69.25 ms 降到 36.46 ms 的那个修复。

## 2. 从 .5 推过来的分支

`my` = `git@github.com:mikewhb/lance.git`

| 分支 | commit | 作用 |
|---|---|---|
| `fts/pr9170-c-profile` | `27f996a2c` | Plan C 工作分支（本次主场） |
| `fts/phrase-v3-squashed` | `eabbcfe13` | patched（follow-up / 待提交补丁） |
| `pr9170-rebased` | `3fbdb7739` | old（PR 原样） |

base = `a11b817ad`（origin/main），本机已有。

## 3. 本机 worktree

```
/data/arrow/code/wt-pr9170-c   分支 wt/pr9170-c  →  my/fts/pr9170-c-profile  @ 27f996a2c
```

commit 链（相对 base `a11b817ad`）：
```
3fbdb7739  old       decode exact-phrase positions from the rarest term first
eabbcfe13  patched   restore anchor skipping in the exact-phrase scan
f38bff929  bench     add phrase position microbenchmark
d7f3bfd1a  bench     filter phrase position microbenchmark
27f996a2c  假设1     inline phrase cursor advance candidate   ← HEAD
```
相对 base 只改两个文件：`wand.rs`(+903/−98)、`wand_phrase_bench.rs`(+283)。

文档用 scp 拉到 `wt-pr9170-c/.agent/fts-iu-profile/`（未跟踪，重启需重拉）。

## 4. 编译设置

- `CARGO_TARGET_DIR=/data/arrow/code/sbg-targets/pr9170-c`（**专属，首次冷启**）
  - 不复用 `sbg-targets/community`：那是 SBG engine 的 target，共享会因 feature 解析不同
    导致 engine 下次全量重编（~18 min）
  - 不复用主仓 `target`：只有 debug 产物（13G），对 release 无帮助
  - 换 revision 时**同一个 target 目录** → 只有 lance crate 重编，增量很快
- `RUSTFLAGS='-C target-cpu=native'`（与 .5 的指纹一致）
- rustc 1.97.0；`~/.cargo/config.toml` 配了 `rustc-wrapper=/root/.cargo/bin/sccache`
  （`SCCACHE_DIR=/data/arrow/.sccache`，当前统计为 0，跨 worktree 可能仍有加速）
- ⚠️ 不要和别的 cargo 并发（本机多会话共用）
- 剖析时用 `CARGO_PROFILE=bench`（`[profile.bench]` = opt-level 3 + debug=true + strip=false）

## 5. 微基准怎么跑

`rust/lance-index/src/scalar/inverted/wand_phrase_bench.rs` 是个 `#[test]`：

```bash
cd /data/arrow/code/wt-pr9170-c
export CARGO_TARGET_DIR=/data/arrow/code/sbg-targets/pr9170-c
export RUSTFLAGS='-C target-cpu=native'

# 批量模式（默认）
LANCE_PHRASE_POSITION_BENCH=1 \
  cargo test -p lance-index phrase_position_bench --release -- --nocapture

# 单次调用 p50/p95（计划里 Workstream 1 要的就是这个）
LANCE_PHRASE_POSITION_BENCH=1 LANCE_PHRASE_POSITION_BENCH_PERCALL=1 \
  cargo test -p lance-index phrase_position_bench --release -- --nocapture
```

过滤：
- `LANCE_PHRASE_POSITION_BENCH_SCENARIO=early_hit_n16384` 只跑某个 fixture
- `LANCE_PHRASE_POSITION_BENCH_PATH=classic|compound_dispatch|bulk` 只跑某条路径

输出 CSV：`scenario,path,matched,mean_ns,p50_ns,p95_ns,iters,percall_p50_ns,percall_p95_ns`

场景名：`<early_hit|disjoint|sparse_hit|late_hit|strided_miss>_n<32|256|2048|16384>`，
外加 `mc3_*` / `mc4_*` 多 clause 场景。

**换 revision 测量**（在同一个 worktree 里切，保持 target 增量）：
```bash
git -C /data/arrow/code/wt-pr9170-c checkout <commit>   # 先 git status 确认干净
```
切完记得切回 `wt/pr9170-c`。

## 6. 纪律（照抄 .5 计划文档的要求）

- 每处改动先 commit 再 build/measure
- commit message 标注类别：profiling infrastructure / implementation hypothesis / test / measurement record
- 失败的假设**留在历史里**并附原始证据，用 `git revert` 回到上一个可用状态，
  **禁止 `reset --hard` / checkout 覆盖 / 删未提交工作**
- 复用 worktree 前先记录当前 branch + HEAD 并 `git status`
- A/A 必须做；不要用 min-of-10 排序；报 median + trimmed mean
- 不要用聚合 e2e 数字当 headline（`0.918x` 是 phrase 加权均值，不是普遍提升）

## 7. 待解释的证据（.5 已有，本机要复现 + 定位）

per-call p95（ns）：

| fixture | base | old(PR) | patched | 含义 |
|---|---:|---:|---:|---|
| `early_hit_n16384` | 91,942 | 91,643 | 106,471 | patched 比 base 慢约 **16%** |
| `early_hit_n2048` | 10,610 | 10,600 | 12,303 | 同上信号 |
| `disjoint_n16384` | 91,863 | 436,203 | 106,191 | 修掉了 PR 的 4.75×，但仍 1.16× base |
| `strided_miss_n16384` | 1,365,006 | 436,374 | 224,444 | 两边都大幅改善 |

待定位的候选成本中心（`exact_phrase_scan` / `PositionCursor::advance_to_at_least`）：
1. 单调游标的 `current`/`next` 检查（每个 classic anchor 一次）
2. follower 推导的 `next_anchor` 界 + 每次失败探测查 anchor slice
3. 兜底的 suffix `partition_point`（密集/交错时会反复走到）

e2e 离群点：`"york photo"` base 2,567 µs vs patched 3,285 µs（1.280×），
**尚无独立多批次确认**，不要当成结论。
