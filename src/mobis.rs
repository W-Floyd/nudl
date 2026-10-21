// SPDX-License-Identifier: GPL-3.0-only

//! Hyundai Mobis's open-source packages (the source and licence notices for each
//! head unit platform), published at mobis.com/en/tech/rnd.do.
//!
//! The page hard-codes one tab per vehicle model, each calling `fnOpenSource(model)`,
//! which POSTs the model to `opensourceSelect.do` for its platforms and two zips each
//! (`OSA`: source, `OSB`: licence notice). `FileDown.do` serves them, but sends no
//! Content-Length and ignores Range, so a file is fetched whole or not at all; speed
//! comes from several files at once, spread over the proxies.
//!
//! Zips land in `<root>/mobis/<file id>/<stored name>` with `<stored name>.sha256`
//! once whole (many models share a file, so it is keyed by id), and
//! `<root>/mobis/index.tsv` maps model and platform to them.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use reqwest::header;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Semaphore, task::JoinSet};
use tokio_stream::StreamExt;
use tracing::warn;

use crate::{client::NuClient, notices::hex};

const PAGE: &str = "https://www.mobis.com/en/tech/rnd.do";
const SELECT: &str = "https://www.mobis.com/index/opensourceSelect.do";
const DOWN: &str = "https://www.mobis.com/cmm/fms/FileDown.do";
const AGENT: &str = "Mozilla/5.0";
/// Requests in flight at once; the site is one origin and not built for more.
const PARALLEL: usize = 4;

#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(rename = "rtnMap")]
    map: Map,
}

#[derive(Debug, Deserialize)]
struct Map {
    flist: Vec<Row>,
}

#[derive(Clone, Debug, Deserialize)]
struct Row {
    #[serde(rename = "osPlatform")]
    platform: String,
    #[serde(rename = "osAtchFileId")]
    source_id: String,
    #[serde(rename = "osAtchFileNm")]
    source_name: String,
    #[serde(rename = "olAtchFileId")]
    notice_id: String,
    #[serde(rename = "olAtchFileNm")]
    notice_name: String,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub model: String,
    pub platform: String,
    pub files: [File; 2],
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct File {
    pub id: String,
    pub name: String,
}

impl File {
    fn path(&self, root: &Path) -> PathBuf {
        root.join("mobis").join(&self.id).join(&self.name)
    }

    fn url(&self) -> String {
        format!(
            "{DOWN}?fileId={}&fileSn=0&fileNm={}",
            urlencoding::encode(&self.id),
            urlencoding::encode(&self.name),
        )
    }
}

fn models(html: &str) -> Vec<String> {
    const CALL: &str = "fnOpenSource('";

    let mut out: Vec<String> = html
        .split(CALL)
        .skip(1)
        .filter_map(|s| s.split_once('\'').map(|(m, _)| m.replace("&amp;", "&")))
        .collect();
    out.sort();
    out.dedup();

    out
}

async fn rows(client: &NuClient, model: &str) -> Result<Vec<Row>> {
    let reply: Reply = client
        .client()
        .post(SELECT)
        .header(header::USER_AGENT, AGENT)
        .header(header::REFERER, PAGE)
        .header("X-Requested-With", "XMLHttpRequest")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded; charset=UTF-8")
        .body(format!("model={}", urlencoding::encode(model).replace("%20", "+")))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(reply.map.flist)
}

/// Every model's packages.
pub async fn list(client: &Arc<NuClient>) -> Result<Vec<Package>> {
    let html = client
        .client()
        .get(PAGE)
        .header(header::USER_AGENT, AGENT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let models = models(&html);
    if models.is_empty() {
        bail!("No models found on {PAGE}; the page changed");
    }

    let permits = Arc::new(Semaphore::new(PARALLEL));
    let mut set = JoinSet::new();
    for model in models {
        let (client, permits) = (client.clone(), permits.clone());
        set.spawn(async move {
            let _permit = permits.acquire().await?;
            let rows = rows(&client, &model)
                .await
                .with_context(|| format!("Failed to list {model}"))?;

            anyhow::Ok((model, rows))
        });
    }

    let mut out = vec![];
    while let Some(r) = set.join_next().await {
        let (model, rows) = r??;
        out.extend(rows.into_iter().map(|r| Package {
            model: model.clone(),
            platform: r.platform,
            files: [
                File { id: r.source_id, name: r.source_name },
                File { id: r.notice_id, name: r.notice_name },
            ],
        }));
    }
    out.sort_by(|a, b| (&a.model, &a.platform).cmp(&(&b.model, &b.platform)));

    Ok(out)
}

fn sum_path(zip: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", zip.display()))
}

async fn fetch(client: &NuClient, file: &File, path: &Path) -> Result<String> {
    let part = PathBuf::from(format!("{}.part", path.display()));
    let r = client
        .client()
        .get(file.url())
        .header(header::USER_AGENT, AGENT)
        .header(header::REFERER, PAGE)
        .send()
        .await?
        .error_for_status()?;

    let mut out = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("Failed to create {part:?}"))?;
    let (mut hasher, mut size) = (Sha256::new(), 0u64);
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        out.write_all(&chunk).await?;
        size += chunk.len() as u64;
    }
    out.flush().await?;
    // An unknown file id gets an empty 200 rather than an error.
    if size == 0 {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("Empty response");
    }
    tokio::fs::rename(&part, path)
        .await
        .with_context(|| format!("Failed to rename {part:?}"))?;

