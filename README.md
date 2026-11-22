# Shopping Sessions: Rust pipeline

An entry for the Databricks "Shopping Sessions" buildathon track. It turns dirty click-stream CSVs in
`/data` into `/output/user_daily/` and `/output/campaign_daily/` Parquet tables, following `rules.md`.

```sh
make venv            # once: Python + polars for the test scripts
make dev             # once: generate the 1M-event dev set (unzip dev-answer.zip to dev-answer/)
make test            # native: 36 fixtures + exact diff against the dev answer
make gate            # Docker with scorer flags: fixtures + dev + 20M bench
```

The image's `CMD` runs `/app/pipeline` with no arguments. See `AGENTS.md` for the constraints, the spec
traps and the design.
