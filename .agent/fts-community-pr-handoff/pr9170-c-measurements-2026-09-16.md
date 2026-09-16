# PR 9170 Plan C —— 本机测量记录（2026-09-16）

机器：21.130.254.33（Zen4 / EPYC 9754，128 核）
工具：`/tmp/iu-profile/phrase_ab.py`（多批次、revision 顺序随机、报 median + 20% trimmed mean）
核绑：`taskset -c 32`；每个目标目录只编一次，测量阶段零 checkout、零重编。

## 0. 编译指纹（跨 target 一致性核对）

| rustc | profile | features |
|---|---|---|
| `18169182855791486453` | `13952719172529475294` | `[]` |

- 四个 target 目录（`pr9170-base` / `-old` / `-patched` / `-c`）**rustc 与 profile 指纹完全相同**
  → 组内 A/B 有效，不存在编译器差异混杂
- `rustc` 值与 .5 文档记录的 `18169182855791486453` **一致**
- `profile` 值与 .5 文档里两个互相矛盾的值（`1783587453833569552`、
  `8681002757401828468`）都不同。判读：.5 那个值是 **engine/e2e** 构建的
  （engine `Cargo.toml` 覆写了 `[profile.release] lto="thin"`、`codegen-units=1`），
  本机这个是 **lance workspace test** 构建，profile 设置本就不同。
  **plan §4.4 要求做的 fingerprint audit 在 .5 上仍未解决**（它自己两份文档就不一致）。
- 取法：`<target>/release/.fingerprint/adler2-*/lib-adler2.json` 的 `rustc` / `profile` 字段

## 1. 测量配置

- 二进制（四者同名 hash，同一 crate 指纹）：
  `<target>/release/deps/lance_index-c98f791a37864482`
- 跑法：`LANCE_PHRASE_POSITION_BENCH=1 LANCE_PHRASE_POSITION_BENCH_PERCALL=1
  LANCE_PHRASE_POSITION_BENCH_SCENARIO=<fixture> taskset -c 32 <bin> phrase_position_bench --nocapture`
- 每批次 2000 次单调用采样，`PERCALL_ITERS=2000`
- base 与 patched 都**追加了同一对 bench commit**（`f38bff929` + `d7f3bfd1a` cherry-pick
  到 base / old），保证四个 revision 用同一把尺子

### revision → 二进制

| 名字 | revision | 说明 |
|---|---|---|
| `base` | `a11b817ad` + bench | 上游 main |
| `old` | `3fbdb7739` + bench | PR 原样 |
| `patched` | `d7f3bfd1a` | follow-up（待提交补丁） |
| `cand` | `27f996a2c` | patched + `#[inline] advance_to_at_least` |

## 2. 结果（7–9 批次，p50 ns）

| fixture | base | base2(A/A) | patched | cand | patched/base |
|---|---:|---:|---:|---:|---:|
| `early_hit_n2048` | 7149 | — | 7110 | 7110 | **0.995×** |
| `early_hit_n16384` | 55879 | 55918 | 55730 | 55789 | 0.997× |
| `disjoint_n16384` | 55929 | 55928 | 55799 | 55779 | 0.998× |
| `sparse_hit_n16384` | 56009 | 56009 | 55810 | 55809 | 0.996× |
| `late_hit_n16384` | 65258 | 65238 | 65617 | 65069 | 1.006× |
| **`strided_miss_n2048`** | 137036 | 137018 | **21389** | 21380 | **0.156×** |
| **`strided_miss_n16384`** | 1360227 | 1360157 | **168116** | 168026 | **0.124×** |

A/A（base vs base2）每个 fixture 都在 **±0.1–0.3%**；p95 列同结论。

## 3. 结论

1. **patched 在本机是纯赚**：病态场景 `strided_miss_n16384` 快 **8×**（1.360ms → 0.168ms），
   classic 场景（`early_hit` / `disjoint` / `sparse_hit` / `late_hit`）**零成本**（±0.6% 内）。
2. **.5 记的"classic 路径 +16%"在本机完全复现不出来**。.5: base 10,610 / patched 12,303（ns,
   per-call p95）；本机: base 7149 / patched 7110。方向相反且幅度在噪声内。
   → 这是本次最重要的发现：那个"回退"很可能是**机器相关或测量假象**，不是代码的固定开销。
3. **`27f996a2c` 的 `#[inline]` 是无效改动**：cand 与 patched 在所有 fixture 上无差异
   （≤0.1%）。按 plan 的纪律，这个假设应当作为"未验证/无效"记录，不要并入待提交补丁。
4. classic 路径在本机**被位置解码主导**：`early_hit_n16384` / `disjoint_n16384` /
   `sparse_hit_n16384` 的 base 都是 **~55,900 ns**，彼此几乎相同，
   说明扫描逻辑的差异被解码成本淹没 —— 这也是"16%"难以在 classic 上稳定观测的结构性原因。

## 4. 已排除的假设

- **H4（`#[cfg(test)] count_anchor_steps()` 污染基准）**：读代码后排除。这些 fixture 的
  anchor 步数极少（`early_hit` 1 步即命中；`disjoint` 1 步即跳完整个 anchor），
  所以每场景只有约 1 次 `thread_local` 访问，量级纳秒。**不是 16% 的来源。**
  （但仍值得记录：基准是 `#[test]`，生产构建里没有这段代码，属于 harness 与生产的差异。）

## 5. 待办

- [ ] `old`（`3fbdb7739`）编译完成后补测：确认 PR 的 `disjoint` 4.75× / `strided_miss` 6× 现象
      在本机是否复现（.5: disjoint base 91,863 / old 436,203；strided base 1,365,006 / old 436,374）
- [ ] perf 剖面 base vs patched 的 `strided_miss`，确认"anchor 跳过"机制确实生效
- [ ] 用 perf 在 `early_hit` 上确认 classic 路径本机没有额外分支/缓存代价（佐证结论 2）
- [ ] `"york photo"` e2e 离群点（需要 engine 构建，最后做）
