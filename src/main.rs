//! Shopping Sessions pipeline. rules.md is the spec; AGENTS.md has the design notes.
//!
//! Reads  $DATA_DIR    (default /data)
//! Writes $OUTPUT_DIR  (default /output): user_daily/ and campaign_daily/ as Parquet
//! Logs go to stderr only (stdout is capped at 50 MB by the scorer).
//!
//! Stages (each internally parallel over NCPU threads):
//!   lookups -> links (union-find, person = smallest ID) -> events parse+validate into
//!   partitions by event_id -> dedup (min ingest_ts, before validation) -> re-partition
//!   survivors by person -> clicks parse+validate into the same person partitions ->
//!   per partition: sort, sessionize, credit, assists -> merge -> write Parquet.

mod csv;
mod keys;
mod output;
mod par;
mod ts;

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use memmap2::{Advice, Mmap};
use parquet::data_type::ByteArray;

use keys::{key_cmp, key_map, pack, part_of, KeyMap, P};
use output::{CampaignDaily, Result, UserDaily};
use ts::{parse_ts, DAY_MS};

const GAP_MS: i64 = 1_800_000;
const WEEK_MS: i64 = 7 * DAY_MS;
const LATE_MS: i64 = DAY_MS;
const CHUNK: usize = 4 << 20;

/// One parsed event copy, before dedup. `kind`: 0 = invalid, 1 = view/cart, 2 = purchase.
#[derive(Clone, Copy)]
struct RawEv {
    ek: u128,
    user: u128,
    ingest: i64,
    ts: i64,
    price: u32,
    cat: u16,
    kind: u8,
}

/// A surviving event, keyed by person.
#[derive(Clone, Copy)]
struct Ev {
    person: u128,
    ts: i64,
    price: u32,
    cat: u16,
    purchase: bool,
}

/// A valid click, keyed by person.
#[derive(Clone, Copy)]
struct Click {
    person: u128,
    ck: u128,
    ts: i64,
    camp: u32,
}

struct Lookups {
    ws: i64,
    we: i64,
    /// Categories (plus `unknown`) sorted byte-wise: index order is the tie-break order.
    cats: Vec<Vec<u8>>,
    unknown: u16,
    prod_short: KeyMap<u16>,
    prod_long: HashMap<Vec<u8>, u16>,
    camp_short: KeyMap<u32>,
    camp_long: HashMap<Vec<u8>, u32>,
    camp_ids: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
}

impl Lookups {
    #[inline]
    fn category(&self, pid: &[u8]) -> u16 {
        let found = if pid.len() <= 15 { self.prod_short.get(&pack(pid)) } else { self.prod_long.get(pid) };
        found.copied().unwrap_or(self.unknown)
    }

    #[inline]
    fn campaign(&self, cid: &[u8]) -> Option<u32> {
        let found = if cid.len() <= 15 { self.camp_short.get(&pack(cid)) } else { self.camp_long.get(cid) };
        found.copied()
    }
}

struct Persons {
    index: KeyMap<u32>,
    person_of: Vec<u128>,
}

impl Persons {
    #[inline]
    fn resolve(&self, user: u128) -> u128 {
        match self.index.get(&user) {
            Some(&i) => self.person_of[i as usize],
            None => user,
        }
    }
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("pipeline: error: {error}");
        std::process::exit(1);
    }
    // Skip tearing down the big buffers: the output files are closed and complete.
    std::process::exit(0);
}

/// Value of a top-level string field in a small JSON object.
fn json_str(s: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let rest = &s[s.find(&quoted)? + quoted.len()..];
    let rest = &rest[rest.find(':')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_string())
}

/// Calls `f` with the fields of every well-formed data line of a small CSV file.
fn small_csv<const N: usize>(path: &Path, mut f: impl FnMut([&[u8]; N])) -> Result<()> {
    let data = fs::read(path)?;
    let Some(header_end) = memchr::memchr(b'\n', &data) else {
        return Ok(());
    };
    csv::for_each_line(&data[header_end + 1..], |line| {
        if let Some(fields) = csv::split_fields::<N>(line) {
            f(fields)
        }
    });
    Ok(())
}

