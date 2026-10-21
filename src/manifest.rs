// SPDX-License-Identifier: GPL-3.0-only

//! A manifest for each source zip, `<zip>.manifest`: what is in it, as the zip's own
//! central directory says, so zips can be compared (what they share, how alike they are)
//! without opening any of them.
//!
//! One line per file, sorted by path, after a header: `crc32<TAB>size<TAB>path`, the CRC
//! as 8 hex digits and the size in bytes. A file's identity is its CRC and size, as in a
//! package's `.ver`. Directories are left out. A tab or newline in a path (none seen) is
//! written as `?`.
//!
//! Only the end of the zip is read, so it is cheap beside the download it follows. A
//! manifest is written once the zip is whole and checked, and for zips already in the
//! archive that have none, on the next sync.

use std::{
    fmt::Write as _,
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rawzip::{RECOMMENDED_BUFFER_SIZE, ZipArchive};
use tracing::warn;

const HEADER: &str = "crc32\tsize\tpath\n";

pub fn path_for(zip: &Path) -> PathBuf {
    PathBuf::from(format!("{}.manifest", zip.display()))
}

const SEVEN_Z: &[u8] = b"7z\xbc\xaf\x27\x1c";

/// Some of what is named `.zip` is a 7z (Mobis serves both under the one name); its header
/// holds the same CRC and size per file.
fn is_7z(path: &Path) -> bool {
    let mut head = [0u8; 6];
    File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .is_ok_and(|()| head == SEVEN_Z)
}

fn clean(path: &str) -> String {
    path.replace(['\t', '\n', '\r'], "?")
}

fn rows_7z(path: &Path) -> Result<Vec<(String, u32, u64)>> {
    let archive = sevenz_rust2::Archive::open(path).with_context(|| format!("Failed to read the header of {path:?}"))?;

    Ok(archive
        .files
        .iter()
        .filter(|e| !e.is_directory() && !e.is_anti_item())
        // An empty file has no stream, and so no CRC to read: its CRC is 0.
        .map(|e| (clean(e.name()), if e.has_crc { e.crc as u32 } else { 0 }, e.size()))
        .collect())
}

fn rows_zip(path: &Path) -> Result<Vec<(String, u32, u64)>> {
    let file = File::open(path).with_context(|| format!("Failed to open {path:?}"))?;
    let mut buffer = vec![0u8; RECOMMENDED_BUFFER_SIZE];
    let archive = ZipArchive::from_file(file, &mut buffer)
        .with_context(|| format!("Failed to read the directory of {path:?}"))?;

    let mut rows = vec![];
    let mut entries = archive.entries(&mut buffer);
    while let Some(entry) = entries.next_entry().context("Failed to list zip entries")? {
        if entry.is_dir() {
            continue;
        }
        let name = String::from_utf8_lossy(entry.file_path().as_bytes());
        rows.push((clean(&name), entry.crc32(), entry.uncompressed_size_hint()));
    }

    Ok(rows)
}

/// The manifest of the zip (or 7z) at `zip`.
pub fn render(zip: &Path) -> Result<String> {
    let mut rows = if is_7z(zip) { rows_7z(zip)? } else { rows_zip(zip)? };
    rows.sort();

    let mut out = String::from(HEADER);
    for (path, crc, size) in rows {
        let _ = writeln!(out, "{crc:08x}\t{size}\t{path}");
    }

    Ok(out)
}

/// Write the manifest beside `zip`, whole or not at all.
pub fn write(zip: &Path) -> Result<()> {
    let text = render(zip)?;
    let manifest = path_for(zip);
    let part = PathBuf::from(format!("{}.part", manifest.display()));
    std::fs::write(&part, text).with_context(|| format!("Failed to write {part:?}"))?;
    std::fs::rename(&part, &manifest).with_context(|| format!("Failed to rename {part:?}"))?;

    Ok(())
}

/// Write `zip`'s manifest unless it has one. A failure is only a warning: the manifest is
/// a convenience, and the zip is good without it.
pub async fn ensure(zip: PathBuf) {
    if path_for(&zip).exists() {
        return;
    }
    let name = zip.display().to_string();
    match tokio::task::spawn_blocking(move || write(&zip)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("{name}: no manifest: {e:#}"),
        Err(e) => warn!("{name}: no manifest: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let path = dir.join(name);
        let mut archive = rawzip::ZipArchiveWriter::new(File::create(&path).unwrap());
        archive.new_dir("src/").create().unwrap();
        for (file, body) in files {
            let (mut entry, config) = archive
                .new_file(file)
                .compression_method(rawzip::CompressionMethod::STORE)
                .start()
                .unwrap();
            let mut writer = config.wrap(&mut entry);
            std::io::Write::write_all(&mut writer, body.as_bytes()).unwrap();
            let (_, output) = writer.finish().unwrap();
            entry.finish(output).unwrap();
        }
        archive.finish().unwrap();

        path
    }

    #[test]
    fn lists_files_by_crc_and_size() {
        let dir = std::env::temp_dir().join(format!("nudl-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let z = zip(&dir, "a.zip", &[("src/main.c", "int main;\n"), ("README", "hi\n")]);

        // crc32("hi\n") = d7ce6e6d? computed, not remembered: compare with crc32fast.
        let want = |body: &str| format!("{:08x}", crc32fast::hash(body.as_bytes()));
        assert_eq!(
            render(&z).unwrap(),
            format!(
                "{HEADER}{}\t3\tREADME\n{}\t10\tsrc/main.c\n",
                want("hi\n"),
                want("int main;\n"),
            ),
        );

        write(&z).unwrap();
        assert_eq!(std::fs::read_to_string(path_for(&z)).unwrap(), render(&z).unwrap());
        assert!(!PathBuf::from(format!("{}.part", path_for(&z).display())).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lists_a_7z_named_zip() {
        // 7zz a: README ("hi\n"), src/main.c ("int main;\n") and an empty file.
        let seven = hex_literal::hex!(
            "377abcaf271c000430e5f2ba9800000000000000220000000000000084709f0b01000c68690a696e74206d61696e3b0a"
            "000000813307ae0fcf04718c07c8437f41b1fafddd83a8504dbf73e7333d8c425f1b09b924fed05bb55fc74e87c2c6b1"
            "0dcf6e4ce32a74816bfbf0076abc5367fef02835088f646e81146153bd44aa4ad0c89f7223c279baa46e57f61bec2a40"
            "4606e160b4e24f8328b9bebd2fa68ba2f69863b14c469f2efadc22ee28be800d1b3c32781c0000001706110109808700"
            "070b01000123030101055d001000000c80a60a014a04d84d0000"
        );
        let dir = std::env::temp_dir().join(format!("nudl-manifest7z-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let z = dir.join("OSA_x.zip");
        std::fs::write(&z, seven).unwrap();

        assert_eq!(
            render(&z).unwrap(),
            format!("{HEADER}00000000\t0\tEMPTY\ned6f7a7a\t3\tREADME\n7546a1f0\t10\tsrc/main.c\n"),
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
