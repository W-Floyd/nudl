// SPDX-License-Identifier: GPL-3.0-only

//! Each brand's own open-source distributions (a source zip and a licence notice per
//! model), apart from Mobis's per-platform ones:
//!
//! - Kia, worldwide.kia.com/en/opensource: listed by `/api/opensource` as paths under
//!   `/files/opensource/`, which is S3 (lengths, ranges, sometimes `x-amz-meta-sha256`).
//! - Hyundai, hyundai.com/worldwide/en/opensource: listed by `opensource.list.do`;
//!   `download.file.do` sends no length and ignores ranges, as Mobis's does.
//! - Genesis, genesis.com/worldwide/en/opensource.html: the page links each DAM path
//!   through `fileDownload('…')`; the paths serve directly, with lengths and ranges.
//!
//! Files land in `<root>/opensource/<brand>/<id>/<name>` with `<name>.sha256` once
//! whole, and `<root>/opensource/<brand>/index.tsv` maps model to them.

use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::warn;

use crate::{cli::Brand, client::NuClient, mobis, notices};

const AGENT: &str = "Mozilla/5.0";

const KIA: &str = "https://worldwide.kia.com";
const KIA_LIST: &str = "/api/opensource";
const KIA_FILES: &str = "/files/opensource/";
const KIA_PAGE_SIZE: usize = 100;

const HYUNDAI: &str = "https://www.hyundai.com";
const HYUNDAI_PAGE: &str = "https://www.hyundai.com/worldwide/en/opensource";
const HYUNDAI_LIST: &str = "/wsvc/ww/opensource.list.do";

const GENESIS: &str = "https://www.genesis.com";
const GENESIS_PAGE: &str = "https://www.genesis.com/worldwide/en/opensource.html";

#[derive(Clone, Debug)]
pub struct Package {
    pub id: String,
    pub title: String,
    pub files: Vec<File>,
}

#[derive(Clone, Debug)]
pub struct File {
    pub url: String,
    pub name: String,
}

pub fn dir(root: &Path, brand: Brand) -> PathBuf {
    root.join("opensource").join(brand.as_pretty_str().to_lowercase())
}

/// A name safe as one path component.
fn clean(name: &str) -> String {
    name.chars().map(|c| if c == '/' || c.is_control() { '_' } else { c }).collect()
}

/// Whether the server can say how long a file is and send it in ranges.
fn ranged(brand: Brand) -> bool {
    !matches!(brand, Brand::Hyundai)
}

#[derive(Debug, Deserialize)]
struct KiaReply {
    data: KiaData,
}

