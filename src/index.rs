// SPDX-License-Identifier: GPL-3.0-only

//! The archive index: which files of an archive are the same inode.
//!
//! `<root>/.nudl-index.gz` overlays the `.ver` files. For each package (a version
//! directory) it lists one number per `.ver` entry, in the `.ver`'s order: the file's
//! *group*, or 0 if the file is not there. Files in the same group are hardlinks of
//! one another. A group is a device, an inode and the inode's link count.
//!
//! The point is for a tool that wants to relate packages to one another (which share
//! storage, what deleting one would free) to need no `stat` of the archive, which is
//! slow on a big one. `nudl sync` already stats the files it writes and links, so it
//! keeps the index current for the versions it finishes. `nudl index` makes it in one
//! pass, and `nudl relink` refreshes it after changing paths.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufReader, BufWriter, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};

use crate::{link, version::VersionInfo};

pub const FILE_NAME: &str = ".nudl-index.gz";
const FORMAT: u32 = 1;
/// Depth of `<brand>/<region>/<model id>/<version>` below the archive root.
const VERSION_DEPTH: usize = 4;

/// (device, inode, link count) of a file that is present, or `None` if it is not.
pub type Stat = Option<(u64, u64, u64)>;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Index {
    pub format: u32,
    /// When the index was last written, RFC 3339.
    pub written: String,
    /// Storage identities as `[device, inode, link count]`. A file's group is its
    /// 1-based position in this list.
    pub groups: Vec<[u64; 3]>,
    /// By `<brand>/<region>/<model id>/<version>`.
    pub packages: BTreeMap<String, IndexedPackage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedPackage {
    pub complete: bool,
    /// One group per `.ver` entry, in order; 0 where the file is missing.
    pub files: Vec<u32>,
}

/// Look a file up: present only if it is a regular file of the size the `.ver` lists,
/// which is also how nudl decides a file is done.
pub fn stat_file(path: &Path, size: u64) -> Stat {
    let m = fs::symlink_metadata(path).ok()?;
    (m.is_file() && m.len() == size).then(|| (m.dev(), m.ino(), m.nlink()))
}

pub fn path(root: &Path) -> PathBuf {
    root.join(FILE_NAME)
}

impl Index {
    /// Read the index, or `None` if there is none or it can't be used.
    pub fn load(root: &Path) -> Option<Self> {
        let f = fs::File::open(path(root)).ok()?;
        let index: Self = serde_json::from_reader(BufReader::new(GzDecoder::new(f))).ok()?;
        (index.format == FORMAT).then_some(index)
    }

    /// Write the index through a temporary file, so a reader never sees half of one.
    pub fn save(mut self, root: &Path) -> Result<()> {
        self.compact();
        self.format = FORMAT;
        self.written = jiff::Timestamp::now().to_string();

        let target = path(root);
        let tmp = root.join(format!("{FILE_NAME}.tmp"));
        {
            let file = fs::File::create(&tmp).with_context(|| format!("Failed to create {tmp:?}"))?;
            let mut gz = GzEncoder::new(BufWriter::new(file), Compression::default());
            serde_json::to_writer(&mut gz, &self)?;
            gz.finish()?.flush()?;
        }
        fs::rename(&tmp, &target).with_context(|| format!("Failed to rename {tmp:?}"))
    }

    /// Group of each (device, inode).
    pub fn lookup(&self) -> HashMap<(u64, u64), u32> {
        self.groups
            .iter()
            .enumerate()
            .map(|(i, g)| ((g[0], g[1]), i as u32 + 1))
            .collect()
    }

    /// Set a package's files, joining each to the group of its inode or starting one.
    /// `lookup` is `self.lookup()` and is kept up to date across calls.
    pub fn record(
        &mut self,
        lookup: &mut HashMap<(u64, u64), u32>,
        id: String,
        complete: bool,
        stats: &[Stat],
    ) {
        let mut files = Vec::with_capacity(stats.len());
        for s in stats {
            files.push(match *s {
                None => 0,
                Some((dev, ino, nlink)) => {
                    let gid = *lookup.entry((dev, ino)).or_insert_with(|| {
                        self.groups.push([dev, ino, nlink]);
                        self.groups.len() as u32
                    });
                    // Freshest count we have: this inode was just looked at.
                    self.groups[gid as usize - 1][2] = nlink;
                    gid
                }
            });
        }
        self.packages.insert(id, IndexedPackage { complete, files });
    }

    /// Drop groups no package refers to any more and renumber the rest.
    fn compact(&mut self) {
        let mut used = vec![false; self.groups.len() + 1];
        for p in self.packages.values() {
            for &g in &p.files {
                used[g as usize] = true;
            }
        }
        let mut renumber = vec![0u32; self.groups.len() + 1];
        let mut kept = Vec::new();
        for (i, g) in self.groups.iter().enumerate() {
            if used[i + 1] {
                kept.push(*g);
                renumber[i + 1] = kept.len() as u32;
            }
        }
        for p in self.packages.values_mut() {
            for g in &mut p.files {
                *g = renumber[*g as usize];
            }
        }
        self.groups = kept;
    }
}

/// A package as the index sees it: where it is and the files its `.ver` lists.
pub struct Listing {
    pub id: String,
    pub dir: PathBuf,
    pub complete: bool,
    /// (path below `dir`, size), in `.ver` order.
    pub files: Vec<(String, u64)>,
}

/// Stat every file of every package, `threads` at a time. Each stat can wait on a slow
/// disk, so the work is spread by package.
pub fn stat_listings(listings: &[Listing], threads: usize) -> Vec<Vec<Stat>> {
    let results: Vec<Mutex<Vec<Stat>>> = listings.iter().map(|_| Mutex::new(Vec::new())).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(l) = listings.get(i) else { break };
                    let stats = l.files.iter().map(|(rel, size)| stat_file(&l.dir.join(rel), *size)).collect();
                    *results[i].lock().unwrap() = stats;
                }
            });
        }
    });

    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// `<brand>/<region>/<model id>/<version>` for a version directory below `root`.
