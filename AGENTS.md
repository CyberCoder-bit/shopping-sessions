# AGENTS.md: Shopping Sessions (Databricks buildathon track)

Read this file before touching the code. `rules.md` is the spec and the only source of truth: if this file,
the code, a fixture README or a starter disagree with `rules.md`, **`rules.md` wins**.

## 1. The job

Turn messy click-stream CSVs in `$DATA_DIR` (`/data`) into two exact Parquet tables in `$OUTPUT_DIR`
(`/output/user_daily/`, `/output/campaign_daily/`). Sessionize each person's events, where a person is a set
of user IDs joined through `links/`. Credit each session to the latest ad click within the 7 days before it,
and count assisting campaigns.

- **Score = container wall time, start to exit (lower wins).** That includes engine startup and CSV reading.
- **Exactness is binary.** One wrong row means the submission is rejected, and "almost right" scores nothing.
- Practice: hidden data of about 20M events, timed once (median of 3 if under 20 s). The cap is 2× baseline
  (baseline 169.6 s) and never more than 5 min.
- **Final: about 60M new events, 3 runs, each capped at 3 min.** Any run over 3 min, a crash, or OOM means no
  final score. Robustness beats the last 5%.
- Each submission first runs on the public 1M dev set (≤ 3 min). If that fails, the submission stops.
- Hidden runs show only the exit code, OOM yes/no, and missing/extra/differing row counts. They show no logs.
- **Budget: 2 submissions per hour per team. Cutoff: Sunday 11:00 AM EST (2026-11-22, 16:00 UTC; confirm on
  the track page).** Equal scores go to the earlier submission.
- Practice times vary by up to about 20% across scoring machines, so don't chase noise.

## 2. Container contract (scorer flags; `scripts/docker_run.sh` reproduces them)

