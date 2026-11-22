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
}
