# ReqOpt 整块 skip：人能读完的 ELI5

日期：2026-09-11  
受众：工程师（先能在纸上走一遍 128 篇，再对照代码）  
本地 Lucene：`/data/arrow/code/lucene/lucene/core/src/java/org/apache/lucene/search/`  
相关：[fts-floor-propagation-eli5.md](./fts-floor-propagation-eli5.md)

两次落地都回滚了。不是「skip 这个想法错了」，是 **把窗剪碎了**。这篇只讲清楚：窗是什么、Lucene 怎么问标签、Lance 能抄什么、不能抄什么。

---

## 一句话

Top-k 堆满之后有一条及格线 `F`。索引每约 **128 篇**贴一张标签（这一叠最高能打几分）。Lucene 一次看一整叠标签：不够 `F` 就整叠扔掉。  
赚不赚钱取决于：**读一次标签，扔掉的是一叠，还是两篇之间的缝。**

---

## 贯穿例子

查询：`airport +security rules`

```text
security (MUST，必须有) : 10, 20, 30, 40, 50, 60, 70, 80, ...
airport  (SHOULD，可加分): 10, 80
rules    (SHOULD，可加分):     40, 90
```

文档 0–127 是索引贴的 **一叠**。当前停在 **10**。只问一件事：这次标签覆盖到哪一篇（右端叫 `up_to`）？

```text
文档号   0 ........ 10 ........ 40 ........ 80 ........ 127
         |          ^           ^           ^            |
         |          当前        rules       airport      这一叠末尾
         +---------------- 一张标签覆盖的范围 ---------------+
```

三个角色：

| 名字 | 是什么 |
|------|--------|
| MUST | 必须出现的词，这里是 `security` |
| optional / SHOULD 并 | `airport OR rules`，命中了才加分 |
| 浅推进 | 只读标签，不把某一篇从 posting 里解出来 |
| 精确推进 | 真的走到某一篇（`advance` / `next`） |

---

## Lucene 在这个例子里怎么走

源码（10.3）：

- `ReqOptSumScorer.advanceShallow` / `advanceImpacts`
- `DisjunctionSumScorer.advanceShallow` / `getMaxScore`

构造 TOP_SCORES 时就会 `req.advanceShallow(0)` 和 `opt.advanceShallow(0)`。此时 optional 的 `docID` 仍是 **-1**（还没翻到任何一页），但第 0 叠的最高分已经在手里。

停在 10 时：

1. MUST 浅推进 → 这一叠右端 **127**。
2. optional 若还是 -1，或已经到了 ≤10：**只浅推进，不 `advance`。**
3. optional **整体**已经跑到 10 前面（并的下一篇是 80）→ 才把窗收到 **79**（MUST 独走的前缀）。这是大门，不是每把椅子。
4. `airport OR rules` 里，已经跑到 10 **前面** 的那个孩子（`rules` 在 40）：**不参与右端的 min**，也不用 `40-1=39` 去截窗。
5. 问完标签，若整叠最高分 `< F`，目标改成 128，去问下一叠。**10、20、30 碰都不碰。** 够格了才 `req.advance`。

所以 Lucene 的 `up_to` 在这个例子里是 **127**，不是 39。

`DisjunctionSumScorer` 原文就是：

```java
if (scorer.docID() <= target) {   // -1 未定位也算 <=
    min = Math.min(min, scorer.advanceShallow(target));
}
// docID > target 的孩子：不问，也不截窗
```

`getMaxScore(upTo)` 则是：`docID() <= upTo` 的孩子都加进来。Lucene 的 impacts 能从上次浅推进起 **跨多块** 回答「一直到 upTo 最高几分」。所以孩子可以人在 40，窗仍到 127，上界仍然保守。

---

## 两次回滚做错了什么

回滚的那刀看到 `rules` 在 40，就算 `up_to = 39`。

| | `up_to` | 读一次标签覆盖 |
|---|---|---|
| Lucene | 127 | 一整叠 |
| 当时 HEAD | 127（或某个孩子的 **块尾**） | 仍是块 |
| 那刀 | **39** | `security` 到下一个 `rules` 的缝 |

`airport` 上计数器：叶子浅推进 **4.1×**，optional 截断 **4.5×**，真正打分反而变少。skip 在「丢掉不够格的文档」上生效了，但读标签变成了主业。

更糟的是：HEAD 在问标签前会 `optional.advance(10)`，先把 airport、rules **都翻到 ≥10**。于是每次浅推进都看到「有孩子在前面」，截断疯狂开火。Lucene 的设计正相反：问标签时迭代器还可以停在 -1。

---

## 五句话，对着例子翻译

### 1. 窗必须是块，不是缝

`up_to` 必须来自某个 **还在窗里的** posting 的 **块尾**（约 128 篇），不能是「某一个 SHOULD 孩子的下一篇 − 1」。