fn load_lookups(data: &Path) -> Result<Lookups> {
    let manifest = fs::read_to_string(data.join("manifest.json"))?;
    let window = |key: &str| -> Result<i64> {
        json_str(&manifest, key)
            .and_then(|s| parse_ts(s.as_bytes()))
            .ok_or_else(|| format!("manifest.json: bad {key}").into())
    };
    let (ws, we) = (window("window_start")?, window("window_end")?);

    let mut products: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    small_csv::<2>(&data.join("products.csv"), |[p, c]| {
        if !p.is_empty() {
            products.push((p.to_vec(), c.to_vec()));
        }
    })?;
    let mut cats: Vec<Vec<u8>> = products.iter().map(|(_, c)| c.clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort_unstable();
    cats.dedup();
    let cat_index: HashMap<Vec<u8>, u16> = cats.iter().enumerate().map(|(i, c)| (c.clone(), i as u16)).collect();
    let unknown = cat_index[b"unknown".as_slice()];
    let mut prod_short = key_map(products.len());
    let mut prod_long = HashMap::new();
    for (p, c) in &products {
        let ci = cat_index[c.as_slice()];
        if p.len() <= 15 {
            prod_short.entry(pack(p)).or_insert(ci);
        } else {
            prod_long.entry(p.clone()).or_insert(ci);
        }
    }

    let mut camp_ids: Vec<Vec<u8>> = Vec::new();
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut camp_short = key_map(4096);
    let mut camp_long: HashMap<Vec<u8>, u32> = HashMap::new();
    small_csv::<2>(&data.join("campaigns.csv"), |[c, ch]| {
        if c.is_empty() {
            return;
        }
        let next = camp_ids.len() as u32;
        let fresh = if c.len() <= 15 {
            match camp_short.entry(pack(c)) {
                Entry::Vacant(e) => {
                    e.insert(next);
                    true
                }
                Entry::Occupied(_) => false,
            }
        } else {
            match camp_long.entry(c.to_vec()) {
                Entry::Vacant(e) => {
                    e.insert(next);
                    true
                }
                Entry::Occupied(_) => false,
            }
        };
        if fresh {
            camp_ids.push(c.to_vec());
            channels.push(ch.to_vec());
        }
    })?;
    Ok(Lookups { ws, we, cats, unknown, prod_short, prod_long, camp_short, camp_long, camp_ids, channels })
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let grand = parent[parent[x as usize] as usize];
        parent[x as usize] = grand;
        x = grand;
    }
    x
}

/// Union-find over all link pairs; each ID's person is the byte-wise smallest ID of its set.
fn build_persons(pairs: Vec<Vec<(u128, u128)>>) -> Persons {
    let n_pairs: usize = pairs.iter().map(Vec::len).sum();
    let mut index: KeyMap<u32> = key_map(n_pairs + 16);
    let mut keys_of: Vec<u128> = Vec::with_capacity(n_pairs + 16);
    let mut parent: Vec<u32> = Vec::with_capacity(n_pairs + 16);
    let mut size: Vec<u32> = Vec::with_capacity(n_pairs + 16);
    let mut id_of = |k: u128, keys_of: &mut Vec<u128>, parent: &mut Vec<u32>, size: &mut Vec<u32>| -> u32 {
        *index.entry(k).or_insert_with(|| {
            let id = keys_of.len() as u32;
            keys_of.push(k);
            parent.push(id);
            size.push(1);
            id
        })
    };
    for chunk in &pairs {
        for &(a, b) in chunk {
            let ia = id_of(a, &mut keys_of, &mut parent, &mut size);
            let ib = id_of(b, &mut keys_of, &mut parent, &mut size);
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                let (big, small) = if size[ra as usize] >= size[rb as usize] { (ra, rb) } else { (rb, ra) };
                parent[small as usize] = big;
                size[big as usize] += size[small as usize];
            }
        }
    }
    let n = keys_of.len();
    let mut name = keys_of.clone();
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if key_cmp(keys_of[i], name[r]).is_lt() {
            name[r] = keys_of[i];
        }
    }
    let person_of = (0..n).map(|i| name[find(&mut parent, i as u32) as usize]).collect();
    Persons { index, person_of }
}

