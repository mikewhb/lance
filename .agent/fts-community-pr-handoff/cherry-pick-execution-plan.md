# 执行计划:把 IU 机制 cherry-pick 到社区 main

> 分支:`fts/iu-reuse-main`(基于社区 main `7b97db782`)
> 候选分支(已推送):`fts/reqopt-window-leftovers` @ `a20371e4b`
> 日期:2026-10-03

## 0. 目标与范围

把 IU(1 MUST + N SHOULD)相关、已验证的 5 个提交 cherry-pick 到社区 main 上,
**跳过** `d59b55d86`(3 子句 cost-skew lead-stream——intersection/phrase 形状,
与 IU 无关,且上游 PR 9175 正在做同一件事)。

预期 diff:~2100–2200 行。预期收益(需在新基线重新实测):
IU 对 Lucene 1.19x → ~1.15x、最坏 2.78x → ~2.39x(旧基线数字,新 main 含 #9612 后会更好)。

**明确不做**:d59b55d86(skew)、af87363a4 的 wand.rs hunk(引用被跳过提交引入的常量)。

## 1. cherry-pick 序列与已核验的冲突面

```
git cherry-pick 88933c0dc c68f74947 9ecec3ebe 7001f0c26 a20371e4b
        │              └─ SHOULD 换叶      └─ ReqOpt 提升   └─ C2      └─ 去重
        └─ 内核+drain+TermLeaf+admission
```

已核验的冲突面(逐 hunk 对比过):

| 文件 | 我们的 hunks | 社区 main 的 hunks | 预期 |
|---|---|---|---|
| compound.rs | +1050/+134/+124 | **完全没动** | 干净 |
| wand_iu_tight.rs / wand_term_leaf.rs | 新文件 | 不存在 | 干净 |
| wand.rs | imports@24、PostingIterator@1109/1361/1664、WandCursor@6003-6190、Wand@6329、tests@7706/12744 | imports@24、norm 相关@56-618/1277、Wand@2472-2670、tests@8038 | **仅 import 行文本冲突** |
| 语义适配 | 保留提交**零调用** main 改过签名的 API(`norm_k_cache`/`norm_addends` 引用数=0) | | 无 |

另:`af87363a4` 的 scorer.rs/compound.rs 部分保留,wand.rs hunk(引用
`BULK_AND_AUTO_SKEW_RATIO`)丢弃;跳过 `d59b55d86` 连带跳过
`peek_remaining_block_docs_upto`(已确认保留代码零调用方)。

## 2. 步骤与验收

| # | 步骤 | 验收 |
|---|---|---|
| S1 | 构建纯 main 基线二进制 `/tmp/do_query_main_base`,并导出 943 参考 dump | dump 行数=943;与旧 dump 差异逐条审阅(预期:分数位一致或仅 #9612 引起的差异) |
| S2 | cherry-pick 序列,逐个解决冲突 | 每个提交干净落位;跳过 hunk 有记录 |
| S3 | `cargo fmt` + `cargo test -p lance-index --lib inverted` + `cargo clippy` | 0 failed;clippy 0 error(基线允许的无关 warning 除外) |
| S4 | **自检视**:逐 hunk 读 cherry-pick 后的 diff,重点核对 (i) import 合并未漏 (ii) 与 main 的 norm-cache 重构无 API 错配 (iii) 跳过 d59b55d86 后无悬空调用 | 自检清单逐项打勾 |
| S5 | **独立 agent 检视**:把 cherry-pick 后的 diff 交给独立 agent,对照社区 main 评审正确性与遗漏 | 无 BLOCKER;RISK/NIT 落实或记录 |
| S6 | 构建 cherry-pick 后二进制,重导 943 dump | 与 S1 的 main 参考 dump **位一致** |
| S7 | **全量 benchmark**:IU 40q(min-of-45,3 轮交错)+ 全量 943(min-of-9,3 轮)+ Lucene 对照 | IU 无回归 >5%;943 无回归 >5%;与 Lucene 比值记录 |
| S8 | 提交(cherry-pick 的提交保留原信息;另加一条适配说明) | `git log` 干净;push 待确认 |

## 3. 风险与回退

| 风险 | 缓解/回退 |
|---|---|
| wand.rs 冲突比预期多(main 的 norm-cache 重构波及上下文) | 逐 hunk 手工合;`git cherry-pick --abort` 回退 |
| main 的 #9612/#9613 改变了索引/打分行为导致 dump 不一致 | 先比对 main-only dump vs 旧分支 dump,定位是 main 侧变化还是 cherry-pick 引入 |
| cherry-pick 后 IU 收益在新 main 上缩水(#9612 已砍固定成本) | S7 实测;若收益 <2% 则如实报告并重估是否继续 |
| main 测试套件与我们的测试冲突 | 逐个看失败原因;区分 main 侧既有失败与 cherry-pick 引入 |

## 4. 验收标准(全部满足才算完成)

1. `cargo test -p lance-index --lib inverted` 0 failed;
2. 943 dump 与纯 main 二进制位一致;
3. IU 40q 相对纯 main 无回归 >5%;
4. 全量 943 相对纯 main 无回归 >5%;
5. clippy/fmt 干净;
6. 独立 agent 复查无 BLOCKER;
7. 提交信息完整(含新基线上的实测数字)。
