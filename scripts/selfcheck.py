#!/usr/bin/env python3
"""Invariant checks from rules.md for an output folder with no answer key (bench/, other seeds).

Usage: selfcheck.py OUT_DIR
Checks: schema/types, both tables non-empty, no duplicate keys, sessions and revenue_cents sums equal across tables,
sum(assisted_sessions) >= credited non-organic sessions, organic assists 0, events >= sessions,
purchasing_sessions <= sessions, top_category null iff revenue 0.
"""
from pathlib import Path
import sys

import polars as pl

from run_fixtures import SCHEMAS, load_actual


def main():
    out = Path(sys.argv[1])
    u = load_actual(out / "user_daily", SCHEMAS["user_daily"])
    c = load_actual(out / "campaign_daily", SCHEMAS["campaign_daily"])
    problems = []

    def check(ok, msg):
        if not ok:
            problems.append(msg)

    check(u.height > 0 and c.height > 0, "a table is empty (fine for some fixtures, never for a generated set)")
    check(u.height == u.n_unique(["user_id", "day"]), "user_daily: duplicate (user_id, day)")
    check(c.height == c.n_unique(["campaign_id", "day"]), "campaign_daily: duplicate (campaign_id, day)")
    us, cs = u["sessions"].sum(), c["sessions"].sum()
    ur, cr = u["revenue_cents"].sum(), c["revenue_cents"].sum()
    check(us == cs, f"sessions differ: user_daily {us} vs campaign_daily {cs}")
    check(ur == cr, f"revenue differs: user_daily {ur} vs campaign_daily {cr}")
    non_organic = c.filter(pl.col("campaign_id") != "organic")
    organic = c.filter(pl.col("campaign_id") == "organic")
    check(c["assisted_sessions"].sum() >= non_organic["sessions"].sum(),
          "sum(assisted_sessions) < credited non-organic sessions")
    check(organic["assisted_sessions"].sum() == 0, "organic has assisted_sessions")
    check((organic["channel"] == "organic").all(), "organic channel is not 'organic'")
    check((u["sessions"] >= 1).all(), "user_daily row with 0 sessions")
    check((u["events"] >= u["sessions"]).all(), "user_daily events < sessions")
    check((c["purchasing_sessions"] <= c["sessions"]).all(), "purchasing_sessions > sessions")
    check(((c["sessions"] > 0) | (c["assisted_sessions"] > 0)).all(), "campaign_daily row with nothing")
    check((u["top_category"].is_null() == (u["revenue_cents"] == 0)).all(),
          "top_category null must match revenue_cents == 0")

    print(f"user_daily rows {u.height}, campaign_daily rows {c.height}, sessions {us}, revenue {ur}")
    for p in problems:
        print("FAIL", p)
    print("SELFCHECK OK" if not problems else "SELFCHECK FAILED")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