/// `price_cents` of a purchase: ASCII digits only, value 1..=10,000,000.
#[inline]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let zeros = b.iter().take_while(|&&c| c == b'0').count();
    let digits = &b[zeros..];
    if digits.len() > 8 {
        return None;
    }
    let v = digits.iter().fold(0u32, |v, &c| v * 10 + (c - b'0') as u32);
    (1..=10_000_000).contains(&v).then_some(v)
}

/// Fills `r` (ts, user, kind, price, cat) if the copy is valid; leaves kind = 0 otherwise.
#[inline]
fn validate_event(r: &mut RawEv, f: &[&[u8]; 7], lk: &Lookups) {
    let [_, uid, tsf, _, typ, pid, price] = *f;
    if uid.is_empty() || pid.is_empty() {
        return;
    }
    let Some(t) = parse_ts(tsf) else { return };
    if t < lk.ws || t >= lk.we || r.ingest - t > LATE_MS {
        return;
    }
    let kind = match typ {
        b"view" | b"cart" => 1,
        b"purchase" => 2,
        _ => return,
    };
    if kind == 2 {
        let Some(p) = parse_price(price) else { return };
        r.price = p;
        r.cat = lk.category(pid);
    }
    r.user = pack(uid);
    r.ts = t;
    r.kind = kind;
}

fn new_bufs<T>(cap: usize) -> Vec<Vec<T>> {
    (0..P).map(|_| Vec::with_capacity(cap)).collect()
}

/// [thread][partition] -> [partition] of per-thread pieces, each take-able once.
fn transpose<T>(per_thread: Vec<Vec<Vec<T>>>) -> Vec<Mutex<Vec<Vec<T>>>> {
    let mut parts: Vec<Vec<Vec<T>>> = (0..P).map(|_| Vec::with_capacity(per_thread.len())).collect();
    for bufs in per_thread {
        for (p, b) in bufs.into_iter().enumerate() {
            parts[p].push(b);
        }
    }
    parts.into_iter().map(Mutex::new).collect()
}

fn take<T>(slot: &Mutex<Vec<Vec<T>>>) -> Vec<Vec<T>> {
    std::mem::take(&mut *slot.lock().unwrap())
}

fn concat<T: Copy>(mut pieces: Vec<Vec<T>>) -> Vec<T> {
    if pieces.len() == 1 {
        return pieces.pop().unwrap();
    }
    let mut all = Vec::with_capacity(pieces.iter().map(Vec::len).sum());
    for piece in pieces {
        all.extend_from_slice(&piece);
    }
    all
}

struct URow {
    person: u128,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: i64,
    /// Category index, or -1 for null.
    top: i32,
}

/// Per-thread accumulators for the session stage.
struct Acc {
    ndays: usize,
    /// [campaign (organic last)][day] -> sessions, purchasing, revenue, assisted.
    camp: Vec<[i64; 4]>,
    rows: Vec<URow>,
    catsum: Vec<i64>,
    cat_touched: Vec<u16>,
    bits: Vec<u64>,
    touched: Vec<u32>,
}

impl Acc {
    fn new(n_camp_rows: usize, ndays: usize, ncat: usize) -> Self {
        Acc {
            ndays,
            camp: vec![[0; 4]; n_camp_rows * ndays],
            rows: Vec::new(),
            catsum: vec![0; ncat],
            cat_touched: Vec::new(),
            bits: vec![0; n_camp_rows.div_ceil(64)],
            touched: Vec::new(),
        }
    }

    #[inline]
    fn add_cat(&mut self, cat: u16, price: i64) {
        let slot = &mut self.catsum[cat as usize];
        if *slot == 0 {
            self.cat_touched.push(cat);
        }
        *slot += price;
    }