| Thing | Value |
| --- | --- |
| Image | `linux/amd64`, ≤ 8 GB. Build has network for ≤ 10 min. Runs have **no network** |
| CPU / RAM | 8 pinned vCPUs (hyperthreads, Intel Xeon Cascade Lake+ with AVX-512), 24 GB, no swap |
| Filesystem | Root read-only. `/tmp` is a 64 MB tmpfs (don't use it). `/data` read-only. `/output` + `/scratch` writable, ≤ 50 GB total |
| User | uid 1000:1000 (no passwd entry needed) |
| Env | `DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch NCPU=8 MEM_GB=24` |
| Limits | ≤ 4096 processes+threads. **≤ 50 MB on stdout: log to stderr only** (`eprintln!`) |
| Entry | `CMD` with no args; exit 0 means "done, check my output" |
| Output | ≥ 1 `*.parquet` per table folder. Names starting with `.`/`_` are ignored. Exact column names, no extras, any int width, `day` = Parquet `DATE` |
| Repo | Public GitHub, `Dockerfile` at root, ≤ 50 MB, **no data checked in** |

## 3. Repo map

```
AGENTS.md          this file (CLAUDE.md just imports it)
rules.md           THE SPEC (organizer's). Never edit
NOTES.md           running log: submissions, timings, decisions, open questions. Keep it updated
Cargo.toml/.lock   Rust crate. Binary = `pipeline`. Lock file is committed; Docker builds --locked
.cargo/config.toml target-cpu=x86-64-v3 (AVX2). Don't raise it: the local box has no AVX-512
src/main.rs        pipeline: lookups, links/union-find, events (parse, dedup), clicks, sessions, output
src/csv.rs         file listing, mmap, chunking, the rules.md CSV dialect (split_fields)
src/ts.rs          F1/F2/F3 timestamp parsing (strict calendar checks)
src/keys.rs        u128 string keys (byte-order preserving), long-string interner, hashing, partitions
src/par.rs         parallel_for: scoped threads + atomic task counter, one state per thread
src/output.rs      Parquet writers with the exact schema (fill column vecs, call write_*)
reference/ref.py   independent pure-Python reference (36/36, dev MATCH); answers in seeds/sN-ref/
Dockerfile         rust:1.97.1-slim-bookworm build -> debian:bookworm-slim runtime, ~115 MB
fixtures/          36 hand-written edge cases (input/ + expected/*.csv). README.md says what each one catches
scripts/           run_fixtures.py, compare.py (organizer's), docker_run.sh (scorer flags), selfcheck.py (invariants)
tools/generator/   organizer's dataset generator (Rust). `make` builds it to bin/generate
dev/  dev-answer/  1M-event dev set (seed 1) + its official answer   [gitignored]
bench/             20M-event practice-size set (seed 2)                [gitignored]
seeds/             extra generated sets (`make seed`)                  [gitignored]
ref/               original starter zips/dirs (PySpark/DuckDB/Polars)  [gitignored, reference only]
.venv/             Python with polars, used by the test scripts        [gitignored]
```

## 4. Commands

```sh
make help            # everything below
make test            # FAST GATE: native build + 36 fixtures + dev run + exact diff vs dev-answer
make fixtures-local  # just the fixtures, native
make run-dev-local check-dev
make run-bench-local # native on bench/ (20M), prints wall/user/sys/maxrss, then selfcheck.py
make gate            # FULL PRE-SUBMIT GATE in Docker: fixtures + dev diff + bench timing
make seed SEED=15 EVENTS=200000   # generate seeds/s15 (window contains 2028-02-29)
make final-data      # 60M-event set in seeds/final, to test the final's scale (3 min cap, 24 GB)
.venv/bin/python scripts/compare.py OUT_A OUT_B   # exact diff of any two output dirs (or answer CSV dirs)
.venv/bin/python scripts/selfcheck.py OUT         # invariants when there's no answer key
```

Native runs are pinned with `taskset` to `CPUS` (default 8) cores and get `NCPU=8`, like the scorer. Local
box: WSL2, Intel Core Ultra 9 275HX (8P+16E, AVX2, **no AVX-512**), **~15 GB RAM** (Docker VM 16 GB), so
`--memory 24g` is not really enforced locally. Watch `maxrss` instead. Measured: empty-container start-to-exit
≈ 0.3 s warm; cold `make build` ≈ 2 min.

## 5. Ground rules for agents

1. **Correctness first, always.** No change merges unless `make test` passes (36/36 fixtures + `MATCH`
   on dev). Perf work must keep it green. Before any submission run `make gate`.
2. **Never push to GitHub or submit without the user's explicit OK.** Submissions are scarce (2/h), and the
   user submits on the track page. Commit only when asked. Log every submission in `NOTES.md` (time, SHA,
   result, practice seconds).
3. The data is untrusted and dirty. **Parsing must be total:** no `unwrap`/index panics on input bytes, no
   overflow on long digit strings, and any byte value, any field length and any file size must work
   (including 0-byte and header-only files and a last line without `\n`). A panic in the final = no score.
4. Don't hard-code anything about a dataset (window, file counts, whales, campaign count). Read it all.
5. Keep stdout nearly empty. Per-stage timings go to stderr (`eprintln!`), which is useful for profiling.
6. Don't commit data, outputs or `ref/`. Keep the repo well under 50 MB.
7. If `rules.md` is genuinely ambiguous, write the question in `NOTES.md` → *Open questions* and tell the
   user (the organizers answer questions). Don't guess silently.
8. Record measured timings and dead ends in `NOTES.md` so the next agent doesn't redo them.

## 6. Spec traps checklist (each one is a known way to fail; fixture IDs in brackets)

**CSV dialect**
- Lines end at `\n` only. `\r` is data: `1000\r` is a bad price and `...Z\r` is a bad ts [E20].
- Skip exactly the first line of every file (the header). A later line equal to the header is DATA [L3].
- A field is either fully wrapped `"..."` with no `"` inside, or contains no `"` at all. Anything else
  (`u1"`, `"a"b`, a lone `"`) makes the **whole line malformed**. Quotes are syntax: `"u1"` == `u1` [E15].
- `""` and an empty bare field are both **null**.
- Wrong field count, or an empty line, means malformed (events 7, clicks 4, links 2 fields; no comma ever
  appears inside a value).
- No trimming, no case folding, no Unicode normalization. Bytes compare exactly. ` u1`, `u1 `, `U1` and
  `u1\t` are all different IDs [E11, E19]. "Digits" means ASCII `0-9` only [E18].

**Timestamps** (to i64 epoch ms, UTC)
- F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS±HH:MM` (subtract the offset; offset hour 00-23,
  minute 00-59, colon required, `-00:00` ok) [E7, E17], F3 = ASCII digits only = epoch ms, leading zeros ok,
  any length; too big for i64 counts as out of window, not a crash [E18].
- Bad: lowercase `t`/`z`, a space for `T`, fractional seconds, no zone, month 13, Feb 30, Feb 29 outside a
  leap year, hour 24, second 60. Validate the *local* calendar fields of F2 before applying the offset.
- **Leap years:** generated windows can include 2028-02-29. Seeds 15, 62, 249, 254, 272 and 355 do. Seed 1
  (dev) does not, so test one of those seeds.
- Event window `[ws, we)`. Click window `[ws - 7d, we)` [E12, A10]. `ingest_ts` is always valid F1.

**Links → persons**
- Drop malformed lines and lines with an empty side. Union is transitive across files, in any order and
  either direction. Self-links and repeats are no-ops [L2, L3].
- The person's name is the **byte-wise smallest ID in the whole component**, even an ID never used in an
  event [L2]. Upper-case IDs (`U...`) sort before lower-case. IDs in no link are their own person.
- Chains run hundreds of links deep, and shared devices merge thousands of people into one person. Use
  union-find with path compression or halving, never recursion.

**Events** (strict order)
1. Parse. Drop malformed lines and empty `event_id`.
2. **Dedup on `event_id` BEFORE validation**: keep the min `ingest_ts` copy (equal ingest ⇒ identical
   copies). If the kept copy is invalid, the event is gone and a later valid copy doesn't rescue it [E5].
   Copies can name different users, and the kept copy's user wins [E4]. `e1` ≠ `e1 ` ≠ `e3\r`.
3. Validate the kept copy: non-empty `user_id`, `ts` valid and in the event window, `event_type` exactly
   `view|cart|purchase`, non-empty `product_id`. Purchases need `price_cents` to be ASCII digits with a value
   in `1..=10_000_000`, leading zeros ok (`0250` = 250, `000…0001` = 1, so strip zeros before checking the
   length). For view/cart, price is ignored entirely [E10].
4. Late: drop if `ingest - ts > 86_400_000` (exactly 24 h is kept, and so is ingest < ts) [E6].
5. Sessionize per **person** (all IDs together), sorted by ts. A new session starts when the gap to the
   previous event's ts is `> 1_800_000` ms (exactly 30:00.000 stays in the session). Equal ts never split [E1, E2, E16, L1].
6. Day = UTC date of the session **start**. The whole session counts on that day, even across midnight [E3].
7. Category from products.csv, else `unknown` (which competes like any other category) [E9].

**Clicks**
1. Parse. Drop malformed lines and empty `click_id`.
2. Dedup: copies have identical values, so dedup **cannot change any output** (credit tie-breaks on
   `click_id`; assists are distinct campaigns). It can be skipped. Re-check this if rules.md changes.
3. Validate: non-empty user, ts valid and in the click window, `campaign_id` exactly in campaigns.csv [A6, A7].
4. Credit: among the person's valid clicks (any of their IDs) with `start - 7d ≤ ts ≤ start`, take the max
   ts; on a tie, the byte-wise smallest `click_id` [A2-A5, A9, L4]. None means `organic`/`organic`.
5. Assists: every distinct campaign with ≥ 1 click in that same closed window, including the crediting one.
   `organic` never assists. An assist-only campaign still gets a row with 0 credited [A11, A12].

**Outputs**
- `user_daily(user_id, day DATE, sessions, events, revenue_cents, top_category?)`: one row per person-day.
  `top_category` = max summed purchase price over that person-day's sessions; a tie goes to the byte-wise
  smallest category; null if no purchases [E8, E9].
- `campaign_daily(campaign_id, channel, day DATE, sessions, purchasing_sessions, revenue_cents,
  assisted_sessions)`: a row if credited **or** assisted ≥ 1. Organic: channel `organic`, assisted 0.
- Invariants (`scripts/selfcheck.py`): Σsessions and Σrevenue equal across tables, and Σassisted ≥
  credited non-organic sessions.

## 7. Implementation plan (Rust, the chosen engine)

Why Rust: the generic CSV readers (DuckDB, Polars, Spark) don't implement this dialect, so you'd hand-parse
lines anyway, and the PySpark baseline takes 170 s. A hand-written parallel byte parser plus integer keys
should reach practice in low single-digit seconds. Suggested shape (adapt freely, measure everything):

1. **Lookups + manifest**: parse `manifest.json` (find the two keys and parse them as F1), `products.csv`
   to a category index (u8/u16; ~20 categories, 50k products), `campaigns.csv` to a dense campaign index
   (~2000) with its channel. Use the same dialect parser on them.
2. **I/O + chunking**: read/mmap every file (`memmap2`). Split big files into ~8-32 MB chunks at `\n`
   boundaries (the largest file holds ~20% of rows). Only a file's first chunk skips the header. Run with
   `rayon` (pool = `NCPU`). Practice data: events ~1.6 GB / 21M lines, clicks ~0.23 GB / 5M, links ~18 MB / 0.9M.
3. **Line parser**: `memchr` for `\n`. Fast path when the line has no `"`: split on `,` and check the
   count. Slow path: validate quote placement per field. Return byte slices, with no allocation per row.
4. **Keys**: all observed IDs are short (`u…`, `e…`, `k…`, ~9-10 bytes). Pack strings ≤ 15 bytes into a
   `u128` (bytes big-endian, left-aligned, length in the low byte). Integer comparison of that `u128` then
   equals **byte-wise string order**, so min-name and click_id tie-breaks come for free, with no hashing
   collisions. Longer strings need an exact fallback (a side table plus real byte compares). Don't rely on
   64-bit hashes being unique without detecting collisions.
5. **Links first** (small): parse → union-find over link IDs → map `id → person name key`. Every other ID
   is its own person.
6. **Events**: while parsing, compute each copy's full validity (steps 3-4 above) and keep a compact record
   `{event_key, ingest_ms, valid, ts_ms, person_key, type, category, price}`. Partition by
   `hash(event_key)` → dedup per partition (min ingest) → drop invalid → re-partition by `hash(person_key)`.
7. **Clicks**: parse + validate → `{person_key, ts_ms, click_key, campaign_idx}` → partition by
   `hash(person_key)` with the **same** partition function, so each partition holds whole persons, events
   and clicks alike, and runs fully in parallel.
8. **Per partition**: sort events by `(person, ts)` and sweep sessions (start, n_events, revenue,
   has_purchase, per-category purchase sums per person-day). Sort clicks by `(person, ts asc, click_id
   desc)`. Then for each session, the last click with `ts ≤ start` is the credit if `ts ≥ start - 7d`.
   Assists = distinct campaigns in the window slice; a 2048-bit bitset (32 × u64) per session is cheap. Keep
   per-thread dense `[campaign+organic][day]` counters and emit user_daily rows as day changes.
9. **Write Parquet** (`src/output.rs`). File count is free, so for millions of user_daily rows write
   several part files in parallel. Day = `start_ms.div_euclid(86_400_000)` as i32.

Perf notes: avoid per-row heap allocation, so use flat `Vec`s of POD records. Pre-size buffers from file
sizes. Allocators (`mimalloc`) only matter if you allocate a lot. Give every stage an `Instant` timer on
stderr. Practice ≈ 21M events: a good target is **< 5 s** end to end, which leaves the final (60M) far under
the 3-min cap. Check peak RSS at 60M (`make final-data` then a native run) and keep it well under 24 GB.
Shared-device "mega persons" (thousands of people, 10⁵ events) and whales (1-2% of all events each) are a
single key each: no O(n²) per person, and expect one partition to be heavy.

## 8. Testing beyond the dev answer

`dev-answer` exists only for seed 1 (1M events, a December window). To catch what seed 1 misses (leap day,
other dirt rates, other file layouts), keep an **independent, literal, slow reference** (e.g.
`reference/ref.py`, pure Python, written from rules.md **without** reading the Rust code). It must pass the
fixtures and match dev-answer. Then diff Rust against it on several small seeds
(`make seed SEED=15 EVENTS=200000`, also 62 and a few random ones) with `scripts/compare.py`. Run
`selfcheck.py` on bench/ and 60M outputs too.