    Ok(hex(&hasher.finalize()))
}

async fn fetch_retrying(client: &NuClient, file: &File, path: &Path, retries: u8) -> Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("Failed to create {dir:?}"))?;
    }

    let mut attempt = 0;
    loop {
        match fetch(client, file, path).await {
            Ok(sum) => {
                let line = format!("{sum}  {}\n", file.name);
                tokio::fs::write(sum_path(path), line).await?;
                crate::manifest::ensure(path.to_path_buf()).await;

                return Ok(());
            }
            Err(e) if attempt < retries => {
                attempt += 1;
                warn!("{}: {e:#}; retry {attempt}/{retries}", file.name);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn index_tsv(packages: &[Package]) -> String {
    let mut s = String::from("model\tplatform\tfile id\tname\n");
    for p in packages {
        for f in &p.files {
            let _ = writeln!(s, "{}\t{}\t{}\t{}", p.model, p.platform, f.id, f.name);
        }
    }

    s
}

/// Bring every package's zips into `<root>/mobis`. With `plan_only`, report how many
/// are missing and stop. Returns the number fetched (or that would be).
pub async fn sync(
    client: &Arc<NuClient>,
    root: &Path,
    plan_only: bool,
    workers: usize,
    retries: u8,
) -> Result<usize> {
    let packages = list(client).await?;
    let distinct: BTreeMap<&File, ()> = packages.iter().flat_map(|p| &p.files).map(|f| (f, ())).collect();
    let todo: Vec<File> = distinct
        .into_keys()
        .filter(|f| !(sum_path(&f.path(root)).exists() && f.path(root).exists()))
        .cloned()
        .collect();
    eprintln!(
        "mobis: {} models' packages, {} distinct files, {} to download",
        packages.len(),
        packages.iter().flat_map(|p| &p.files).collect::<std::collections::BTreeSet<_>>().len(),
        todo.len(),
    );
    if plan_only {
        return Ok(todo.len());
    }

    let permits = Arc::new(Semaphore::new(workers.max(1)));
    let mut set = JoinSet::new();
    for file in &todo {
        let (client, permits, file, path) = (client.clone(), permits.clone(), file.clone(), file.path(root));
        set.spawn(async move {
            let _permit = permits.acquire().await?;
            let r = fetch_retrying(&client, &file, &path, retries).await;

            anyhow::Ok((file, r))
        });
    }
    let mut failed = vec![];
    while let Some(r) = set.join_next().await {
        if let (file, Err(e)) = r?? {
            failed.push(format!("{}: {e:#}", file.name));
        }
    }

    // Zips from before manifests, or whose manifest could not be written then.
    for file in packages.iter().flat_map(|p| &p.files).collect::<std::collections::BTreeSet<_>>() {
        let path = file.path(root);
        if sum_path(&path).exists() && path.exists() {
            crate::manifest::ensure(path).await;
        }
    }

    let dir = root.join("mobis");
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join("index.tsv"), index_tsv(&packages)).await?;

    if !failed.is_empty() {
        for f in &failed {
            eprintln!("  {f}");
        }
        bail!("{} Mobis downloads failed; rerun to retry", failed.len());
    }
    eprintln!("mobis: {} downloaded", todo.len());

    Ok(todo.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_models() {
        let html = r#"function fnOpenSource(model) {}
            <a onclick="fnOpenSource('EV6 ME (CV1)');">x</a>
            <a onclick="fnOpenSource('EV6 ME (CV1)');">x</a>
            <a onclick="fnOpenSource('A &amp; B');">y</a>"#;

        assert_eq!(models(html), ["A & B", "EV6 ME (CV1)"]);
    }
}