    fn flush(&mut self, person: u128, day: i64, sessions: u32, events: u32, revenue: i64) {
        let mut top = -1i32;
        let mut best = -1i64;
        for &c in &self.cat_touched {
            let v = self.catsum[c as usize];
            if v > best || (v == best && (c as i32) < top) {
                best = v;
                top = c as i32;
            }
            self.catsum[c as usize] = 0;
        }
        self.cat_touched.clear();
        self.rows.push(URow { person, day: day as i32, sessions, events, revenue, top });
    }

    /// Sessionizes one person (events sorted by ts, clicks by ts asc then click_id desc),
    /// credits and assists each session, and emits the person's user_daily rows.
    fn person(&mut self, evs: &[Ev], cls: &[Click], d0: i64, organic: usize) {
        let person = evs[0].person;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut cur_day = i64::MIN;
        let (mut day_sessions, mut day_events, mut day_revenue) = (0u32, 0u32, 0i64);
        let mut k = 0;
        while k < evs.len() {
            let start = evs[k].ts;
            let day = start.div_euclid(DAY_MS);
            if day != cur_day {
                if cur_day != i64::MIN {
                    self.flush(person, cur_day, day_sessions, day_events, day_revenue);
                }
                cur_day = day;
                (day_sessions, day_events, day_revenue) = (0, 0, 0);
            }
            let mut last = start;
            let (mut n, mut revenue, mut purchased) = (0u32, 0i64, false);
            let mut m = k;
            while m < evs.len() && evs[m].ts - last <= GAP_MS {
                let e = evs[m];
                n += 1;
                if e.purchase {
                    revenue += e.price as i64;
                    purchased = true;
                    self.add_cat(e.cat, e.price as i64);
                }
                last = e.ts;
                m += 1;
            }
            day_sessions += 1;
            day_events += n;
            day_revenue += revenue;

            while hi < cls.len() && cls[hi].ts <= start {
                hi += 1;
            }
            while lo < hi && cls[lo].ts < start - WEEK_MS {
                lo += 1;
            }
            let di = (day - d0) as usize;
            let credited = if hi > lo { cls[hi - 1].camp as usize } else { organic };
            let cell = &mut self.camp[credited * self.ndays + di];
            cell[0] += 1;
            cell[1] += purchased as i64;
            cell[2] += revenue;
            for c in &cls[lo..hi] {
                let (w, b) = ((c.camp >> 6) as usize, 1u64 << (c.camp & 63));
                if self.bits[w] & b == 0 {
                    self.bits[w] |= b;
                    self.touched.push(c.camp);
                }
            }
            for &c in &self.touched {
                self.camp[c as usize * self.ndays + di][3] += 1;
                self.bits[(c >> 6) as usize] = 0;
            }
            self.touched.clear();
            k = m;
        }
        self.flush(person, cur_day, day_sessions, day_events, day_revenue);
    }
}

