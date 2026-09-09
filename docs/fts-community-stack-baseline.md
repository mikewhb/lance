# 社区 FTS stack 基准（main + `#9033` + `#9030`）

日期：2026-09-09  
生产清单：[fts-next-optimizations-production.md](./fts-next-optimizations-production.md)（叠在这份基线上加什么）  
分析分支全量数字：[fts-next-optimizations.md](./fts-next-optimizations.md) 第 6 节（`perf/fts-top10-analysis` @ `3d1189834`，**不要**和本表横比当 PR 收益）

本文只记录 **往社区贡献之前** 的尺子。后面每个 PR 用这份 JSON 比，不要用分析分支的 1.16×。

---

## 树与协议

| | |
|---|---|
| 分支 | `bench/community-fts-stack` |
| HEAD | `15d48f91e` `perf(fts): accelerate wide and queries with bulk intersection`（`#9030`） |
| 相对 `origin/main` @ `468f6e892` | 已含 `#9031` / `#9032`；cherry-pick `#9033` `e5658f895` + `#9030` `15d48f91e` |
| 二进制 | `/tmp/sbg-community/release/do_query`，`target-cpu=native`，thin LTO |
| 索引 | `/dev/shm/sbg/indexes/lance-f03a2783c24f`（A1）、`…-mt`（A2），未重建 |
| 原始 JSON | `.agent/fts-community-stack-baseline/results.json` |
| 脚本 | `.agent/fts-community-stack-baseline/run-sbg-topk.sh` |

协议与 [fts-bench-lance-lucene-tantivy.md](./fts-bench-lance-lucene-tantivy.md) 相同：英文 Wikipedia 5,032,104 篇，943 条 AOL 派生查询，`client.py` 热身 60s、每条 10 轮取最好时间。`COMMANDS='TOP_10 TOP_100'`，四引擎：Tantivy 0.25、Lucene 10.3.0、Lance A2、Lance A1。墙钟 **12m 24s**，`BENCH_EXIT:0`。所有 `LANCE_HACK_*` / `LANCE_DIAG_*` 未设置。

这是 **`do_query` 墙钟**（plan + 叶子 + DataFusion drain），不是 `probe_wiki search_us`。

| | A1 `lance-f03a2783c24f` | A2 `lance-f03a2783c24f-mt` |
|---|---|---|
| FTS partition | 1 | 产品默认（本机 4） |
| Tokio | `current_thread` | `multi_thread` |
| `SEARCH_CHUNK` | 1 | 16（默认） |

---

## 全量

| Command | Engine | AVERAGE μs | P50 | P90 | P99 | ×Lucene avg |
|---|---|---:|---:|---:|---:|---:|
| TOP_10 | Lucene 10.3.0 | 1,480 | 632 | 2,568 | 10,327 | 1.00× |
| TOP_10 | Tantivy 0.25 | 1,889 | 633 | 2,977 | 14,484 | 1.28× |
| TOP_10 | Lance A1 | **3,911** | 1,330 | 7,196 | 57,681 | **2.64×** |
| TOP_10 | Lance A2 | 4,113 | 1,472 | 7,089 | 57,117 | 2.78× |
| TOP_100 | Lucene 10.3.0 | 2,002 | 874 | 3,569 | 13,741 | 1.00× |
| TOP_100 | Tantivy 0.25 | 2,211 | 751 | 3,583 | 19,472 | 1.10× |
| TOP_100 | Lance A1 | **4,792** | 1,714 | 8,730 | 59,965 | **2.39×** |
| TOP_100 | Lance A2 | 5,004 | 1,882 | 9,400 | 59,292 | 2.50× |

对照同机分析分支 @ `3d1189834`（hack 全关、同一套索引与 `client.py`）：

| | 社区 A1 TOP_10 | 分析 A1 TOP_10 | 社区 A1 TOP_100 | 分析 A1 TOP_100 |
|---|---:|---:|---:|---:|
| AVERAGE μs | 3,911 | 1,775 | 4,792 | 2,470 |
| ×Lucene | 2.64× | 1.16× | 2.39× | 1.20× |

Lucene 两次跑在噪声里（1,480 vs 1,536）。A1 从 3.9ms 收到 1.8ms 是分析分支上的叶子核，不是机器。往社区合入时，收益相对 **本表的 3,911 / 4,792**，不是相对 1,775。

---

## 按查询形态（AVERAGE μs）

| Command | Tag | n | Lucene | Tantivy | A1 | A2 | ×L A1 |
|---|---|---:|---:|---:|---:|---:|---:|
| TOP_10 | union | 301 | 1,150 | 1,788 | 2,538 | 2,608 | 2.21× |
| TOP_10 | intersection | 300 | 938 | 1,156 | 2,115 | 2,341 | 2.26× |
| TOP_10 | phrase | 300 | 1,988 | 1,777 | 3,860 | 4,187 | 1.94× |
| TOP_10 | intersection_union | 40 | 3,138 | 3,187 | **26,784** | 26,859 | **8.53×** |
| TOP_10 | term (`the`) | 1 | 2,225 | 2,289 | 679 | 864 | 0.31× |
| TOP_10 | two-phase-critic | 1 | 44,000 | 233,284 | 59,676 | 59,990 | 1.36× |
| TOP_100 | union | 301 | 1,935 | 2,793 | 3,905 | 4,343 | 2.02× |
| TOP_100 | intersection | 300 | 1,188 | 1,154 | 2,521 | 2,681 | 2.12× |
| TOP_100 | phrase | 300 | 2,576 | 1,778 | 4,499 | 4,622 | 1.75× |
| TOP_100 | intersection_union | 40 | 3,188 | 3,202 | **28,946** | 28,545 | **9.08×** |

IU 8.5× 对应生产文档 5a/5b（社区还没有升格 / tight）。union 里 `niceville high school` 12.5ms / 19×，对应第 1 条 seed 还没上。

---

## named representatives（`do_query` min μs）

| Query | TOP_10 L | A1 | ×L | TOP_100 L | A1 | ×L |
|---|---:|---:|---:|---:|---:|---:|
| `+walk +the +line` | 1,666 | 10,214 | 6.13× | 2,415 | 10,734 | 4.44× |
| `+university +of +washington` | 2,879 | 12,241 | 4.25× | 4,073 | 15,442 | 3.79× |
| `+time +for +kids` | 1,165 | 8,097 | 6.95× | 1,695 | 8,715 | 5.14× |
| `+care +a +lot` | 2,666 | 9,853 | 3.70× | 2,633 | 9,922 | 3.77× |
| `freedom tower` | 1,908 | 4,681 | 2.45× | 2,074 | 5,792 | 2.79× |
| `cheap hotels` | 442 | 1,551 | 3.51× | 757 | 1,756 | 2.32× |
| `chicago teachers union` | 1,928 | 6,039 | 3.13× | 4,860 | 8,045 | 1.66× |

`+walk +the +line` 在社区因 `#7624` 无条件进 bulk（无偏斜门）。第 2 条 BMC 前移已用本列当改前量过：walk **+10.7%**，已回滚，见 `.agent/fts-item2-bmc/note.md`。
