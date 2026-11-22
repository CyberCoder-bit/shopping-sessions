#!/usr/bin/env python3
"""
Independent, literal, slow reference implementation of rules.md (Shopping Sessions).

Written from rules.md (and fixtures/README.md) only, without looking at the Rust code, to cross-check it.
Pure Python 3 standard library for all logic; polars is imported only to write the two Parquet files.

    DATA_DIR=dev OUTPUT_DIR=out-ref python reference/ref.py

Reads $DATA_DIR (default /data). Writes $OUTPUT_DIR/user_daily/part-0.parquet and
$OUTPUT_DIR/campaign_daily/part-0.parquet (default /output). Everything it prints goes to stderr.

Conventions used throughout: every value is `bytes`, exactly as it appears between the separators (after
removing the CSV quotes that wrap a whole field). Comparisons of IDs, categories, event types and campaign
IDs are therefore exact and byte-wise; Python's `bytes` ordering is byte-wise lexicographic, with a proper
prefix sorting first, which is what "byte-wise smallest" means. An empty value (b"") is null.
"""
import bisect
import datetime
import json
import os
from pathlib import Path
import re
import sys
import time

DAY_MS = 86_400_000
LATE_LIMIT_MS = 86_400_000          # events step 4: drop if ingest_ts - ts is OVER 24 hours
SESSION_GAP_MS = 1_800_000          # events step 5: new session if the gap is OVER 30 minutes
LOOKBACK_MS = 7 * DAY_MS            # clicks: 7 days = 604,800,000 ms
assert LOOKBACK_MS == 604_800_000

EVENT_TYPES = (b"view", b"cart", b"purchase")
UNKNOWN_CATEGORY = b"unknown"
ORGANIC = b"organic"

T0 = time.time()