fn run() -> Result<()> {
    let t0 = Instant::now();
    let lap = |what: &str| eprintln!("[{:8.3}s] {what}", t0.elapsed().as_secs_f64());
    let data = env_path("DATA_DIR", "/data");
    let out = env_path("OUTPUT_DIR", "/output");
    let ncpu: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));

    let ev_maps = csv::map_files(&csv::list_csv(&data.join("events")))?;
    let ck_maps = csv::map_files(&csv::list_csv(&data.join("clicks")))?;
    let ln_maps = csv::map_files(&csv::list_csv(&data.join("links")))?;
    for m in ln_maps.iter().chain(&ev_maps).chain(&ck_maps) {
        let _ = m.advise(Advice::WillNeed);
    }
    let bytes = |maps: &[Mmap]| maps.iter().map(|m| m.len()).sum::<usize>();
    let (ev_bytes, ck_bytes) = (bytes(&ev_maps), bytes(&ck_maps));
    let lk = load_lookups(&data)?;
    lap(&format!("lookups: ncpu={ncpu} events {} files {} MB, clicks {} files {} MB, links {} files",
        ev_maps.len(), ev_bytes >> 20, ck_maps.len(), ck_bytes >> 20, ln_maps.len()));

    // Links -> persons.
    let ln_chunks = csv::chunks(&ln_maps, CHUNK);
    let pairs = par::parallel_for(ncpu, ln_chunks.len(), Vec::new, |v: &mut Vec<(u128, u128)>, t| {
        let c = &ln_chunks[t];
        csv::for_each_line(&ln_maps[c.file][c.start..c.end], |line| {
            if let Some([a, b]) = csv::split_fields::<2>(line) {
                if !a.is_empty() && !b.is_empty() {
                    v.push((pack(a), pack(b)));
                }
            }
        });
    });
    let persons = build_persons(pairs);
    lap(&format!("links: {} linked ids", persons.person_of.len()));

    // Events: parse + validate every copy, partitioned by event_id.
    let ev_chunks = csv::chunks(&ev_maps, CHUNK);
    let cap = ev_bytes / 70 / (ncpu * P) * 5 / 4 + 16;
    let raw = par::parallel_for(ncpu, ev_chunks.len(), || new_bufs::<RawEv>(cap), |bufs, t| {
        let c = &ev_chunks[t];
        csv::for_each_line(&ev_maps[c.file][c.start..c.end], |line| {
            let Some(f) = csv::split_fields::<7>(line) else { return };
            if f[0].is_empty() {
                return;
            }
            let ek = pack(f[0]);
            let ingest = parse_ts(f[3]).unwrap_or(i64::MAX);
            let mut r = RawEv { ek, user: 0, ingest, ts: 0, price: 0, cat: 0, kind: 0 };
            validate_event(&mut r, &f, &lk);
            bufs[part_of(ek)].push(r);
        });
    });
    let raw_parts = transpose(raw);
    lap("events parsed");

    // Dedup per event_id partition: keep the min-ingest copy, then drop it if invalid.
    let ev_out = par::parallel_for(ncpu, P, || new_bufs::<Ev>(cap), |out, p| {
        let pieces = take(&raw_parts[p]);
        let n: usize = pieces.iter().map(Vec::len).sum();
        let mut best: KeyMap<u64> = key_map(n);
        for (pi, piece) in pieces.iter().enumerate() {
            for (i, r) in piece.iter().enumerate() {
                let code = ((pi as u64) << 32) | i as u64;
                match best.entry(r.ek) {
                    Entry::Vacant(e) => {
                        e.insert(code);
                    }
                    Entry::Occupied(mut e) => {
                        let c = *e.get();
                        if r.ingest < pieces[(c >> 32) as usize][(c & 0xFFFF_FFFF) as usize].ingest {
                            e.insert(code);
                        }
                    }
                }
            }
        }
        for &c in best.values() {
            let r = &pieces[(c >> 32) as usize][(c & 0xFFFF_FFFF) as usize];
            if r.kind == 0 {
                continue;
            }
            let person = persons.resolve(r.user);
            out[part_of(person)].push(Ev { person, ts: r.ts, price: r.price, cat: r.cat, purchase: r.kind == 2 });
        }
    });
    drop(raw_parts);
    let ev_parts = transpose(ev_out);
    lap("events deduped");

    // Clicks: parse + validate, partitioned by person. (Copies are identical, so no dedup.)
    let ck_chunks = csv::chunks(&ck_maps, CHUNK);
    let ck_cap = ck_bytes / 45 / (ncpu * P) * 5 / 4 + 16;
    let click_window_start = lk.ws - WEEK_MS;
    let clicks = par::parallel_for(ncpu, ck_chunks.len(), || new_bufs::<Click>(ck_cap), |bufs, t| {
        let c = &ck_chunks[t];
        csv::for_each_line(&ck_maps[c.file][c.start..c.end], |line| {
            let Some([cid, uid, camp, tsf]) = csv::split_fields::<4>(line) else { return };
            if cid.is_empty() || uid.is_empty() {
                return;
            }
            let Some(t) = parse_ts(tsf) else { return };
            if t < click_window_start || t >= lk.we {
                return;
            }
            let Some(ci) = lk.campaign(camp) else { return };
            let person = persons.resolve(pack(uid));
            bufs[part_of(person)].push(Click { person, ck: pack(cid), ts: t, camp: ci });
        });
    });
    let ck_parts = transpose(clicks);
    lap("clicks parsed");

    // Sessions, credit and assists per person partition.
    let n_camp = lk.camp_ids.len();
    let organic = n_camp;
    let d0 = lk.ws.div_euclid(DAY_MS);
    let ndays = ((lk.we - 1).div_euclid(DAY_MS) - d0 + 1).max(1) as usize;
    let ncat = lk.cats.len();
    let accs = par::parallel_for(ncpu, P, || Acc::new(n_camp + 1, ndays, ncat), |acc, p| {
        let mut evs = concat(take(&ev_parts[p]));
        let mut cls = concat(take(&ck_parts[p]));
        evs.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)));
        cls.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)).then_with(|| key_cmp(b.ck, a.ck)));
        let (mut i, mut ci) = (0usize, 0usize);
        while i < evs.len() {
            let person = evs[i].person;
            let mut j = i + 1;
            while j < evs.len() && evs[j].person == person {
                j += 1;
            }
            while ci < cls.len() && cls[ci].person < person {
                ci += 1;
            }
            let c0 = ci;
            while ci < cls.len() && cls[ci].person == person {
                ci += 1;
            }
            acc.person(&evs[i..j], &cls[c0..ci], d0, organic);
            i = j;
        }
    });
    lap("sessions");

    // Merge and write.
    let mut camp = vec![[0i64; 4]; (n_camp + 1) * ndays];
    let mut rows: Vec<URow> = Vec::with_capacity(accs.iter().map(|a| a.rows.len()).sum());
    for acc in accs {
        for (total, cell) in camp.iter_mut().zip(&acc.camp) {
            for x in 0..4 {
                total[x] += cell[x];
            }
        }
        rows.extend(acc.rows);
    }
    let cat_names: Vec<ByteArray> = lk.cats.iter().map(|c| ByteArray::from(c.clone())).collect();
    let per = rows.len().div_ceil(ncpu).max(1);
    let parts: Vec<&[URow]> = if rows.is_empty() { vec![&rows[..]] } else { rows.chunks(per).collect() };
    let errors = par::parallel_for(ncpu, parts.len(), || None, |err: &mut Option<String>, i| {
        let part = parts[i];
        let mut t = UserDaily::default();
        for r in part {
            t.user_id.push(ByteArray::from(keys::to_vec(r.person)));
            t.day.push(r.day);
            t.sessions.push(r.sessions as i64);
            t.events.push(r.events as i64);
            t.revenue_cents.push(r.revenue);
            t.top_category.push((r.top >= 0).then(|| cat_names[r.top as usize].clone()));
        }
        if let Err(e) = output::write_user_daily(&out, &format!("part-{i:03}.parquet"), &t) {
            *err = Some(e.to_string());
        }
    });
    if let Some(e) = errors.into_iter().flatten().next() {
        return Err(e.into());
    }

    let mut t = CampaignDaily::default();
    let organic_name = ByteArray::from(b"organic".to_vec());
    for c in 0..=n_camp {
        let (id, channel) = if c == organic {
            (organic_name.clone(), organic_name.clone())
        } else {
            (ByteArray::from(lk.camp_ids[c].clone()), ByteArray::from(lk.channels[c].clone()))
        };
        for d in 0..ndays {
            let cell = camp[c * ndays + d];
            if cell[0] > 0 || cell[3] > 0 {
                t.campaign_id.push(id.clone());
                t.channel.push(channel.clone());
                t.day.push((d0 + d as i64) as i32);
                t.sessions.push(cell[0]);
                t.purchasing_sessions.push(cell[1]);
                t.revenue_cents.push(cell[2]);
                t.assisted_sessions.push(cell[3]);
            }
        }
    }
    output::write_campaign_daily(&out, "part-000.parquet", &t)?;
    lap(&format!("wrote user_daily {} rows, campaign_daily {} rows", rows.len(), t.day.len()));
    Ok(())
}