#[derive(Debug, Deserialize)]
struct KiaData {
    list: Vec<KiaRow>,
    paging: KiaPaging,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiaPaging {
    total_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiaRow {
    id: u64,
    title: String,
    source_code: String,
    license_notice: String,
}

/// Some listed paths lack the extension the stored file has (EV3's both 404 as given).
fn kia_file(path: &str, ext: &str) -> File {
    let path = match path.rsplit('/').next() {
        Some(last) if !last.contains('.') => format!("{path}.{ext}"),
        _ => path.to_owned(),
    };
    let encoded: Vec<_> = path.split('/').map(urlencoding::encode).collect();

    File {
        url: format!("{KIA}{KIA_FILES}{}", encoded.join("/")),
        name: clean(path.rsplit('/').next().unwrap_or(&path)),
    }
}

async fn kia(client: &NuClient) -> Result<Vec<Package>> {
    let mut rows = vec![];
    for p in 0.. {
        let reply: KiaReply = client
            .client()
            .get(format!("{KIA}{KIA_LIST}?pageSize={KIA_PAGE_SIZE}&p={p}"))
            .header(header::USER_AGENT, AGENT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .with_context(|| format!("Failed to list {KIA}{KIA_LIST} (page {p})"))?;
        let done = reply.data.list.is_empty();
        rows.extend(reply.data.list);
        if done || rows.len() >= reply.data.paging.total_count {
            break;
        }
    }

    Ok(rows
        .into_iter()
        .map(|r| Package {
            id: r.id.to_string(),
            title: r.title,
            files: vec![kia_file(&r.source_code, "zip"), kia_file(&r.license_notice, "txt")],
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct HyundaiReply {
    data: Vec<HyundaiRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyundaiRow {
    node_id: String,
    display_name: String,
    #[serde(default)]
    source_code: Vec<HyundaiFile>,
    #[serde(default)]
    license_code: Vec<HyundaiFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HyundaiFile {
    file_name: String,
    file_path: String,
}

async fn hyundai(client: &NuClient) -> Result<Vec<Package>> {
    let reply: HyundaiReply = client
        .client()
        .post(format!("{HYUNDAI}{HYUNDAI_LIST}"))
        .header(header::USER_AGENT, AGENT)
        .header(header::REFERER, HYUNDAI_PAGE)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded; charset=UTF-8")
        .body("category=")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("Failed to list {HYUNDAI}{HYUNDAI_LIST}"))?;

    Ok(reply
        .data
        .into_iter()
        .map(|r| Package {
            id: clean(&r.node_id),
            title: r.display_name.trim().to_owned(),
            files: r
                .source_code
                .into_iter()
                .chain(r.license_code)
                .map(|f| File {
                    // Node ids hold spaces (`QV PE-2023`).
                    url: format!("{HYUNDAI}{}", f.file_path.replace(' ', "%20")),
                    name: clean(&f.file_name),
                })
                .collect(),
        })
        .collect())
}

/// The DAM paths the page passes to `fileDownload`, grouped by folder, in page order.
fn genesis_packages(html: &str) -> Vec<Package> {
    const CALL: &str = "fileDownload('";

    let mut seen = HashSet::new();
    let mut out: Vec<Package> = vec![];
    for path in html.split(CALL).skip(1).filter_map(|s| s.split_once('\'').map(|(p, _)| p)) {
        // An empty path is a model whose files are "to be updated soon".
        if path.is_empty() || !seen.insert(path) {
            continue;
        }
        let Some((folder, raw)) = path.rsplit_once('/') else { continue };
        let id = folder.rsplit('/').next().unwrap_or(folder);
        let file = File {
            url: format!("{GENESIS}{path}"),
            name: clean(&urlencoding::decode(raw).map_or_else(|_| raw.into(), |s| s.into_owned())),
        };
        match out.iter_mut().find(|p| p.id == id) {
            Some(p) => p.files.push(file),
            None => out.push(Package { id: id.to_owned(), title: id.to_owned(), files: vec![file] }),
        }
    }

    out
}

async fn genesis(client: &NuClient) -> Result<Vec<Package>> {
    let html = client
        .client()
        .get(GENESIS_PAGE)
        .header(header::USER_AGENT, AGENT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await
        .with_context(|| format!("Failed to read {GENESIS_PAGE}"))?;

    Ok(genesis_packages(&html))
}

/// Every model's distribution for `brand`.
pub async fn list(client: &NuClient, brand: Brand) -> Result<Vec<Package>> {
    let packages = match brand {
        Brand::Kia => kia(client).await?,
        Brand::Hyundai => hyundai(client).await?,
        Brand::Genesis => genesis(client).await?,
    };
    if packages.is_empty() {
        bail!("No {} open-source packages listed; the site changed", brand.as_pretty_str());
    }

    Ok(packages)
}

fn index_tsv(packages: &[Package]) -> String {
    let mut s = String::from("id\ttitle\tname\n");
    for p in packages {
        for f in &p.files {
            let _ = writeln!(s, "{}\t{}\t{}", p.id, p.title, f.name);
        }
    }

    s
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status) == Some(StatusCode::NOT_FOUND)
}

/// For a server without ranges: fetch whole, retrying from the start. `None` when the
/// server answers with a web page, not the file.
async fn fetch_whole(client: &NuClient, url: &str, referer: &str, path: &Path, retries: u8) -> Result<Option<String>> {
    let mut attempt = 0;
    loop {
        match mobis::fetch(client, url, referer, path).await {
            Ok(sum) => return Ok(Some(sum)),
            Err(e) if e.is::<mobis::WebPage>() => return Ok(None),
            Err(e) if attempt < retries => {
                attempt += 1;
                warn!("{url}: {e:#}; retry {attempt}/{retries}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// One file to fetch: where it goes, and its length when the server gives one.
struct Job {
    url: String,
    name: String,
    path: PathBuf,
    head: Option<notices::Head>,
}

/// Fetch a job's file and write its checksum (and a zip's manifest) beside it. `false`
/// when the server sent a web page, not the file.
async fn run(client: &Arc<NuClient>, job: &Job, referer: &str, connections: usize, retries: u8) -> Result<bool> {
    let parent = job.path.parent().expect("has a parent");
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create {parent:?}"))?;
    let sum = match &job.head {
        Some(head) => Some(notices::fetch(client, &job.url, &job.path, head, connections, retries).await?),
        None => fetch_whole(client, &job.url, referer, &job.path, retries).await?,
    };
    let Some(sum) = sum else { return Ok(false) };
    tokio::fs::write(notices::sum_path(&job.path), format!("{sum}  {}\n", job.name)).await?;
    // A licence notice is text, so has no manifest.
    if job.name.to_ascii_lowercase().ends_with(".zip") {
        crate::manifest::ensure(job.path.clone()).await;
    }

    Ok(true)
}

/// Bring `brand`'s files into its folder, `workers` at a time (each request goes to the
/// next proxy). With `plan_only`, report what is missing and stop. Returns the number
/// fetched (or that would be).
async fn sync_brand(
    client: Arc<NuClient>,
    root: PathBuf,
    brand: Brand,
    plan_only: bool,
    workers: usize,
    connections: usize,
    retries: u8,
) -> Result<usize> {
    let tag = format!("opensource {}", brand.as_pretty_str().to_lowercase());
    let referer = match brand {
        Brand::Kia => KIA,
        Brand::Hyundai => HYUNDAI_PAGE,
        Brand::Genesis => GENESIS_PAGE,
    };
    let packages = list(&client, brand).await?;
    let dir = dir(&root, brand);
    let (mut jobs, mut bytes, mut failed) = (vec![], 0u64, vec![]);

    for p in &packages {
        for f in &p.files {
            let path = dir.join(&p.id).join(&f.name);
            if notices::sum_path(&path).exists() && path.exists() {
                if !plan_only && f.name.to_ascii_lowercase().ends_with(".zip") {
                    crate::manifest::ensure(path).await;
                }
                continue;
            }

            let head = if ranged(brand) {
                match notices::head(&client, &f.url).await {
                    Ok(h) => Some(h),
                    // Listed but not stored; not a failure of the run, and asked for again next time.
                    Err(e) if is_not_found(&e) => {
                        warn!("{tag}: {}: listed, but not on the server; skipped", f.url);
                        continue;
                    }
                    Err(e) => {
                        failed.push(format!("{}: {e:#}", f.url));
                        continue;
                    }
                }
            } else {
                None
            };
            match head.as_ref().map(|h| h.size) {
                Some(size) => {
                    bytes += size;
                    eprintln!("{tag}: {} {} ({} MB)", p.title, f.name, size / 1_000_000);
                }
                None => eprintln!("{tag}: {} {}", p.title, f.name),
            }
            jobs.push(Job { url: f.url.clone(), name: f.name.clone(), path, head });
        }
    }

    let size = if ranged(brand) { format!(" ({} GB)", bytes / 1_000_000_000) } else { String::new() };
    if plan_only {
        eprintln!("{tag}: {} models, {} files to download{size}", packages.len(), jobs.len());
        return Ok(jobs.len());
    }

    let permits = Arc::new(Semaphore::new(workers.max(1)));
    let mut set = JoinSet::new();
    for job in jobs {
        let (client, permits) = (client.clone(), permits.clone());
        set.spawn(async move {
            let _permit = permits.acquire().await?;
            let r = run(&client, &job, referer, connections, retries).await;

            anyhow::Ok((job, r))
        });
    }
    let (mut fetched, mut skipped) = (0, 0);
    while let Some(r) = set.join_next().await {
        match r?? {
            (_, Ok(true)) => fetched += 1,
            (job, Ok(false)) => {
                skipped += 1;
                warn!("{tag}: {}: the server sent a web page, not the file; skipped", job.url);
            }
            (job, Err(e)) => failed.push(format!("{}: {e:#}", job.url)),
        }
    }
    eprintln!("{tag}: {} models, {fetched} files downloaded{size}, {skipped} skipped", packages.len());

    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join("index.tsv"), index_tsv(&packages)).await?;

    if !failed.is_empty() {
        for f in &failed {
            eprintln!("  {f}");
        }
        bail!("{} {} open-source downloads failed; rerun to retry", failed.len(), brand.as_pretty_str());
    }

    Ok(fetched)
}

/// [`sync_brand`] for each of `brands` (all of them if empty) at once: they are separate
/// servers. One brand failing doesn't stop the rest.
pub async fn sync(
    client: &Arc<NuClient>,
    root: &Path,
    brands: &[Brand],
    plan_only: bool,
    workers: usize,
    connections: usize,
    retries: u8,
) -> Result<usize> {
    let all = [Brand::Hyundai, Brand::Kia, Brand::Genesis];
    let brands = if brands.is_empty() { &all[..] } else { brands };

    let mut set = JoinSet::new();
    for &brand in brands {
        let (client, root) = (client.clone(), root.to_path_buf());
        set.spawn(sync_brand(client, root, brand, plan_only, workers, connections, retries));
    }
    let (mut todo, mut first_err) = (0, None);
    while let Some(r) = set.join_next().await {
        match r? {
            Ok(n) => todo += n,
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    Ok(todo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kia_files() {
        for (path, ext, url, name) in [
            (
                "The Kia EV3/The Kia EV3_source_distribution",
                "zip",
                "https://worldwide.kia.com/files/opensource/The%20Kia%20EV3/The%20Kia%20EV3_source_distribution.zip",
                "The Kia EV3_source_distribution.zip",
            ),
            (
                "EV2/EV2_opensource_notice.txt",
                "txt",
                "https://worldwide.kia.com/files/opensource/EV2/EV2_opensource_notice.txt",
                "EV2_opensource_notice.txt",
            ),
        ] {
            let f = kia_file(path, ext);
            assert_eq!((f.url.as_str(), f.name.as_str()), (url, name));
        }
    }

    #[test]
    fn parses_the_kia_list() {
        let json = r#"{"status":200,"data":{"list":[{"id":71,"title":"EV2",
            "sourceCode":"EV2/EV2_source_distribution.zip","licenseNotice":"EV2/EV2_opensource_notice.txt"}],
            "search":"","category":"","paging":{"totalCount":20,"entriesPerPage":100,"pagesPerPageGroup":10,"currentPage":0},
            "categories":[]}}"#;
        let reply: KiaReply = serde_json::from_str(json).unwrap();

        assert_eq!(reply.data.paging.total_count, 20);
        assert_eq!(reply.data.list[0].id, 71);
        assert_eq!(reply.data.list[0].source_code, "EV2/EV2_source_distribution.zip");
    }

    #[test]
    fn parses_the_hyundai_list() {
        let json = r#"{"data":[{"displayName":"PAVISE","nodeId":"QV PE-2023",
            "licenseCode":[{"fileName":"Pavise_open_source_notice.txt",
                "filePath":"/wsvc/ww/download.file.do?id=/content/hyundai/ww/data/opensource/data/QV PE-2023/licenseCode/info"}],
            "sourceCode":[]}],"resultCode":"0000","message":""}"#;
        let reply: HyundaiReply = serde_json::from_str(json).unwrap();

        assert_eq!(reply.data[0].node_id, "QV PE-2023");
        assert_eq!(reply.data[0].license_code[0].file_name, "Pavise_open_source_notice.txt");
        assert!(reply.data[0].source_code.is_empty());
    }

    #[test]
    fn finds_genesis_files() {
        let html = r#"<a href="javascript:fileDownload('/content/dam/genesis-p2/worldwide/opensource/g70-2023/G70%202023_source_distribution.zip');">
            <a href="javascript:fileDownload('/content/dam/genesis-p2/worldwide/opensource/g70-2023/G70%202023_source_distribution.zip');">
            <a href="javascript:fileDownload('');">
            <a href="javascript:fileDownload('/content/dam/genesis-p2/worldwide/opensource/gv70/gv70_source_distribution.zip');">
            <a href="javascript:fileDownload('/content/dam/genesis-p2/worldwide/opensource/gv70/gv70_open_source_notice.txt');">"#;
        let got = genesis_packages(html);

        assert_eq!(got.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["g70-2023", "gv70"]);
        assert_eq!(got[0].files.len(), 1);
        assert_eq!(got[0].files[0].name, "G70 2023_source_distribution.zip");
        assert_eq!(
            got[1].files[1].url,
            "https://www.genesis.com/content/dam/genesis-p2/worldwide/opensource/gv70/gv70_open_source_notice.txt",
        );
    }
}
