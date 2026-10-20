// SPDX-License-Identifier: GPL-3.0-only

//! `nudl relink`: turn duplicate files in an archive into hardlinks.
//!
//! Two files with the same CRC32 and size (taken from the `.ver` files) are the same
//! file. Where a group of them lives in separate inodes, all but one are replaced by
//! a hardlink to the best-linked copy. Each replacement links to a temporary name and
//! renames it over the original, so a path never disappears.

use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use crate::{cli::RelinkCli, link, sync, version::VersionInfo};

type Key = (u32, u64);

#[derive(Default)]
struct Stats {
    groups_done: AtomicUsize,
    /// Files whose inode differs from their group's chosen copy.
    to_relink: AtomicU64,
    /// Bytes and inodes that go away once every link to them is replaced.
    freed_bytes: AtomicU64,
    freed_inodes: AtomicU64,
    relinked: AtomicU64,
    already_linked: AtomicU64,
    other_filesystem: AtomicU64,
    mismatched: AtomicU64,
    errors: AtomicU64,
}

fn same_contents(a: &Path, b: &Path) -> Result<bool> {
    let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);

    loop {
        let na = fa.read(&mut ba)?;
        if na == 0 {
            return Ok(fb.read(&mut bb)? == 0);
        }
        fb.read_exact(&mut bb[..na])?;
        if ba[..na] != bb[..na] {
            return Ok(false);
        }
    }
}

fn relink_one(canonical: &Path, path: &Path, verify: bool, stats: &Stats) {
    if verify {
        match same_contents(canonical, path) {
            Ok(true) => {}
            Ok(false) => {
                warn!("Contents differ despite equal CRC32 and size: {path:?}");
                stats.mismatched.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                warn!("Failed to compare {path:?}: {e}");
                stats.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    let tmp = PathBuf::from(format!("{}.relink.tmp", path.display()));
    let _ = fs::remove_file(&tmp);
    let result = fs::hard_link(canonical, &tmp).and_then(|()| fs::rename(&tmp, path));
    match result {
        Ok(()) => {
            stats.relinked.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            warn!("Failed to relink {path:?}: {e}");
            let _ = fs::remove_file(&tmp);
            stats.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn process_group(key: Key, paths: Vec<PathBuf>, cli: &RelinkCli, stats: &Stats) {
    let size = key.1;

    // (path, device, inode, link count) of each file that is really there.
    let infos: Vec<(PathBuf, u64, u64, u64)> = paths
        .into_iter()
        .filter_map(|p| {
            let m = fs::metadata(&p).ok()?;
            (m.is_file() && m.len() == size).then(|| (p, m.dev(), m.ino(), m.nlink()))
        })
        .collect();
    if infos.len() < 2 {
        return;
    }

    // Keep the copy that already has the most links.
    let canon = infos
        .iter()
        .enumerate()
        .max_by_key(|(i, (_, _, _, nlink))| (*nlink, std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
        .expect("not empty");
    let (cpath, cdev, cino, _) = infos[canon].clone();

    // How many of this group's paths point at each other inode, and how many links it
    // has in all. If they match, replacing them frees the inode; otherwise something
    // else (such as the old `.cas` store) still holds it.
    let mut by_inode: HashMap<u64, (u64, u64)> = HashMap::new();
    for (_, dev, ino, nlink) in &infos {
        if *dev == cdev && *ino != cino {
            by_inode.entry(*ino).or_insert((0, *nlink)).0 += 1;
        }
    }
    for (count, nlink) in by_inode.values() {
        if count == nlink {
            stats.freed_bytes.fetch_add(size, Ordering::Relaxed);
            stats.freed_inodes.fetch_add(1, Ordering::Relaxed);
        }
    }

    for (path, dev, ino, _) in &infos {
        if *dev != cdev {
            stats.other_filesystem.fetch_add(1, Ordering::Relaxed);
        } else if *ino == cino {
            // The chosen copy itself isn't "already linked" to anything.
            if path != &cpath {
                stats.already_linked.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            stats.to_relink.fetch_add(1, Ordering::Relaxed);
            if cli.apply {
                relink_one(&cpath, path, cli.verify, stats);
            }
        }
    }
}

pub fn run(cli: &RelinkCli) -> Result<()> {
    let root = cli.root.clone();
    if !root.is_dir() {
        bail!("Archive directory does not exist: {root:?}");
    }
    // Relinking an archive while a sync writes it would race; share the sync lock.
    let _lock = sync::lock(&root)?;

    let vers = link::find_ver_files(&[root.clone()], Some(4))
        .with_context(|| format!("Failed to scan {root:?}"))?;

    // Group every file of every finished version by (CRC32, size).
    let mut groups: HashMap<Key, Vec<PathBuf>> = HashMap::new();
    let (mut versions, mut files) = (0usize, 0u64);
    for ver in &vers {
        let Some(dir) = ver.parent() else { continue };
        if !dir.join(".complete").exists() {
            continue;
        }
        let info = match fs::read_to_string(ver)
            .map_err(anyhow::Error::from)
            .and_then(|s| s.parse::<VersionInfo>().map_err(anyhow::Error::from))
        {
            Ok(i) => i,
            Err(e) => {
                debug!("Skipping {ver:?}: {e}");
                continue;
            }
        };
        versions += 1;
        for entry in info.entries {
            if entry.size >= cli.min_size {
                groups
                    .entry((entry.crc32, entry.size))
                    .or_default()
                    .push(dir.join(&*entry.path()));
                files += 1;
            }
        }
    }
    groups.retain(|_, v| v.len() > 1);
    let total_groups = groups.len();
    eprintln!(
        "relink: {versions} complete versions, {files} files, {total_groups} groups with more \
         than one path"
    );

    let work = std::sync::Mutex::new(groups.into_iter().collect::<Vec<_>>());
    let stats = Stats::default();
    std::thread::scope(|scope| {
        for _ in 0..cli.threads.max(1) {
            scope.spawn(|| {
                loop {
                    let Some((key, paths)) = work.lock().unwrap().pop() else {
                        break;
                    };
                    process_group(key, paths, cli, &stats);
                    let done = stats.groups_done.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 5000 == 0 {
                        eprintln!("relink: {done}/{total_groups} groups");
                    }
                }
            });
        }
    });

    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    eprintln!(
        "relink: {} files are separate copies of another file; {} inodes and {:.1} GB would be \
         freed. {} already linked, {} on another filesystem.",
        g(&stats.to_relink),
        g(&stats.freed_inodes),
        g(&stats.freed_bytes) as f64 / 1e9,
        g(&stats.already_linked),
        g(&stats.other_filesystem),
    );
    if cli.apply {
        eprintln!(
            "relink: relinked {}, {} mismatched (left alone), {} errors",
            g(&stats.relinked),
            g(&stats.mismatched),
            g(&stats.errors),
        );
        if g(&stats.errors) > 0 {
            bail!("{} files could not be relinked", g(&stats.errors));
        }
        // Paths now point at other inodes, so an existing index is out of date.
        if g(&stats.relinked) > 0 && crate::index::path(&root).exists() {
            eprintln!("relink: refreshing the archive index");
            crate::index::build(&root, cli.threads)?.save(&root)?;
        }
    } else {
        eprintln!("relink: dry run; pass --apply to do it");
    }

    Ok(())
}
