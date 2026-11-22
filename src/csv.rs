//! Input files and the rules.md CSV dialect.
//!
//! Lines end at `\n` only (`\r` is data). The first line of every file is the header.
//! A field is either wrapped in quotes with no quote inside (the quotes are removed) or
//! has no quote at all; anything else, a wrong field count or an empty line makes the
//! line malformed. An empty field (bare or `""`) is null, represented as an empty slice.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use memchr::{memchr, memchr_iter};
use memmap2::Mmap;

/// Every `*.csv` file directly in `dir`, sorted. A missing dir has no files.
pub fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "csv") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Memory-maps every non-empty file (an empty file has no lines at all).
pub fn map_files(paths: &[PathBuf]) -> std::io::Result<Vec<Mmap>> {
    let mut maps = Vec::with_capacity(paths.len());
    for path in paths {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            continue;
        }
        // SAFETY: /data is read-only and nothing modifies the files while we run.
        maps.push(unsafe { Mmap::map(&file)? });
    }
    Ok(maps)
}

pub struct Chunk {
    pub file: usize,
    pub start: usize,
    pub end: usize,
}

/// Splits every file's data lines (after the header) into chunks of about `target`
/// bytes that start and end on line boundaries. Largest chunks first.
pub fn chunks(maps: &[Mmap], target: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (file, map) in maps.iter().enumerate() {
        let data: &[u8] = map;
        let Some(header_end) = memchr(b'\n', data) else {
            continue; // header only, no data lines
        };
        let mut start = header_end + 1;
        while start < data.len() {
            let mut end = (start + target).min(data.len());
            if end < data.len() {
                end = match memchr(b'\n', &data[end..]) {
                    Some(at) => end + at + 1,
                    None => data.len(),
                };
            }
            out.push(Chunk { file, start, end });
            start = end;
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.end - c.start));
    out
}

/// Calls `f` for every line of `data` (without its `\n`), including a last line with
/// no `\n`. Empty lines are passed too (split_fields rejects them).
#[inline]
pub fn for_each_line(data: &[u8], mut f: impl FnMut(&[u8])) {
    let mut start = 0;
    for at in memchr_iter(b'\n', data) {
        f(&data[start..at]);
        start = at + 1;
    }
    if start < data.len() {
        f(&data[start..]);
    }
}

/// Splits a line into exactly `N` fields with quotes removed, or None if malformed.
#[inline]
pub fn split_fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut out: [&[u8]; N] = [&[]; N];
    let mut i = 0;
    let mut start = 0;
    for at in memchr_iter(b',', line) {
        if i + 1 >= N {
            return None; // too many fields
        }
        out[i] = &line[start..at];
        i += 1;
        start = at + 1;
    }
    if i + 1 != N {
        return None; // too few fields (an empty line has one empty field)
    }
    out[N - 1] = &line[start..];
    if memchr(b'"', line).is_some() {
        for field in out.iter_mut() {
            if memchr(b'"', field).is_some() {
                let n = field.len();
                if n >= 2 && field[0] == b'"' && field[n - 1] == b'"' && memchr(b'"', &field[1..n - 1]).is_none() {
                    *field = &field[1..n - 1];
                } else {
                    return None;
                }
            }
        }
    }
    Some(out)
}

/// Line/field state of `for_each_record`, advanced one delimiter (`,` `\n` `"`) at a time.
struct Scan<'a, const N: usize> {
    data: &'a [u8],
    fields: [&'a [u8]; N],
    nf: usize,      // index of the current field in the line
    fs: usize,      // start of the current field
    ok: bool,       // line still well-formed
    quotes: u32,    // quotes in the current field
    q_first: usize, // position of its first quote
    q_last: usize,  // position of its last quote
}

impl<'a, const N: usize> Scan<'a, N> {
    #[inline(always)]
    fn end_field(&mut self, pos: usize) {
        if self.ok {
            if self.nf >= N {
                self.ok = false;
            } else if self.quotes == 0 {
                self.fields[self.nf] = &self.data[self.fs..pos];
            } else if self.quotes == 2 && self.q_first == self.fs && self.q_last + 1 == pos && pos - self.fs >= 2 {
                self.fields[self.nf] = &self.data[self.fs + 1..pos - 1];
            } else {
                self.ok = false;
            }
        }
        self.nf += 1;
        self.fs = pos + 1;
        self.quotes = 0;
    }

