use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::{client::FirmwareInfo, version::VersionInfo};

type Key = (u32, u64);

/// Find `.ver` files under `dir`, descending at most `max_depth` directories
/// (unlimited when `None`). Hidden directories (`.cas`, `.cache`) are skipped.
fn collect_ver_files(
    dir: &Path,
    depth: usize,
    max_depth: Option<usize>,
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        // Not following symlinks keeps a loop in the archive from hanging the walk.
        let ty = entry.file_type()?;
        let path = entry.path();

        if ty.is_dir() {
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if !hidden && max_depth.is_none_or(|m| depth < m) {
                collect_ver_files(&path, depth + 1, max_depth, out)?;
            }
        } else if ty.is_file() && path.extension().is_some_and(|e| e == "ver") {
            out.push(path);
        }
    }

    Ok(())
}

/// Find the `.ver` files under `roots`; see [`collect_ver_files`] for the walk.
pub fn find_ver_files(roots: &[PathBuf], max_depth: Option<usize>) -> io::Result<Vec<PathBuf>> {
    let mut out = vec![];
    for root in roots {
        collect_ver_files(root, 0, max_depth, &mut out)?;
    }

    Ok(out)
}

/// Map (CRC32, size) to an existing file, for every file listed in a `.ver`
/// under `roots`. A file only exists at its final path once it is downloaded and
/// verified, so presence plus a matching size is enough to trust the `.ver`.
pub fn index(
    roots: &[PathBuf],
    min_size: u64,
    max_depth: Option<usize>,
) -> Result<HashMap<Key, PathBuf>> {
    let mut vers = vec![];
    for root in roots {
        collect_ver_files(root, 0, max_depth, &mut vers)
            .with_context(|| format!("Failed to scan for version files: {root:?}"))?;
    }

    let mut map = HashMap::new();

    for ver in vers {
        let Some(dir) = ver.parent() else { continue };
        let info = match fs::read_to_string(&ver)
            .map_err(anyhow::Error::from)
            .and_then(|s| s.parse::<VersionInfo>().map_err(anyhow::Error::from))
        {
            Ok(i) => i,
            Err(e) => {
                debug!("Skipping {ver:?}: {e}");
                continue;
            }
        };

        for entry in info.entries {
            // One verified copy per key is all that is needed, and each check is a
            // stat on what may be a slow filesystem.
            let key = (entry.crc32, entry.size);
            if entry.size < min_size || map.contains_key(&key) {
                continue;
            }

            let path = dir.join(&*entry.path());
            if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == entry.size) {
                map.insert(key, path);
            }
        }
    }

    Ok(map)
}

/// Hardlink files of `firmware` that `index` already holds into `output`, so the
/// downloader sees them as complete. Returns how many were linked.
pub fn link_files(
    index: &HashMap<Key, PathBuf>,
    output: &Path,
    firmware: &FirmwareInfo,
    min_size: u64,
) -> Result<usize> {
    let mut linked = 0;

    for file in &firmware.files {
        if file.size < min_size {
            continue;
        }

        let dest = output.join(&*file.path());
        if dest.exists() {
            continue;
        }

        let Some(src) = index.get(&(file.crc32, file.size)) else {
            continue;
        };

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {parent:?}"))?;
        }

        match fs::hard_link(src, &dest) {
            Ok(()) => {
                println!("Linked: {} <- {}", file.path(), src.display());
                linked += 1;
            }
            // Likely a different filesystem; download it normally.
            Err(e) => warn!("Failed to link {src:?} to {dest:?}: {e}"),
        }
    }

    Ok(linked)
}