def log(*args):
    print(f"[ref {time.time() - T0:7.2f}s]", *args, file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------------------------------------
# CSV dialect
# ---------------------------------------------------------------------------------------------------------

def csv_files(folder):
    """Every *.csv file directly inside `folder` (sorted, although the order never matters)."""
    if not folder.is_dir():
        log(f"WARNING: {folder} is not a directory; treating it as empty")
        return []
    return sorted(p for p in folder.iterdir() if p.name.endswith(".csv") and p.is_file())


def data_lines(path):
    """The lines of a file after its first line (the header).

    Only b"\\n" ends a line; b"\\r" is ordinary data. The text after the last b"\\n" is a line only if it is
    non-empty (a last line without a newline). A 0-byte file has no lines at all.
    """
    lines = path.read_bytes().split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()          # what follows a final "\n" is not a line
    return lines[1:]         # skip exactly the first line, whatever it holds


def parse_line(line, n_fields):
    """Split one line into exactly n_fields values, or return None if the line is malformed.

    A field either contains no double quote at all (taken as is), or is wrapped in double quotes with no
    quote inside (the wrapping quotes are removed). Anything else, a wrong field count, or an empty line is
    malformed. No trimming of any kind.
    """
    if line == b"":
        return None
    fields = line.split(b",")
    if len(fields) != n_fields:
        return None
    if b'"' not in line:
        return fields
    values = []
    for f in fields:
        if b'"' in f:
            if len(f) >= 2 and f[:1] == b'"' and f[-1:] == b'"' and b'"' not in f[1:-1]:
                f = f[1:-1]
            else:
                return None
        values.append(f)
    return values


# ---------------------------------------------------------------------------------------------------------
# Timestamps
# ---------------------------------------------------------------------------------------------------------

_D = rb"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})"
RE_F1 = re.compile(_D + rb"Z")
RE_F2 = re.compile(_D + rb"([+-])([0-9]{2}):([0-9]{2})")
RE_DIGITS = re.compile(rb"[0-9]+")      # ASCII digits only ([0-9] in a bytes pattern is bytes 0x30-0x39)
EPOCH_ORDINAL = datetime.date(1970, 1, 1).toordinal()


def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


def days_in_month(y, m):
    if m == 2:
        return 29 if is_leap(y) else 28
    if m in (4, 6, 9, 11):
        return 30
    return 31


def civil_to_ms(y, mo, d, h, mi, s):
    """Epoch ms of a UTC calendar date-time after strict validation, or None if it is impossible."""
    if not 1 <= mo <= 12:
        return None
    if not 1 <= d <= days_in_month(y, mo):
        return None
    if not (0 <= h <= 23 and 0 <= mi <= 59 and 0 <= s <= 59):
        return None
    if y < 1:
        # Year 0000 is not representable by datetime.date. It is thousands of years outside every window,
        # so calling it bad instead of out-of-window cannot change any output (both drop the row).
        return None
    days = datetime.date(y, mo, d).toordinal() - EPOCH_ORDINAL
    return ((days * 24 + h) * 60 + mi) * 60_000 + s * 1000


def parse_ts(v):
    """F1 / F2 / F3 timestamp -> integer epoch ms (UTC), or None if it is a bad timestamp."""
    if not v:
        return None
    m = RE_F1.fullmatch(v)
    if m:
        return civil_to_ms(*(int(g) for g in m.groups()))
    m = RE_F2.fullmatch(v)
    if m:
        g = m.groups()
        local = civil_to_ms(*(int(x) for x in g[:6]))      # validate the local calendar fields first
        if local is None:
            return None
        off_h, off_m = int(g[7]), int(g[8])
        if not (0 <= off_h <= 23 and 0 <= off_m <= 59):
            return None
        offset = (off_h * 60 + off_m) * 60_000
        return local - offset if g[6] == b"+" else local + offset   # subtract the offset to get UTC
    if RE_DIGITS.fullmatch(v):
        return int(v)       # F3: any length, leading zeros fine (int-size limit is lifted in main)
    return None


def parse_f1(v):
    m = RE_F1.fullmatch(v) if v else None
    return civil_to_ms(*(int(g) for g in m.groups())) if m else None


def purchase_price(v):
    """price_cents of a purchase: ASCII digits only, value 1..=10,000,000 -> int, else None."""
    if not v or not RE_DIGITS.fullmatch(v):
        return None
    p = int(v)
    return p if 1 <= p <= 10_000_000 else None


# ---------------------------------------------------------------------------------------------------------
# Pipeline
# ---------------------------------------------------------------------------------------------------------

class Session:
    __slots__ = ("start", "n_events", "n_purchases", "revenue", "cat_sums")

    def __init__(self, start):
        self.start = start
        self.n_events = 0
        self.n_purchases = 0
        self.revenue = 0
        self.cat_sums = {}


def read_lookup(path, what):
    """products.csv / campaigns.csv (2 columns, clean, first column unique) -> dict."""
    table = {}
    bad = dups = 0
    for line in data_lines(path):
        f = parse_line(line, 2)
        if f is None or f[0] == b"":
            bad += 1
            continue
        if f[0] in table:
            dups += 1
            continue
        table[f[0]] = f[1]
    if bad or dups:
        log(f"WARNING: {what}: {bad} malformed/empty-key lines and {dups} duplicate keys (spec says clean)")
    return table


def main():
    sys.set_int_max_str_digits(0)      # F3 / price digit strings may be arbitrarily long
    data_dir = Path(os.environ.get("DATA_DIR") or "/data")
    out_dir = Path(os.environ.get("OUTPUT_DIR") or "/output")
    warnings = 0

    # --- manifest + lookups ------------------------------------------------------------------------------
    manifest = json.loads((data_dir / "manifest.json").read_bytes())
    ws = parse_ts(manifest["window_start"].encode("utf-8"))
    we = parse_ts(manifest["window_end"].encode("utf-8"))
    if ws is None or we is None:
        sys.exit(f"bad manifest window: {manifest}")
    click_ws = ws - LOOKBACK_MS
    log(f"window [{manifest['window_start']}, {manifest['window_end']}) = [{ws}, {we}) ms")

    category_of = read_lookup(data_dir / "products.csv", "products.csv")
    channel_of = read_lookup(data_dir / "campaigns.csv", "campaigns.csv")
    if UNKNOWN_CATEGORY in set(category_of.values()):
        log("WARNING: products.csv uses the category 'unknown' (spec says it never does)")
        warnings += 1
    if ORGANIC in channel_of:
        log("WARNING: campaigns.csv contains 'organic' (spec says it never does)")
        warnings += 1
    log(f"{len(category_of)} products, {len(channel_of)} campaigns")

    # --- links -> persons -------------------------------------------------------------------------------
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:                 # full path compression, iterative
            parent[x], x = root, parent[x]
        return root

    n_link_lines = n_link_used = 0
    for path in csv_files(data_dir / "links"):
        for line in data_lines(path):
            n_link_lines += 1
            f = parse_line(line, 2)
            if f is None or f[0] == b"" or f[1] == b"":
                continue                          # malformed, or a side is empty
            n_link_used += 1
            a, b = f
            parent.setdefault(a, a)
            parent.setdefault(b, b)
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[ra] = rb

    members = {}
    for x in parent:
        members.setdefault(find(x), []).append(x)
    person_of = {}
    for ids in members.values():
        name = min(ids)                           # byte-wise smallest ID of the whole component
        for x in ids:
            person_of[x] = name
    log(f"links: {n_link_lines} lines, {n_link_used} used, {len(parent)} linked IDs, {len(members)} components")

    def person(user_id):
        return person_of.get(user_id, user_id)   # an ID in no remaining link is a person on its own

    # --- events, step 1 (parse) and step 2 (dedup on event_id, smallest ingest_ts) ------------------------
    kept = {}           # event_id -> (ingest_ms, fields of the kept copy)
    n_lines = n_malformed = n_empty_id = n_copies = n_equal_ingest_conflicts = 0
    for path in csv_files(data_dir / "events"):
        for line in data_lines(path):
            n_lines += 1
            f = parse_line(line, 7)
            if f is None:
                n_malformed += 1
                continue
            event_id = f[0]
            if event_id == b"":
                n_empty_id += 1
                continue
            ingest = parse_f1(f[3])
            if ingest is None:
                sys.exit(f"ingest_ts is not valid F1 (rules.md says it always is): {line!r} in {path}")
            cur = kept.get(event_id)
            if cur is None:
                kept[event_id] = (ingest, f)
                continue
            n_copies += 1
            if ingest < cur[0]:
                kept[event_id] = (ingest, f)
            elif ingest == cur[0] and f != cur[1]:
                n_equal_ingest_conflicts += 1
    log(f"events: {n_lines} lines, {n_malformed} malformed, {n_empty_id} empty event_id, "
        f"{len(kept)} distinct event_ids, {n_copies} extra copies")
    if n_equal_ingest_conflicts:
        log(f"WARNING: {n_equal_ingest_conflicts} event copies share the min ingest_ts but differ in value "
            "(rules.md says such copies are identical); the first one read was kept")
        warnings += 1

    # --- events, step 3 (validate the kept copy) and step 4 (late) ---------------------------------------
    events_by_person = {}
    n_invalid = n_late = 0
    for event_id, (ingest, f) in kept.items():
        _, user_id, ts_raw, _, event_type, product_id, price_raw = f
        if user_id == b"":
            n_invalid += 1
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not ws <= ts < we:
            n_invalid += 1
            continue
        if event_type not in EVENT_TYPES:
            n_invalid += 1
            continue
        if product_id == b"":
            n_invalid += 1
            continue
        is_purchase = event_type == b"purchase"
        price = 0
        if is_purchase:
            price = purchase_price(price_raw)
            if price is None:
                n_invalid += 1
                continue
        # (view / cart: price_cents is ignored whatever it holds)
        if ingest - ts > LATE_LIMIT_MS:
            n_late += 1
            continue
        category = category_of.get(product_id, UNKNOWN_CATEGORY)   # step 7
        events_by_person.setdefault(person(user_id), []).append((ts, is_purchase, price, category))
    n_surviving = sum(len(v) for v in events_by_person.values())
    log(f"events: {n_invalid} invalid kept copies, {n_late} late, {n_surviving} surviving "
        f"for {len(events_by_person)} persons")

    # --- events, step 5 (sessionize per person) -----------------------------------------------------------
    sessions_by_person = {}
    n_sessions = 0
    for p, evs in events_by_person.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        prev_ts = None
        for ts, is_purchase, price, category in evs:
            # equal timestamps have gap 0, so they always share the session; the gap is ts - previous ts
            if prev_ts is None or ts - prev_ts > SESSION_GAP_MS:
                sessions.append(Session(ts))     # a session starts at its first event's ts
            s = sessions[-1]
            s.n_events += 1
            if is_purchase:
                s.n_purchases += 1
                s.revenue += price
                s.cat_sums[category] = s.cat_sums.get(category, 0) + price
            prev_ts = ts
        sessions_by_person[p] = sessions
        n_sessions += len(sessions)
    log(f"sessions: {n_sessions}")

    # --- clicks: step 1 (parse), step 2 (dedup on click_id), step 3 (validate) ---------------------------
    click_copy = {}
    n_lines = n_malformed = n_empty_id = n_copies = n_click_conflicts = 0
    for path in csv_files(data_dir / "clicks"):
        for line in data_lines(path):
            n_lines += 1
            f = parse_line(line, 4)
            if f is None:
                n_malformed += 1
                continue
            if f[0] == b"":
                n_empty_id += 1
                continue
            cur = click_copy.get(f[0])
            if cur is None:
                click_copy[f[0]] = f
            else:
                n_copies += 1
                if cur != f:
                    n_click_conflicts += 1
    if n_click_conflicts:
        log(f"WARNING: {n_click_conflicts} click copies differ in value (rules.md says copies are identical); "
            "the first one read was kept")
        warnings += 1

    clicks_by_person = {}
    n_valid_clicks = 0
    for click_id, (_, user_id, campaign_id, ts_raw) in click_copy.items():
        if user_id == b"":
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not click_ws <= ts < we:
            continue
        if campaign_id not in channel_of:
            continue
        clicks_by_person.setdefault(person(user_id), []).append((ts, click_id, campaign_id))
        n_valid_clicks += 1
    for lst in clicks_by_person.values():
        lst.sort()
    log(f"clicks: {n_lines} lines, {n_malformed} malformed, {n_empty_id} empty click_id, "
        f"{len(click_copy)} distinct click_ids, {n_copies} extra copies, {n_valid_clicks} valid")

    # --- clicks, steps 4-7 (credit + assists), and both output tables ------------------------------------
    user_rows = {}      # (person, day) -> [sessions, events, revenue, {category: summed price}]
    camp_rows = {}      # (campaign, day) -> [sessions, purchasing_sessions, revenue, assisted_sessions]
    n_credited = n_assists = 0
    for p, sessions in sessions_by_person.items():
        clicks = clicks_by_person.get(p, [])
        click_ts = [c[0] for c in clicks]
        for s in sessions:
            lo_ts, hi_ts = s.start - LOOKBACK_MS, s.start
            # clicks with s.start - 7d <= ts <= s.start (clicks is sorted by ts)
            window = clicks[bisect.bisect_left(click_ts, lo_ts):bisect.bisect_right(click_ts, hi_ts)]
            if len(clicks) <= 64:       # cheap literal cross-check of the bisect slice
                assert window == [c for c in clicks if lo_ts <= c[0] <= hi_ts]
            if window:
                # latest ts; on a tie, the byte-wise smallest click_id
                best = min(window, key=lambda c: (-c[0], c[1]))
                credited = best[2]
                n_credited += 1
            else:
                credited = ORGANIC
            assists = {c[2] for c in window}     # each campaign once; organic never assists
            n_assists += len(assists)

            day = s.start // DAY_MS             # UTC date of the session start
            u = user_rows.get((p, day))
            if u is None:
                u = user_rows[(p, day)] = [0, 0, 0, {}]
            u[0] += 1
            u[1] += s.n_events
            u[2] += s.revenue
            for cat, amount in s.cat_sums.items():
                u[3][cat] = u[3].get(cat, 0) + amount

            c = camp_rows.get((credited, day))
            if c is None:
                c = camp_rows[(credited, day)] = [0, 0, 0, 0]
            c[0] += 1
            c[1] += 1 if s.n_purchases > 0 else 0
            c[2] += s.revenue
            for campaign in assists:
                a = camp_rows.get((campaign, day))
                if a is None:
                    a = camp_rows[(campaign, day)] = [0, 0, 0, 0]
                a[3] += 1
    log(f"credited sessions {n_credited}, organic {n_sessions - n_credited}, assists {n_assists}; "
        f"user_daily rows {len(user_rows)}, campaign_daily rows {len(camp_rows)}")

    write_outputs(out_dir, user_rows, camp_rows, channel_of)
    log(f"done ({warnings} warnings)")


def write_outputs(out_dir, user_rows, camp_rows, channel_of):
    import polars as pl     # used only to write Parquet

    def text(b):
        return b.decode("utf-8")        # input is UTF-8; fail loudly if it is not

    def day_of(day_number):
        return datetime.date.fromordinal(EPOCH_ORDINAL + day_number)

    u_user, u_day, u_sessions, u_events, u_revenue, u_top = [], [], [], [], [], []
    for (p, day), (sessions, events, revenue, cat_sums) in user_rows.items():
        u_user.append(text(p))
        u_day.append(day_of(day))
        u_sessions.append(sessions)
        u_events.append(events)
        u_revenue.append(revenue)
        if cat_sums:
            # highest summed purchase price; a tie goes to the byte-wise smallest category
            u_top.append(text(min(cat_sums.items(), key=lambda kv: (-kv[1], kv[0]))[0]))
        else:
            u_top.append(None)
    user_daily = pl.DataFrame([
        pl.Series("user_id", u_user, dtype=pl.String),
        pl.Series("day", u_day, dtype=pl.Date),
        pl.Series("sessions", u_sessions, dtype=pl.Int64),
        pl.Series("events", u_events, dtype=pl.Int64),
        pl.Series("revenue_cents", u_revenue, dtype=pl.Int64),
        pl.Series("top_category", u_top, dtype=pl.String),
    ])

    c_id, c_channel, c_day, c_sessions, c_purch, c_revenue, c_assisted = [], [], [], [], [], [], []
    for (campaign, day), (sessions, purchasing, revenue, assisted) in camp_rows.items():
        assert sessions > 0 or assisted > 0
        c_id.append(text(campaign))
        c_channel.append(text(ORGANIC if campaign == ORGANIC else channel_of[campaign]))
        c_day.append(day_of(day))
        c_sessions.append(sessions)
        c_purch.append(purchasing)
        c_revenue.append(revenue)
        c_assisted.append(0 if campaign == ORGANIC else assisted)
    campaign_daily = pl.DataFrame([
        pl.Series("campaign_id", c_id, dtype=pl.String),
        pl.Series("channel", c_channel, dtype=pl.String),
        pl.Series("day", c_day, dtype=pl.Date),
        pl.Series("sessions", c_sessions, dtype=pl.Int64),
        pl.Series("purchasing_sessions", c_purch, dtype=pl.Int64),
        pl.Series("revenue_cents", c_revenue, dtype=pl.Int64),
        pl.Series("assisted_sessions", c_assisted, dtype=pl.Int64),
    ])

    for name, frame in (("user_daily", user_daily), ("campaign_daily", campaign_daily)):
        folder = out_dir / name
        folder.mkdir(parents=True, exist_ok=True)
        stale = [p.name for p in folder.glob("*.parquet")
                 if p.name != "part-0.parquet" and not p.name.startswith((".", "_"))]
        if stale:
            sys.exit(f"{folder} already holds other parquet files {stale[:5]}; use an empty OUTPUT_DIR")
        frame.write_parquet(folder / "part-0.parquet")
        log(f"wrote {folder / 'part-0.parquet'} ({frame.height} rows)")


if __name__ == "__main__":
    main()