    /// `pos` holds a `,`, `\n` or `"`.
    #[inline(always)]
    fn delimiter(&mut self, pos: usize, f: &mut impl FnMut(&[&'a [u8]; N])) {
        match self.data[pos] {
            b'"' => {
                if self.quotes == 0 {
                    self.q_first = pos;
                }
                self.q_last = pos;
                self.quotes += 1;
            }
            b',' => self.end_field(pos),
            _ => {
                self.end_field(pos);
                if self.ok && self.nf == N {
                    f(&self.fields);
                }
                self.nf = 0;
                self.ok = true;
            }
        }
    }
}

/// Bit i set iff `p[i]` is `,`, `\n` or `"`, for the 32 bytes at `p`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
fn delimiter_mask(block: &[u8; 32]) -> u32 {
    use std::arch::x86_64::*;
    // SAFETY: AVX2 is enabled at compile time (target-cpu=x86-64-v3); the load reads
    // exactly the 32 bytes of `block`.
    unsafe {
        let v = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
        let c = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b',' as i8));
        let l = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
        let q = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8));
        _mm256_movemask_epi8(_mm256_or_si256(_mm256_or_si256(c, l), q)) as u32
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
fn delimiter_mask(block: &[u8; 32]) -> u32 {
    let mut m = 0u32;
    for (i, &b) in block.iter().enumerate() {
        if b == b',' || b == b'\n' || b == b'"' {
            m |= 1 << i;
        }
    }
    m
}

/// Calls `f` with the `N` unquoted fields of every well-formed line of `data` (a chunk of
/// whole lines, header already skipped). One pass: a 32-byte SIMD compare finds every
/// `,` `\n` `"`, and the set bits are walked in order (dense matches, so no per-match
/// re-search). Same dialect as `for_each_line` + `split_fields`: lines end at `\n` only;
/// a field is either `"..."` with no inner quote (unwrapped) or has no quote; a wrong
/// field count, an empty line or a misplaced quote drops the line.
#[inline]
pub fn for_each_record<'a, const N: usize>(data: &'a [u8], mut f: impl FnMut(&[&'a [u8]; N])) {
    let mut st = Scan::<N> { data, fields: [&[]; N], nf: 0, fs: 0, ok: true, quotes: 0, q_first: 0, q_last: 0 };
    let mut base = 0;
    while let Some(block) = data.get(base..base + 32) {
        let mut m = delimiter_mask(block.try_into().expect("32-byte block"));
        while m != 0 {
            st.delimiter(base + m.trailing_zeros() as usize, &mut f);
            m &= m - 1;
        }
        base += 32;
    }
    for pos in base..data.len() {
        if matches!(data[pos], b',' | b'\n' | b'"') {
            st.delimiter(pos, &mut f);
        }
    }
    if st.fs < data.len() || st.nf > 0 {
        st.end_field(data.len());
        if st.ok && st.nf == N {
            f(&st.fields);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect() {
        assert_eq!(split_fields::<2>(b"a,b"), Some([&b"a"[..], &b"b"[..]]));
        assert_eq!(split_fields::<2>(b"\"a\",\"\""), Some([&b"a"[..], &b""[..]]));
        assert_eq!(split_fields::<2>(b""), None);
        assert_eq!(split_fields::<2>(b"a"), None);
        assert_eq!(split_fields::<2>(b"a,b,c"), None);
        assert_eq!(split_fields::<2>(b"a,b\""), None);
        assert_eq!(split_fields::<2>(b"a,\""), None);
        assert_eq!(split_fields::<2>(b"a,\"b\"c"), None);
        assert_eq!(split_fields::<2>(b"a,\"b\"\"c\""), None);
        assert_eq!(split_fields::<2>(b" a ,b\r"), Some([&b" a "[..], &b"b\r"[..]]));
    }

    fn same<const N: usize>(data: &[u8]) {
        let mut a: Vec<[Vec<u8>; N]> = Vec::new();
        for_each_line(data, |l| {
            if let Some(f) = split_fields::<N>(l) {
                a.push(f.map(|x| x.to_vec()));
            }
        });
        let mut b: Vec<[Vec<u8>; N]> = Vec::new();
        for_each_record::<N>(data, |f| b.push((*f).map(|x| x.to_vec())));
        assert_eq!(a, b, "{:?}", String::from_utf8_lossy(data));
    }

    #[test]
    fn scanner_matches_split_fields() {
        let alphabet = b"ab,\"\n \r";
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200_000 {
            let len = (next() % 200) as usize;
            let data: Vec<u8> = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            same::<2>(&data);
            same::<4>(&data);
            same::<7>(&data);
        }
        for d in [&b""[..], b"\n", b"a,b", b"a,b\n", b"a,b\n\n", b"\"a\",\"\"", b"a,\"", b",", b",\n,"] {
            same::<2>(d);
        }
    }
}
