//! Loader for Wikimedia hourly pageview dumps.
//!
//! Each line is `domain_code page_title count_views total_response_size`.
//! Group keys are 64-bit hashes of the grouping columns. That keeps the
//! engine on fixed-width keys; `names` re-reads the files to recover the
//! strings behind the winning hashes and reports any hash that more than
//! one distinct string maps to, so a collision in the answer cannot go
//! unnoticed.

use crate::hash::hash_bytes;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    /// `(domain_code, page_title)`: one article on one wiki.
    Page,
    /// `page_title` alone: the same title summed across every wiki/project.
    Title,
}

fn open(path: &Path) -> std::io::Result<Box<dyn Read + Send>> {
    if path.extension().is_some_and(|e| e == "gz") {
        let child = Command::new("gzip").arg("-dc").arg(path).stdout(Stdio::piped()).spawn()?;
        Ok(Box::new(child.stdout.unwrap()))
    } else {
        Ok(Box::new(std::fs::File::open(path)?))
    }
}

fn for_each_line(path: &Path, mut f: impl FnMut(&[u8], &[u8], i64)) -> std::io::Result<()> {
    let mut r = BufReader::with_capacity(1 << 20, open(path)?);
    let mut line = Vec::with_capacity(512);
    loop {
        line.clear();
        if r.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let mut it = line.trim_ascii_end().split(|&b| b == b' ');
        let (Some(domain), Some(title), Some(views)) = (it.next(), it.next(), it.next()) else { continue };
        let Some(views) = std::str::from_utf8(views).ok().and_then(|s| s.parse::<i64>().ok()) else { continue };
        f(domain, title, views);
    }
    Ok(())
}

fn key(by: GroupBy, domain: &[u8], title: &[u8]) -> u64 {
    match by {
        GroupBy::Page => hash_bytes(&[domain, title]),
        GroupBy::Title => hash_bytes(&[title]),
    }
}

/// Load files in parallel (one thread per file); rows keep file order.
pub fn load(paths: &[PathBuf], by: GroupBy) -> std::io::Result<(Vec<u64>, Vec<i64>)> {
    let parts: Vec<std::io::Result<(Vec<u64>, Vec<i64>)>> = std::thread::scope(|s| {
        let hs: Vec<_> = paths
            .iter()
            .map(|p| {
                s.spawn(move || {
                    let mut ks = Vec::with_capacity(6_000_000);
                    let mut vs = Vec::with_capacity(6_000_000);
                    for_each_line(p, |d, t, v| {
                        ks.push(key(by, d, t));
                        vs.push(v);
                    })?;
                    Ok((ks, vs))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut keys = Vec::new();
    let mut vals = Vec::new();
    for p in parts {
        let (k, v) = p?;
        keys.extend_from_slice(&k);
        vals.extend_from_slice(&v);
    }
    Ok((keys, vals))
}

/// Recover display names for `wanted` keys. Returns key -> every distinct
/// string that hashed to it (more than one would be a 64-bit collision).
pub fn names(paths: &[PathBuf], by: GroupBy, wanted: &[u64]) -> std::io::Result<HashMap<u64, Vec<String>>> {
    let wanted: HashSet<u64> = wanted.iter().copied().collect();
    let mut out: HashMap<u64, HashSet<String>> = HashMap::new();
    for p in paths {
        for_each_line(p, |d, t, _| {
            let k = key(by, d, t);
            if wanted.contains(&k) {
                let name = match by {
                    GroupBy::Page => format!("{} {}", String::from_utf8_lossy(d), String::from_utf8_lossy(t)),
                    GroupBy::Title => String::from_utf8_lossy(t).into_owned(),
                };
                out.entry(k).or_default().insert(name);
            }
        })?;
    }
    Ok(out.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect())
}