pub fn package_id(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .unwrap_or(dir)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The version directories of an archive, read from their `.ver` files.
pub fn read_listings(root: &Path) -> Result<Vec<Listing>> {
    let mut vers = link::find_ver_files(&[root.to_path_buf()], Some(VERSION_DEPTH))
        .with_context(|| format!("Failed to scan {root:?}"))?;
    vers.sort();

    let mut out = Vec::with_capacity(vers.len());
    for ver in vers {
        let Some(dir) = ver.parent() else { continue };
        let info = match fs::read_to_string(&ver)
            .map_err(anyhow::Error::from)
            .and_then(|s| s.parse::<VersionInfo>().map_err(anyhow::Error::from))
        {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!("Skipping {ver:?}: {e}");
                continue;
            }
        };
        out.push(Listing {
            id: package_id(root, dir),
            dir: dir.to_path_buf(),
            complete: dir.join(".complete").exists(),
            files: info.entries.iter().map(|e| (e.path().into_owned(), e.size)).collect(),
        });
    }

    Ok(out)
}

/// Index the whole archive from scratch: one stat per listed file.
pub fn build(root: &Path, threads: usize) -> Result<Index> {
    let listings = read_listings(root)?;
    let stats = stat_listings(&listings, threads);

    let mut index = Index::default();
    let mut lookup = HashMap::new();
    for (l, s) in listings.iter().zip(&stats) {
        index.record(&mut lookup, l.id.clone(), l.complete, s);
    }

    Ok(index)
}

/// Add finished versions to the index, creating it if there is none. Files are
/// stat'ed only for these versions, so it costs what they hold.
pub fn record_finished(root: &Path, finished: Vec<Listing>, threads: usize) -> Result<()> {
    let stats = stat_listings(&finished, threads);

    let mut index = Index::load(root).unwrap_or_default();
    let mut lookup = index.lookup();
    for (l, s) in finished.iter().zip(&stats) {
        index.record(&mut lookup, l.id.clone(), l.complete, s);
    }

    index.save(root)
}

