# 本地性能基线

使用已有 Rust 测试依赖和 Python 3.11+ 标准库及 Linux GNU time；不访问互联网或真实 AI API。

```sh
cargo build --release --locked --bin rss-ai-news
python3 tools/perf/run.py --output /tmp/rss-perf.json
```

默认每个场景运行三次独立测试进程，报告中位数及原始样本。Rust 内部计时排除 fixture 建立；GNU time 的 peak RSS 包括 fixture 建立和测试 runtime。不同 ingest 条目数共用一进程，因此其 RSS 相同，不能解释为某个条目数单独消耗。保持同机器、工具链、构建 profile 比较，避免同时编译其他任务。

场景包括 feed parse、10/100/1000 条 SQLite ingest 与重复去重、1000 个源配置、30 条报告渲染、Readability HTML fixture、100 个 mock AI 任务。PG 性能必须另备真实数据库，当前脚本不伪造 PG 数字。

`--executable` 可指定预先复制的 baseline 测试二进制，`--binary` 指定与之对应的 CLI binary（大小与 SHA-256），`--repeat 1..20` 调整采样数。报告保留 Git HEAD、dirty 状态、rustc、平台和 release binary 大小；不做时间阈值 gate。GitHub maintenance workflow 手动触发时上传数据。

SQLite 查询计划可运行 `python3 tools/perf/query_plans.py`；使用当前 migration 与生产 SQL，在内存数据库建10k行、执行 ANALYZE，不修改用户数据库。

若系统未安装GNU time，先安装发行版的 `time` 包，或用 `--time-command /path/to/time` 指定已解包的可执行文件。不用Python fork的resourceusage代替，否则其解释器RSS会污染小进程测量下限。
