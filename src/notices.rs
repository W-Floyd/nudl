// SPDX-License-Identifier: GPL-3.0-only

//! Firmware published only as a download on a notice at update.kia.com (the Niro Plus
//! taxi's, for one), which the navigation updater's API doesn't list.
//!
//! The notice list is server-rendered, and a notice links its zip as a plain URL.
//! Zips land in `<root>/notices/<notice id>/<name>.zip`, with `<name>.zip.sha256`
//! written once the file is whole. The archive index only reads `.ver` files, so this
//! tree is invisible to it.

use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use crate::client::NuClient;

const BASE: &str = "https://update.kia.com";
const LIST: &str = "/KR/KO/updateNoticeList";
const VIEW: &str = "/KR/KO/updateNoticeView/";
/// The site serves a bare error page to some non-browser agents.
const AGENT: &str = "Mozilla/5.0";
/// Pages walked at most; the list repeats or empties once past the last.
const MAX_PAGES: usize = 100;

#[derive(Clone, Debug)]
pub struct Zip {
    pub url: String,
    /// Decoded file name.
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub id: String,
    pub zips: Vec<Zip>,
}

impl Notice {
    pub fn url(&self) -> String {
        format!("{BASE}{VIEW}{}", self.id)
    }
}

/// Values of the quoted attributes in `html` that satisfy `keep`.
fn quoted<'a>(html: &'a str, keep: impl Fn(&str) -> bool + 'a) -> impl Iterator<Item = &'a str> {
    html.split('"').filter(move |s| keep(s))
}