/// `nudl index`: index the archive from scratch.
pub fn run(cli: &crate::cli::IndexCli) -> Result<()> {
    let root = &cli.root;
    anyhow::ensure!(root.is_dir(), "Archive directory does not exist: {root:?}");
    // Share the sync lock: the archive must not change under the pass.
    let _lock = crate::sync::lock(root)?;

    let index = build(root, cli.threads)?;
    let (packages, groups) = (index.packages.len(), index.groups.len());
    let files: usize = index.packages.values().map(|p| p.files.len()).sum();
    index.save(root)?;

    let bytes = fs::metadata(path(root)).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "index: {packages} versions, {files} files in {groups} storage groups, written to {:?} ({:.1} MB)",
        path(root),
        bytes as f64 / 1e6,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudl-index-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn version(root: &Path, name: &str, files: &[(&str, &[u8])]) {
        let dir = root.join("hyundai/CA/CAR").join(name);
        fs::create_dir_all(&dir).unwrap();
        let mut ver = format!("+|1|{name}|HM|CAR|1|1\n");
        for (rel, data) in files {
            fs::write(dir.join(rel), data).unwrap();
            let crc = crc32fast::hash(data) as i32;
            ver.push_str(&format!("CAR|{rel}|1|{crc}|{}|1\n", data.len()));
        }
        fs::write(dir.join("CAR.ver"), ver).unwrap();
        fs::write(dir.join(".complete"), b"").unwrap();
    }

    #[test]
    fn groups_follow_hardlinks() {
        let root = temp();
        version(&root, "v1", &[("a.bin", b"aaaa"), ("b.bin", b"bbbb")]);
        version(&root, "v2", &[("b.bin", b"bbbb")]);
        // v2's copy of b.bin becomes a hardlink of v1's.
        let (b1, b2) = (root.join("hyundai/CA/CAR/v1/b.bin"), root.join("hyundai/CA/CAR/v2/b.bin"));
        fs::remove_file(&b2).unwrap();
        fs::hard_link(&b1, &b2).unwrap();

        let index = build(&root, 4).unwrap();
        let v1 = index.packages["hyundai/CA/CAR/v1"].files.clone();
        let v2 = index.packages["hyundai/CA/CAR/v2"].files.clone();
        assert_ne!(v1[0], v1[1], "a.bin and b.bin are different inodes");
        assert_eq!(v1[1], v2[0], "b.bin is one inode in both packages");
        assert_eq!(index.groups.len(), 2);
        assert_eq!(index.groups[(v1[1] - 1) as usize][2], 2, "two links to b.bin");
        assert!(index.packages["hyundai/CA/CAR/v1"].complete);

        // Round trip through the file.
        index.save(&root).unwrap();
        let back = Index::load(&root).unwrap();
        assert_eq!(back.packages["hyundai/CA/CAR/v2"].files, v2);
        assert!(!back.written.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_and_short_files_are_group_zero() {
        let root = temp();
        version(&root, "v1", &[("a.bin", b"aaaa"), ("b.bin", b"bbbb"), ("c.bin", b"cc")]);
        let dir = root.join("hyundai/CA/CAR/v1");
        fs::remove_file(dir.join("a.bin")).unwrap();
        fs::write(dir.join("b.bin"), b"short").unwrap();

        let index = build(&root, 2).unwrap();
        let files = &index.packages["hyundai/CA/CAR/v1"].files;
        assert_eq!(files[0], 0, "missing");
        assert_eq!(files[1], 0, "wrong size");
        assert_ne!(files[2], 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finished_versions_join_existing_groups_and_unused_groups_go() {
        let root = temp();
        version(&root, "v1", &[("a.bin", b"aaaa")]);
        build(&root, 2).unwrap().save(&root).unwrap();

        // A new version links to v1's file; only it is looked at.
        version(&root, "v2", &[("a.bin", b"aaaa")]);
        let (a1, a2) = (root.join("hyundai/CA/CAR/v1/a.bin"), root.join("hyundai/CA/CAR/v2/a.bin"));
        fs::remove_file(&a2).unwrap();
        fs::hard_link(&a1, &a2).unwrap();
        let listing = read_listings(&root).unwrap().into_iter().find(|l| l.id.ends_with("v2")).unwrap();
        record_finished(&root, vec![listing], 2).unwrap();

        let index = Index::load(&root).unwrap();
        assert_eq!(index.packages.len(), 2);
        assert_eq!(index.groups.len(), 1, "both versions share one group");
        assert_eq!(index.groups[0][2], 2);

        // Dropping a package leaves groups only it used unreferenced, and they go.
        let mut index = index;
        index.packages.remove("hyundai/CA/CAR/v1");
        index.packages.remove("hyundai/CA/CAR/v2");
        index.save(&root).unwrap();
        assert!(Index::load(&root).unwrap().groups.is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