可以留：整个 `airport OR rules` 都还在 80 → 窗收到 79（一层楼的大门）。  
不能做：OR 已经在 10，只因为 `rules` 在 40 → 窗收到 39（把客厅按椅子切开）。

### 2. Disjunction 不要发明截断

未定位（Lance 的 `doc() == None`，对应 Lucene `-1`）或 `doc <= 10`：在 **10** 浅推进。  
已经在 10 前面的孩子：不要 `min(doc-1)`。

Lance 还有一层硬限制，见下一节。

### 3. 浅推进不要精确推进 optional

读标签 ≠ 翻到那一页。HEAD 的 `ensure_optional_at_or_after` 写在 `advance_shallow` 里，等于每问一次标签先走一遍可选并。这是 Lucene「延迟 skipTo」的反面。

叶子：未定位时只走 skip 表上的 `shallow_next(target)`。  
跨列 merge / mapped **没有** Lucene 那种 impacts：未定位就不要 skip（fail-closed）。用「第一块最高分」去盖整段前缀，会把后面 fragment 的命中跳没——第一次刀挂 mapped oracle 就是这个。

### 4. 扔掉不够格的叠，发生在 `next` 之前

Lucene：先问标签，不够就 `target = up_to + 1`，再问下一叠，最后才 `req.advance`。  
Lance 收集器：先 `next()` 解出一篇 MUST，再问标签。可扔掉的那叠里，至少第一篇 MUST 已经解出来了。

`ReqOpt::position` 里已有 combined skip，但窗一旦是缝，这句打不着（leftover IU 上计数曾是 0）。要对齐 Lucene，这条回路必须在 **块** 上转，并且转在精确 `advance` 之前。

### 5. 这一叠 MUST 最高分都 `< F` → optional 临时变必有

0–127 里，只有 `security`、没有 airport/rules 的文档不可能进 top-10。这叠临时变成求交。HEAD 已经在做。不要 sticky 把整个 OR 永远提升成 MUST，也不要把 `F - 可选词全局最高分` 塞给 MUST。

---

## Lance 不能原样抄的一句（第一性原理）

Lucene 可以「浅推进只看 `docID <= target` 的孩子，但 `getMaxScore(127)` 仍计入人在 40 的孩子」——因为 impacts **能跨块** 回答上界。

Lance 的 `WandCursor` / `MaterializedScorer` **一次只缓存一块**。`score_bounds(up_to)` 要求 `up_to` 落在这块里。如果窗右端是 127，而 `rules` 只在 40 浅推进过、块只到 54，问 `score_bounds(127)` 会直接崩；若假装这块的最高分覆盖到 127，后面一块若更高，上界偏低，会 **多跳、丢命中**。

所以 Lance 在孩子 **已经定位且在前面** 时，HEAD 用 `advance_shallow(孩子自己的文档)`，并把 **那一块的块尾** 纳入 `min`。那仍是块，不是 `doc-1`。人在 40、块到 54 时，窗收到 54，下一窗再开。比 39 粗，比「假跨块」安全。

**这刀要抄的是：问标签前不要 `advance` optional；未定位孩子在 target 浅推进；整段 OR 在前面才截大门；先问标签再精确推进 MUST。不要抄「每个 SHOULD 孩子 doc-1」。**

---

## 不变量（落地必须有测试钉住）

1. 一块 MUST 里夹着一个 SHOULD 命中时，`advance_shallow` 的 `up_to` **≥ 该 MUST 块尾**，且 **≠ should_doc - 1**。
2. 只浅推进、不 `advance` optional 时，optional 的 `advance` 计数仍可以是 0，但窗上界必须能看到 optional 的块最高分（TermLeaf / materialized）。
3. mapped / merge 未定位不得把后面 fragment 的命中跳没（oracle 同分也要留较小 row id）。conjunction 里一个孩子报 ZERO、另一个报真上界时，合起来仍不能当成整窗证明。
4. 地板升高后，MUST 的精确 `advance` 次数下降，且 MUST 的浅推进次数按 **块数** 涨，不按 SHOULD 缝线性涨。
5. skip 循环只在 `up_to > target`（真·块）时用浅推进跳；`up_to == target` 是 merge 在 fragment 之间的 **1 号缝**。再用 `advance_shallow` 会按整数扫 row-address 空洞（会挂死）。这种缝改走 posting 的 `required.advance(up_to + 1)`。
6. Lance 叶子一次只缓存一块，且这块的左端可以是 **第一篇 posting**，不是 probe `target`。并集因此 **不能** 对未定位孩子做浅推进：否则父窗右端来自另一个孩子，问标签会落在这块左边并崩掉（`college +admissions criteria`）。Lucene impacts 能回答「target 到 upTo」；Lance 不行。未定位并集孩子改为 fail-closed，不问 `score_bounds`。

违反 1 或 4，SBG leftover IU 会再变慢。违反 2、3、5 或 6，正确性挂、死循环或内部错误。
