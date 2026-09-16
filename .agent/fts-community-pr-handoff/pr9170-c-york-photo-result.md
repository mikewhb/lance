# PR 9170 Plan C —— `"york photo"` 离群点结论（本机，2026-09-16）

## 结论

**本机不复现 1.342× 离群点**，且证据指向它与 classic 那组是**同一个布局伪影**，不是 phrase 逻辑问题。

## 1. 单条隔离协议

三个 engine，各来自独立 worktree，构建配置与 .5 一致（engine `Cargo.toml` 的
`[profile.release]` = `lto="thin"`, `codegen-units=1`, `opt-level=3`，
`RUSTFLAGS='-C target-cpu=native'`）：

| engine | 来源 worktree | 内容 |
|---|---|---|
| `eng-base` | `wt-pr9170-base` | base `a11b817ad` |
| `eng-nofix` | `wt-pr9170-nofix` | patched `eabbcfe13`（无修复） |
| `eng-fix` | `wt-pr9170-fix` | patched + `pr9170-align-fix`(`3660c5f92`) |

驱动 `/tmp/iu-profile/york_photo.py`：每 query **预热 3 次** → 15 次重复、**每次重复打乱引擎顺序**
→ p50 / 20% trimmed mean / min，并校验 TOP_10 `row_id:score` 一致。

| query | base p50 | nofix p50 | fix p50 | nofix/base | fix/base |
|---|---:|---:|---:|---:|---:|
| **`"york photo"`** | 2269 | 2299 | 2264 | **1.013×** | **0.998×** |
| `"jesus as a child"` | 8934 | 5482 | 5342 | 0.614× | 0.598× |
| `"the book of life"` | 67147 | 46240 | 46169 | 0.689× | 0.688× |
| `"time for kids"` | 12886 | 10548 | 10412 | 0.819× | 0.808× |

`"york photo"` 原始样本完全重叠（base 2131–2382，nofix 2186–2402）→ **无回退**。
TOP_10 dump 三引擎逐位一致。

## 2. 全量 phrase 组（300 条）

`/tmp/iu-profile/phrase_e2e.py phrase 5 32 ...`：每 query 预热 2 次 → 5 次重复、顺序随机。

| engine | median-of-p50 µs | 几何平均比 | 胜出条数 |
|---|---:|---:|---:|
| base | 1146 | 1.000× | 72 |
| nofix | 1131 | **0.982×** | 79 |
| fix | 1121 | **0.976×** | **149** |

- **TOP_10 dumps 三引擎差异 0 条**
- **最差 nofix/base = 1.065×**（`"music stands"`）→ 不存在 1.342× 量级的离群点
- 方向与 .5 的 phrase `0.918×` 一致（本机弱一些；统计量与构建布局都不同）

## 3. 跨机对照（关键）

| | base | patched |
|---|---:|---:|
| .5 机器 | 2,296 µs | **3,082 µs** |
| 本机 | 2,269 µs（差 **1%**） | **2,299 µs（差 34%）** |

**base 跨机稳定、patched 跨机差 34%** —— 与 classic 组（见
`pr9170-c-classic-cost-finding.md`）的特征完全相同：base 在多个构建里都稳，
patched 的表现由二进制布局决定。

→ `"york photo"` 的 1.342× 不是 phrase 路径的逻辑问题，而是**布局抽签恰好抽中这一条**。

## 4. 复现步骤

```bash
# engine 就绪后（三个 do_query 已编好）
B=/data/arrow/code/sbg-targets
python3 /tmp/iu-profile/york_photo.py 15 32 \
  base=$B/eng-base/release/do_query \
  nofix=$B/eng-nofix/release/do_query \
  fix=$B/eng-fix/release/do_query

python3 /tmp/iu-profile/phrase_e2e.py phrase 5 32 \
  base=$B/eng-base/release/do_query \
  nofix=$B/eng-nofix/release/do_query \
  fix=$B/eng-fix/release/do_query
```

注意：`TOP_10` / `TOP_10_DUMP` 首次命中某 query 会比热路径慢 ~10×（建计划 + 触碰 posting），
**必须先做 per-query 预热**，否则会把冷启动当成回退。