fn notice_ids(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();

    quoted(html, |s| s.starts_with(VIEW))
        .map(|s| &s[VIEW.len()..])
        .filter(|id| !id.is_empty() && *id != "software-version" && !id.contains('/'))
        .filter(|id| seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

fn zips(html: &str) -> Vec<Zip> {
    let mut seen = HashSet::new();

    quoted(html, |s| s.ends_with(".zip") && s.contains("/s3/upload/"))
        .filter_map(|s| {
            let url = if s.starts_with('/') {
                format!("{BASE}{s}")
            } else {
                s.to_owned()
            };
            let raw = url.rsplit('/').next()?;
            let name: String = urlencoding::decode(raw)
                .ok()?
                .chars()
                .map(|c| if c == '/' || c.is_control() { '_' } else { c })
                .collect();

            seen.insert(url.clone()).then_some(Zip { url, name })
        })
        .collect()
}

async fn get_text(client: &NuClient, url: &str) -> Result<String> {
    Ok(client
        .client()
        .get(url)
        .header(header::USER_AGENT, AGENT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Every notice that links a zip, newest first.
pub async fn list(client: &Arc<NuClient>) -> Result<Vec<Notice>> {
    let mut ids: Vec<String> = vec![];
    let mut seen = HashSet::new();

    for page in 1..=MAX_PAGES {
        let html = get_text(client, &format!("{BASE}{LIST}?page={page}"))
            .await
            .with_context(|| format!("Failed to list notices (page {page})"))?;
        let new: Vec<String> = notice_ids(&html)
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();
        if new.is_empty() {
            break;
        }
        ids.extend(new);
    }
    debug!("{} notices listed", ids.len());

    let permits = Arc::new(Semaphore::new(8));
    let mut set = JoinSet::new();
    for (i, id) in ids.iter().enumerate() {
        let (client, permits, id) = (client.clone(), permits.clone(), id.clone());
        set.spawn(async move {
            let _permit = permits.acquire().await?;
            let html = get_text(&client, &format!("{BASE}{VIEW}{id}"))
                .await
                .with_context(|| format!("Failed to read notice {id}"))?;

            anyhow::Ok((i, Notice { id, zips: zips(&html) }))
        });
    }

    let mut found: Vec<(usize, Notice)> = vec![];
    while let Some(r) = set.join_next().await {
        let (i, n) = r??;
        if !n.zips.is_empty() {
            found.push((i, n));
        }
    }
    found.sort_by_key(|(i, _)| *i);

    Ok(found.into_iter().map(|(_, n)| n).collect())
}

fn dest(root: &Path, notice: &Notice, zip: &Zip) -> PathBuf {
    root.join("notices").join(&notice.id).join(&zip.name)
}

fn sum_path(zip: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", zip.display()))
}

struct Head {
    size: u64,
    /// The server's own SHA-256, when it publishes one.
    sha256: Option<String>,
}

async fn head(client: &NuClient, url: &str) -> Result<Head> {
    let r = client
        .client()
        .head(url)
        .header(header::USER_AGENT, AGENT)
        .send()
        .await?
        .error_for_status()?;
    let h = r.headers();
    let size = h
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .context("No Content-Length")?;
    let sha256 = h
        .get("x-amz-meta-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());

    Ok(Head { size, sha256 })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Smallest file worth splitting into parallel ranges.
const SPLIT_MIN: u64 = 64 << 20;

/// Fetch bytes `start..end` of `url` into `part`, resuming what is already there.
async fn fetch_range(
    client: &NuClient,
    url: &str,
    part: &Path,
    (start, end): (u64, u64),
    done: &AtomicU64,
    retries: u8,
) -> Result<()> {
    let want = end - start;
    let mut attempt = 0;

    loop {
        let result = async {
            let have = tokio::fs::metadata(part).await.map_or(0, |m| m.len());
            if have > want {
                tokio::fs::remove_file(part).await?;
                bail!("Partial range larger than requested; discarded");
            }
            if have == want {
                return Ok(have);
            }

            let r = client
                .client()
                .get(url)
                .header(header::USER_AGENT, AGENT)
                .header(header::RANGE, format!("bytes={}-{}", start + have, end - 1))
                .send()
                .await?
                .error_for_status()?;
            if r.status() != StatusCode::PARTIAL_CONTENT {
                bail!("Range ignored (HTTP {})", r.status());
            }

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(part)
                .await?;
            let (mut got, mut stream) = (have, r.bytes_stream());
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                file.write_all(&chunk).await?;
                got += chunk.len() as u64;
                done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            file.flush().await?;

            anyhow::Ok(got)
        }
        .await;

        match result {
            Ok(got) if got == want => return Ok(()),
            Ok(got) => bail!("Range got {got} of {want} bytes"),
            Err(e) if attempt < retries => {
                attempt += 1;
                warn!("{url}: {e:#}; retry {attempt}/{retries}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Fetch `url` as `n` parallel ranges (spread over the proxies), then join them into
/// `path` and return the SHA-256. Each range resumes from `<path>.part.<i>`.
async fn fetch_split(
    client: &Arc<NuClient>,
    url: &str,
    path: &Path,
    head: &Head,
    n: usize,
    retries: u8,
) -> Result<String> {
    let chunk = head.size.div_ceil(n as u64);
    let ranges: Vec<(u64, u64)> = (0..n as u64)
        .map(|i| (i * chunk, ((i + 1) * chunk).min(head.size)))
        .filter(|(s, e)| s < e)
        .collect();
    let parts: Vec<PathBuf> = (0..ranges.len())
        .map(|i| PathBuf::from(format!("{}.part.{i}", path.display())))
        .collect();

    let done = Arc::new(AtomicU64::new(0));
    let progress = {
        let (done, name, size) = (done.clone(), path.file_name().unwrap_or_default().to_owned(), head.size);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                eprintln!(
                    "notices: {} {}/{} MB",
                    name.to_string_lossy(),
                    done.load(Ordering::Relaxed) / 1_000_000,
                    size / 1_000_000,
                );
            }
        })
    };

    let mut set = JoinSet::new();
    for (range, part) in ranges.iter().zip(&parts) {
        let (client, url, part, range, done) =
            (client.clone(), url.to_owned(), part.clone(), *range, done.clone());
        set.spawn(async move { fetch_range(&client, &url, &part, range, &done, retries).await });
    }
    let mut result = Ok(());
    while let Some(r) = set.join_next().await {
        if let Err(e) = r.map_err(anyhow::Error::from).and_then(|r| r) {
            result = Err(e);
            set.abort_all();
        }
    }
    progress.abort();
    result?;

    // Join in order, hashing as it goes.
    let joined = PathBuf::from(format!("{}.part", path.display()));
    let mut out = tokio::fs::File::create(&joined).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    for part in &parts {
        let mut f = tokio::fs::File::open(part).await?;
        loop {
            let k = f.read(&mut buf).await?;
            if k == 0 {
                break;
            }
            hasher.update(&buf[..k]);
            out.write_all(&buf[..k]).await?;
        }
    }
    out.flush().await?;
    let sum = hex(&hasher.finalize());
    if let Some(want) = &head.sha256
        && *want != sum
    {
        // The pieces are suspect; start over rather than resume them.
        for p in &parts {
            let _ = tokio::fs::remove_file(p).await;
        }
        let _ = tokio::fs::remove_file(&joined).await;
        bail!("SHA-256 mismatch: server {want}, got {sum}");
    }
    tokio::fs::rename(&joined, path).await?;
    for p in &parts {
        let _ = tokio::fs::remove_file(p).await;
    }

    Ok(sum)
}

/// Fetch `url` into `path` and return the SHA-256 of the result. A large file is
/// split over `connections` ranges; either way an interrupted run resumes from what
/// reached the disk.
async fn fetch(
    client: &Arc<NuClient>,
    url: &str,
    path: &Path,
    head: &Head,
    connections: usize,
    retries: u8,
) -> Result<String> {
    if connections > 1 && head.size >= SPLIT_MIN {
        return fetch_split(client, url, path, head, connections, retries).await;
    }

    let part = PathBuf::from(format!("{}.part", path.display()));
    let mut attempt = 0;

    loop {
        match fetch_once(client, url, &part, head).await {
            Ok(sum) => {
                tokio::fs::rename(&part, path)
                    .await
                    .with_context(|| format!("Failed to rename {part:?}"))?;

                return Ok(sum);
            }
            Err(e) if attempt < retries => {
                attempt += 1;
                warn!("{url}: {e:#}; retry {attempt}/{retries}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn fetch_once(client: &NuClient, url: &str, part: &Path, head: &Head) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut have = 0u64;

    if let Ok(mut f) = tokio::fs::File::open(part).await {
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            have += n as u64;
        }
    }
    if have > head.size {
        tokio::fs::remove_file(part).await?;
        bail!("Partial file larger than the download; discarded");
    }

    if have < head.size {
        let r = client
            .client()
            .get(url)
            .header(header::USER_AGENT, AGENT)
            .header(header::RANGE, format!("bytes={have}-"))
            .send()
            .await?
            .error_for_status()?;
        // A 200 to a ranged request would restart the body and corrupt an append.
        if have > 0 && r.status() != StatusCode::PARTIAL_CONTENT {
            bail!("Unexpected HTTP status {}", r.status());
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part)
            .await
            .with_context(|| format!("Failed to open {part:?}"))?;
        let mut stream = r.bytes_stream();
        let mut last = Instant::now();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            have += chunk.len() as u64;

            if last.elapsed() >= Duration::from_secs(30) {
                eprintln!(
                    "notices: {} {}/{} MB",
                    part.file_name().unwrap_or_default().to_string_lossy(),
                    have / 1_000_000,
                    head.size / 1_000_000,
                );
                last = Instant::now();
            }
        }
        file.flush().await?;
    }

    if have != head.size {
        bail!("Got {have} of {} bytes", head.size);
    }
    let sum = hex(&hasher.finalize());
    if let Some(want) = &head.sha256
        && *want != sum
    {
        // Resuming a bad part would only reproduce the mismatch.
        let _ = tokio::fs::remove_file(part).await;
        bail!("SHA-256 mismatch: server {want}, got {sum}");
    }

    Ok(sum)
}

/// Bring every notice zip into `<root>/notices`. With `plan_only`, report what is
/// missing and stop. Returns the number of zips fetched (or that would be).
pub async fn sync(
    client: &Arc<NuClient>,
    root: &Path,
    plan_only: bool,
    connections: usize,
    retries: u8,
) -> Result<usize> {
    let notices = list(client).await?;
    let (mut todo, mut bytes, mut failed) = (0, 0u64, vec![]);

    for n in &notices {
        for z in &n.zips {
            let path = dest(root, n, z);
            if sum_path(&path).exists() && path.exists() {
                continue;
            }

            let head = match head(client, &z.url).await {
                Ok(h) => h,
                Err(e) => {
                    failed.push(format!("{}: {e:#}", z.name));
                    continue;
                }
            };
            todo += 1;
            bytes += head.size;
            eprintln!("notices: {} {} ({} MB)", n.url(), z.name, head.size / 1_000_000);
            if plan_only {
                continue;
            }

            let dir = path.parent().expect("has a parent");
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("Failed to create {dir:?}"))?;
            match fetch(client, &z.url, &path, &head, connections, retries).await {
                Ok(sum) => {
                    let line = format!("{sum}  {}\n", z.name);
                    tokio::fs::write(sum_path(&path), line).await?;
                }
                Err(e) => failed.push(format!("{}: {e:#}", z.name)),
            }
        }
    }
    eprintln!(
        "notices: {} zips listed, {todo} {} ({} GB)",
        notices.iter().map(|n| n.zips.len()).sum::<usize>(),
        if plan_only { "to download" } else { "downloaded" },
        bytes / 1_000_000_000,
    );

    if !failed.is_empty() {
        for f in &failed {
            eprintln!("  {f}");
        }
        bail!("{} notice downloads failed; rerun to retry", failed.len());
    }

    Ok(todo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_notices_and_zips() {
        let html = r#"<a href="/KR/KO/updateNoticeView/software-version">x</a>
            <a href="/KR/KO/updateNoticeView/Y2htTa">a</a><a href="/KR/KO/updateNoticeView/Y2htTa">b</a>
            <a href="https://update.kia.com/s3/upload/global/updatenotice/20260303/A%20B.zip">z</a>
            <img src="/s3/upload/global/updatenotice/20260303/pic.png">"#;

        assert_eq!(notice_ids(html), ["Y2htTa"]);
        let z = zips(html);
        assert_eq!(z.len(), 1);
        assert_eq!(z[0].name, "A B.zip");
    }
}
