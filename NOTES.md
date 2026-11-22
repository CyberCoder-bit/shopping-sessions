# NOTES: running log

Keep this current: what was tried, what it measured, what was submitted. Newest first within each section.

## Status

- 2026-11-22 09:40: **v3.1** (33c5e15...) gate green. bench 20M native ~1.4-1.6 s (noisy box), Docker 1.63 s in-container. Pushed for submission #3.
- 2026-11-22 09:28: **v2** (d0820ab) gate green, pushed. Submission #2 candidate.
- 2026-11-22 09:19: **v1** (6798d8c) gate green, pushed to github.com/CyberCoder-bit/shopping-sessions. Submission #1 candidate.
- 2026-11-22 09:16: **v1 done.** 36/36 fixtures, dev MATCH, and MATCH vs `reference/ref.py` on seeds 15, 62 (leap-day windows), 3 and 7. bench (20M) native 2.23 s warm, maxrss 4.2 GB.
- 2026-11-21: Repo set up. Rust skeleton writes both tables with the right schema and no rows.
  `make fixtures-local`: 2/36 (only the cases with empty expected output). Docker image builds (115 MB) and
  runs under scorer flags.

## Submissions (2 per hour; log every one)

| When (EST) | Commit | Dev check | Practice result | Practice time (s) | Notes |
| --- | --- | --- | --- | --- | --- |

## Local timings

| When | Commit | Dataset | Mode | Wall (s) | Max RSS | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-11-22 | v1 | bench 20M | native, 8 cores | 2.23 | 4.2 GB | warm cache. Stages: links 0.26, ev parse 0.59, dedup 0.54, clicks 0.23, sessions 0.32, write 0.12, exit 0.14 |
| 2026-11-22 | v1 | bench 20M | native, 8 cores | 5.42 | 4.2 GB | first run, partly cold page cache (ev parse 2.1 s) |
| 2026-11-22 | v1 | dev 1M | native, 8 cores | 0.15 | 0.23 GB | |
| 2026-11-21 | skeleton | fixture A1 | docker | 0.31 | n/a | empty-container start-to-exit floor (warm) |

## Datasets on disk (gitignored)

- `dev/`: seed 1, 1M events / 250k clicks, window 2027-12-09 to 2027-12-23, 92 MB. Answer in `dev-answer/`.
- `bench/`: seed 2, 20M events / 5M clicks, window 2026-09-07 to 2026-09-21, 1.8 GB, 95 event files
  (largest 320 MB). No answer key: use selfcheck.py and diffs against the reference.
- Seeds whose window contains 2028-02-29: 15, 62, 249, 254, 272, 355.

## Decisions

- Tried and rejected: mimalloc (sys time 3-23 s, slower), GLIBC_TUNABLES=glibc.malloc.hugetlb=1 (no gain), per-thread reuse of dedup map + concat buffers (no measurable gain).
- Local timing is noisy (15 GB box, page cache evictions): compare binaries with interleaved runs and CPU time.
- Engine: Rust (custom dialect parser; generic CSV readers don't match rules.md). `parquet` crate v60,
  low-level writer, no arrow.
- `target-cpu=x86-64-v3`: the scorer has AVX-512, the local box doesn't.
- Click dedup skipped: copies are identical by rule, so no output can change (see AGENTS.md section 6).

## Open questions for organizers

-
