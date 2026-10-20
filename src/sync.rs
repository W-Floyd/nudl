// SPDX-License-Identifier: GPL-3.0-only

//! `nudl sync`: keep an archive of firmware up to date.
//!
//! The unit of work is a distinct file (CRC32 + size), not a firmware version.
//! Every version that needs a file the archive lacks waits on one download of it,
//! and gets a hardlink when it lands, so the download workers never idle behind a
//! version that shares a file with one in flight.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use cap_std::{ambient_authority, fs::Dir};
use tokio::{signal::ctrl_c, sync::Semaphore, task::JoinSet};
use tracing::{debug, warn};

use crate::{
    build_client,
    cli::{Brand, SyncCli},
    client::{self, CarInfo, FileInfo, FirmwareInfo, NuClient},
    download::{Downloader, MoveConfig, ProgressMessage},
    link, prepare_client,
    version::VersionInfo,
};

const REGIONS: &[&str] = &[
    "BR", "CA", "EU", "ID", "IN", "JP", "KR", "ME", "NZ", "RU", "SG", "TR", "US",
];
const GIB: u64 = 1 << 30;
/// Depth of `<brand>/<region>/<model id>/<version>` below the archive root.
const VERSION_DEPTH: usize = 4;

type Key = (u32, u64);

fn brand_dir(brand: Brand) -> String {
    format!("{brand:?}").to_lowercase()
}

/// One firmware version to put in the archive.
struct Target {
    label: String,
    car: Arc<CarInfo>,
    fw: Arc<FirmwareInfo>,
    dest: PathBuf,
    /// Files not yet in place.
    remaining: usize,
    /// Whether `<id>.ver` is on disk.
    ver_written: bool,
}

/// A version whose files are all in place; `.complete` still has to be written.
struct Completion {
    label: String,
    car: Arc<CarInfo>,
    fw: Arc<FirmwareInfo>,
    dest: PathBuf,
    ver_written: bool,
}

enum Kind {
    /// One large file, shared by every target that needs it.
    Big(usize),
    /// A target's small files, fetched together.
    Bulk(Vec<usize>),
}

struct Job {
    kind: Kind,
    /// Target whose directory receives the files.
    owner: usize,
    /// (target, file index) pairs to hardlink once the owner's file lands.
    waiters: Vec<(usize, usize)>,
    /// Scratch space to reserve, in bytes.
    need: u64,
    /// Name of the staging directory, stable across runs so a resume finds it.
    name: String,
}

struct Plan {
    targets: Vec<Target>,
    jobs: Vec<Job>,
    /// (target, file index, existing file to link from)
    links: Vec<(usize, usize, PathBuf)>,
    listed: u64,
    to_download: u64,
    to_link: u64,
    complete_targets: usize,
}

fn is_no_data(e: &anyhow::Error) -> bool {
    e.downcast_ref::<client::Error>()
        .is_some_and(|c| matches!(c, client::Error::RegionNoData(_)))
}

fn file_present(path: &Path, size: u64) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
}

/// Scratch needed to download a file: its parts, plus the joined file for a split.
fn file_need(f: &FileInfo) -> u64 {
    f.download_size() + if f.is_split() { f.size } else { 0 }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect()
}

async fn make_plan(cli: &SyncCli, root: &Path) -> Result<Plan> {
    let brands: Vec<Brand> = if cli.brand.is_empty() {
        vec![Brand::Hyundai, Brand::Kia, Brand::Genesis]
    } else {
        cli.brand.clone()
    };
    let regions: Vec<String> = if cli.region.is_empty() {
        REGIONS.iter().map(|r| (*r).to_owned()).collect()
    } else {
        cli.region.clone()
    };

    let index = {
        let root = root.to_path_buf();
        let min = cli.link_min_size;
        tokio::task::spawn_blocking(move || link::index(&[root], min, Some(VERSION_DEPTH)))
            .await??
    };
    debug!("Archive index: {} files", index.len());

    // Cars with at least one version not yet complete. Cars that are entirely
    // complete never need a manifest, so count their versions here.
    let mut pending: Vec<(Brand, String, CarInfo)> = vec![];
    let mut settled_versions = 0;
    for &brand in &brands {
        for region in &regions {
            eprint!("\rlisting {} {region}   ", brand_dir(brand));
            let (client, region, guid) =
                match prepare_client(brand, Some(region), &cli.network).await {
                    Ok(v) => v,
                    Err(e) if is_no_data(&e) => continue,
                    Err(e) => return Err(e),
                };
            let cars = client
                .get_cars(&region, &guid, brand.as_code_str())
                .await?;

            for car in cars {
                if !cli.model.is_empty() && !cli.model.contains(&car.id) {
                    continue;
                }
                let dest = root.join(brand_dir(brand)).join(&region).join(&car.id);
                if car
                    .versions
                    .iter()
                    .any(|v| !dest.join(v).join(".complete").exists())
                {
                    pending.push((brand, region.clone(), car));
                } else {
                    settled_versions += car.versions.len();
                }
            }
        }
    }
    eprintln!("\r{:40}", "");

    // Manifests, a few at a time; one client rotating over the proxies.
    let client = Arc::new(build_client(&cli.network, cli.network.proxy.clone())?);
    let permits = Arc::new(Semaphore::new(cli.plan_jobs.max(1)));
    let mut set = JoinSet::new();
    for (i, (_, region, car)) in pending.iter().enumerate() {
        let (client, permits) = (client.clone(), permits.clone());
        let (region, car) = (region.clone(), car.clone());
        set.spawn(async move {
            let _permit = permits.acquire().await?;
            let fw = client.get_firmware_info(&region, &car).await;

            anyhow::Ok((i, fw))
        });
    }
    let mut firmware: Vec<Option<Arc<FirmwareInfo>>> = vec![None; pending.len()];
    let mut done = 0;
    while let Some(r) = set.join_next().await {
        let (i, fw) = r??;
        firmware[i] = Some(Arc::new(
            fw.with_context(|| format!("Failed to get manifest for {}", pending[i].2.id))?,
        ));
        done += 1;
        if done % 25 == 0 || done == pending.len() {
            eprint!("\rplanning {done}/{}   ", pending.len());
        }
    }
    eprintln!();

    // Targets, then what each of their files needs.
    let mut targets: Vec<Target> = vec![];
    let mut complete_targets = settled_versions;
    for ((brand, region, car), fw) in pending.iter().zip(&firmware) {
        let fw = fw.clone().expect("fetched above");
        let car = Arc::new(car.clone());
        for version in &car.versions {
            let dest = root
                .join(brand_dir(*brand))
                .join(region)
                .join(&car.id)
                .join(version);
            if dest.join(".complete").exists() {
                complete_targets += 1;
                continue;
            }
            targets.push(Target {
                label: format!("{} {region} {} {version}", brand_dir(*brand), car.id),
                car: car.clone(),
                fw: fw.clone(),
                dest,
                remaining: 0,
                ver_written: false,
            });
        }
    }

    let mut jobs: Vec<Job> = vec![];
    let mut by_key: HashMap<Key, usize> = HashMap::new();
    let mut links = vec![];
    let (mut listed, mut to_download, mut to_link) = (0u64, 0u64, 0u64);

    for ti in 0..targets.len() {
        let t = &targets[ti];
        let mut bulk: Vec<usize> = vec![];
        let mut remaining = 0;

        for (fi, f) in t.fw.files.iter().enumerate() {
            listed += f.size;
            if file_present(&t.dest.join(f.path()), f.size) {
                continue;
            }
            remaining += 1;

            if f.size < cli.link_min_size {
                bulk.push(fi);
                to_download += f.size;
                continue;
            }

            let key = (f.crc32, f.size);
            if let Some(src) = index.get(&key) {
                links.push((ti, fi, src.clone()));
                to_link += f.size;
            } else if let Some(&j) = by_key.get(&key) {
                jobs[j].waiters.push((ti, fi));
                to_link += f.size;
            } else {
                by_key.insert(key, jobs.len());
                jobs.push(Job {
                    kind: Kind::Big(fi),
                    owner: ti,
                    waiters: vec![],
                    need: file_need(f),
                    name: format!("f-{:08x}-{}", f.crc32, f.size),
                });
                to_download += f.size;
            }
        }

        if !bulk.is_empty() {
            let need = bulk.iter().map(|&i| file_need(&t.fw.files[i])).sum();
            jobs.push(Job {
                kind: Kind::Bulk(bulk),
                owner: ti,
                waiters: vec![],
                need,
                name: format!("b-{}", sanitize(&t.label)),
            });
        }
        targets[ti].remaining = remaining;
    }

    Ok(Plan {
        targets,
        jobs,
        links,
        listed,
        to_download,
        to_link,
        complete_targets,
    })
}

#[cfg(unix)]
fn available_space(path: &Path) -> Option<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `s` is zeroed plain data that
    // statvfs fills in.
    let s = unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        s
    };

    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_space(_path: &Path) -> Option<u64> {
    None
}

fn mem_available() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;

    Some(kb * 1024)
}

/// Whether `path` is on a RAM-backed filesystem (so free space is really memory).
fn is_ram_backed(path: &Path) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return false;
    };

    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_dev, point, fstype) = (f.next()?, f.next()?, f.next()?);
            path.starts_with(point).then_some((point.len(), fstype))
        })
        .max_by_key(|(len, _)| *len)
        .is_some_and(|(_, t)| t == "tmpfs" || t == "ramfs")
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

struct Tier {
    path: PathBuf,
    cap: Option<u64>,
    ram: bool,
}

/// Scratch tiers with space accounting. A download reserves its worst-case size in
/// the first tier that has room, so a later tier only sees overflow.
struct Tiers {
    tiers: Vec<Tier>,
    reserved: Mutex<Vec<u64>>,
    headroom: u64,
}

impl Tiers {
    fn new(specs: &[String], headroom: u64) -> Result<Self> {
        let mut tiers = vec![];

        for spec in specs {
            let (path, cap) = match spec.rsplit_once(':') {
                Some((p, c)) if c.parse::<u64>().is_ok() => (p, Some(c.parse::<u64>()? * GIB)),
                _ => (spec.as_str(), None),
            };
            let path = PathBuf::from(path).join("nudl-sync");
            let ram = is_ram_backed(path.parent().unwrap_or(&path));

            if ram {
                // RAM doesn't survive a reboot, and leftovers would hold memory.
                let _ = fs::remove_dir_all(&path);
            }
            if let Err(e) = fs::create_dir_all(&path) {
                warn!("Stage tier {path:?} unusable, skipping: {e}");
                continue;
            }
            tiers.push(Tier { path, cap, ram });
        }

        let n = tiers.len();
        Ok(Self {
            tiers,
            reserved: Mutex::new(vec![0; n]),
            headroom,
        })
    }

    /// Reserve `need` bytes on the first tier with room.
    fn claim(&self, need: u64) -> Option<usize> {
        let mut reserved = self.reserved.lock().unwrap();

        for (i, t) in self.tiers.iter().enumerate() {
            if t.cap.is_some_and(|c| reserved[i] + need > c) {
                continue;
            }

            let mut avail = available_space(&t.path)?;
            if t.ram
                && let Some(mem) = mem_available()
            {
                avail = avail.min(mem);
            }
            // Free space already reflects what in-flight downloads wrote, so add
            // that back before subtracting every reservation.
            let written = dir_size(&t.path);
            if need + reserved[i] + self.headroom <= avail + written {
                reserved[i] += need;
                return Some(i);
            }
        }

        None
    }

    fn release(&self, tier: usize, need: u64) {
        self.reserved.lock().unwrap()[tier] -= need;
    }
}

struct State {
    targets: Vec<Target>,
    jobs: Vec<Job>,
    queue: VecDeque<usize>,
    active: usize,
    failed: Vec<String>,
    targets_done: usize,
    jobs_done: usize,
}

struct Shared {
    state: Mutex<State>,
    tiers: Tiers,
    bytes: AtomicU64,
    stop: AtomicBool,
    concurrency: usize,
    retries: u8,
    /// Background setup (manifests, links to files already archived): done / total.
    prep_done: AtomicU64,
    prep_total: u64,
}

fn hard_link_into(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if dst.exists() {
        return Ok(());
    }
    fs::hard_link(src, dst).with_context(|| format!("Failed to link {src:?} to {dst:?}"))
}

impl State {
    /// `n` more files of target `ti` are in place. Returns the target when that was
    /// the last. Only counts: the filesystem work happens outside the lock, since
    /// the archive filesystem can be very slow and every worker waits on this lock.
    fn landed(&mut self, ti: usize, n: usize) -> Option<Completion> {
        let t = &mut self.targets[ti];
        t.remaining = t.remaining.saturating_sub(n);

        (t.remaining == 0).then(|| Completion {
            label: t.label.clone(),
            car: t.car.clone(),
            fw: t.fw.clone(),
            dest: t.dest.clone(),
            ver_written: t.ver_written,
        })
    }
}

/// Write `<id>.ver` for a version, so later runs can find its files.
fn write_ver(dest: &Path, car: &CarInfo, fw: &FirmwareInfo) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("Failed to create {dest:?}"))?;
    let ver = VersionInfo::new(car, fw).to_string();
    let path = dest.join(format!("{}.ver", car.id));

    fs::write(&path, ver).with_context(|| format!("Failed to write {path:?}"))
}

/// Mark versions complete. Their manifest goes first if setup hasn't written it yet.
fn mark_complete(shared: &Shared, completions: Vec<Completion>) {
    for c in completions {
        let result = if c.ver_written {
            Ok(())
        } else {
            write_ver(&c.dest, &c.car, &c.fw)
        }
        .and_then(|()| {
            fs::write(c.dest.join(".complete"), b"")
                .with_context(|| format!("Failed to mark {} complete", c.label))
        });

        let mut st = shared.state.lock().unwrap();
        match result {
            Ok(()) => {
                st.targets_done += 1;
                debug!("Complete: {}", c.label);
            }
            Err(e) => {
                warn!("{e:#}");
                st.failed.push(format!("{}: {e:#}", c.label));
            }
        }
    }
}

/// Account for a finished job: link its file into every version that waited on it and
/// complete the versions that now have everything.
fn finish_job(shared: &Shared, jid: usize, result: Result<()>) {
    let mut completions = vec![];

    let links: Vec<(usize, PathBuf, PathBuf)> = {
        let mut st = shared.state.lock().unwrap();
        st.active -= 1;

        if let Err(e) = result {
            let label = st.targets[st.jobs[jid].owner].label.clone();
            st.failed.push(format!("{label}: {e:#}"));
            warn!("Failed: {label}: {e:#}");
            return;
        }
        st.jobs_done += 1;

        let owner = st.jobs[jid].owner;
        let waiters = std::mem::take(&mut st.jobs[jid].waiters);
        let big = match &st.jobs[jid].kind {
            Kind::Big(fi) => Some(*fi),
            Kind::Bulk(files) => {
                let n = files.len();
                completions.extend(st.landed(owner, n));
                None
            }
        };

        match big {
            None => vec![],
            Some(fi) => {
                let src = st.targets[owner].dest.join(st.targets[owner].fw.files[fi].path());
                completions.extend(st.landed(owner, 1));

                waiters
                    .into_iter()
                    .map(|(wt, wf)| {
                        let dst = st.targets[wt].dest.join(st.targets[wt].fw.files[wf].path());
                        (wt, src.clone(), dst)
                    })
                    .collect()
            }
        }
    };

    let results: Vec<_> = links
        .into_iter()
        .map(|(wt, src, dst)| (wt, hard_link_into(&src, &dst)))
        .collect();
    {
        let mut st = shared.state.lock().unwrap();
        for (wt, r) in results {
            match r {
                Ok(()) => completions.extend(st.landed(wt, 1)),
                Err(e) => {
                    let label = st.targets[wt].label.clone();
                    st.failed.push(format!("{label}: {e:#}"));
                }
            }
        }
    }

    mark_complete(shared, completions);
}

/// Setup that doesn't gate the downloads: write each version's manifest, then link in
/// the files the archive already holds. Runs beside the workers because the archive
/// filesystem may be slow, and the scratch tiers don't depend on it.
fn prepare(shared: &Shared, links: Vec<(usize, PathBuf, PathBuf)>, threads: usize) {
    let n = shared.state.lock().unwrap().targets.len();
    let (next_target, next_link) = (AtomicUsize::new(0), AtomicUsize::new(0));

    // Each operation can wait seconds on a loaded array, so run many at once; they
    // touch different directories and files.
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let ti = next_target.fetch_add(1, Ordering::Relaxed);
                    if ti >= n || shared.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let todo = {
                        let st = shared.state.lock().unwrap();
                        let t = &st.targets[ti];
                        (t.remaining > 0 && !t.ver_written)
                            .then(|| (t.dest.clone(), t.car.clone(), t.fw.clone()))
                    };
                    if let Some((dest, car, fw)) = todo {
                        match write_ver(&dest, &car, &fw) {
                            Ok(()) => shared.state.lock().unwrap().targets[ti].ver_written = true,
                            Err(e) => warn!("{e:#}"),
                        }
                    }
                    shared.prep_done.fetch_add(1, Ordering::Relaxed);
                }

                loop {
                    let li = next_link.fetch_add(1, Ordering::Relaxed);
                    if li >= links.len() || shared.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let (ti, src, dst) = &links[li];
                    let completion = match hard_link_into(src, dst) {
                        Ok(()) => shared.state.lock().unwrap().landed(*ti, 1),
                        Err(e) => {
                            warn!("{e:#}");
                            None
                        }
                    };
                    mark_complete(shared, completion.into_iter().collect());
                    shared.prep_done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
}

async fn run_job(shared: &Shared, client: &Arc<NuClient>, jid: usize) -> Result<()> {
    let (car, fw, dest, name, need) = {
        let st = shared.state.lock().unwrap();
        let job = &st.jobs[jid];
        let t = &st.targets[job.owner];
        let files: Vec<FileInfo> = match &job.kind {
            Kind::Big(f) => vec![t.fw.files[*f].clone()],
            Kind::Bulk(fs) => fs.iter().map(|&i| t.fw.files[i].clone()).collect(),
        };

        (
            t.car.clone(),
            FirmwareInfo { files, ..(*t.fw).clone() },
            t.dest.clone(),
            job.name.clone(),
            job.need,
        )
    };

    // The first tier with room; none: straight into the archive.
    let tier = shared.tiers.claim(need);
    let (out, move_to) = match tier {
        Some(i) => (
            shared.tiers.tiers[i].path.join(&name),
            Some(MoveConfig {
                dir: dest.clone(),
                hash_log: None,
                concurrency: 2,
            }),
        ),
        None => (dest.clone(), None),
    };

    let result = async {
        fs::create_dir_all(&out).with_context(|| format!("Failed to create {out:?}"))?;
        let dir = Dir::open_ambient_dir(&out, ambient_authority())
            .with_context(|| format!("Failed to open {out:?}"))?;
        let (downloader, mut rx) = Downloader::new(
            dir,
            client.clone(),
            (*car).clone(),
            fw,
            shared.concurrency,
            shared.retries,
            false,
            None,
            move_to,
        );
        let downloader = downloader.without_version_file();
        let handle = downloader.download();
        tokio::pin!(handle);

        loop {
            tokio::select! {
                r = &mut handle => break r,
                m = rx.recv() => {
                    if let Some(ProgressMessage::Download(b)) = m {
                        shared.bytes.fetch_add(b, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    .await;

    if let Some(i) = tier {
        shared.tiers.release(i, need);
        // A failed stage is kept on disk-backed tiers so the retry resumes; RAM is
        // always freed.
        if result.is_ok() || shared.tiers.tiers[i].ram {
            let _ = fs::remove_dir_all(&out);
        }
    }

    result
}

async fn worker(shared: Arc<Shared>, client: Arc<NuClient>) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let jid = {
            let mut st = shared.state.lock().unwrap();
            let Some(jid) = st.queue.pop_front() else {
                return;
            };
            st.active += 1;
            jid
        };

        let result = run_job(&shared, &client, jid).await;
        // Links and markers touch the archive filesystem, so keep them off the async
        // threads.
        let s = shared.clone();
        let _ = tokio::task::spawn_blocking(move || finish_job(&s, jid, result)).await;
    }
}

/// Hold `<root>/.sync.lock` for the life of the returned file, so two syncs of the
/// same archive can't run at once.
#[cfg(unix)]
fn lock(root: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd;

    let path = root.join(".sync.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open {path:?}"))?;
    // SAFETY: the descriptor is valid for as long as `file` is.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another sync of {root:?} is already running");
    }

    Ok(file)
}

#[cfg(not(unix))]
fn lock(root: &Path) -> Result<fs::File> {
    let path = root.join(".sync.lock");

    fs::File::create(&path).with_context(|| format!("Failed to create {path:?}"))
}

fn gb(bytes: u64) -> u64 {
    bytes / 1_000_000_000
}

fn report(plan: &Plan, root: &Path) -> Option<u64> {
    let free = available_space(root);
    eprintln!(
        "plan: {} versions to do ({} already complete); {} GB listed; {} GB to download, \
         {} GB shared or already archived; {} jobs; {} GB free",
        plan.targets.len(),
        plan.complete_targets,
        gb(plan.listed),
        gb(plan.to_download),
        gb(plan.to_link),
        plan.jobs.len(),
        free.map_or_else(|| "?".to_owned(), |f| gb(f).to_string()),
    );

    free
}

pub async fn run(cli: &SyncCli) -> Result<()> {
    let root = cli.root.clone();
    if !root.is_dir() {
        if cli.plan {
            bail!("Archive directory does not exist: {root:?}");
        }
        fs::create_dir_all(&root).with_context(|| format!("Failed to create {root:?}"))?;
    }

    // Planning only reads, so it can run beside a live sync; a real run takes the lock.
    let _lock = if cli.plan { None } else { Some(lock(&root)?) };

    let plan = make_plan(cli, &root).await?;
    let free = report(&plan, &root);
    if cli.plan {
        return Ok(());
    }

    if let Some(free) = free
        && plan.to_download + cli.min_free_gb * GIB > free
        && !cli.ignore_space
    {
        bail!(
            "Not enough space: leaves under {} GiB free. Narrow --brand/--region or \
             pass --ignore-space.",
            cli.min_free_gb,
        );
    }

    let Plan {
        targets,
        jobs,
        links,
        ..
    } = plan;

    // Versions with every file already in place just lack their marker (an earlier
    // run stopped at the very end).
    let ready: Vec<Completion> = targets
        .iter()
        .filter(|t| t.remaining == 0)
        .map(|t| Completion {
            label: t.label.clone(),
            car: t.car.clone(),
            fw: t.fw.clone(),
            dest: t.dest.clone(),
            ver_written: false,
        })
        .collect();
    // Where each file the archive already holds goes.
    let link_ops: Vec<(usize, PathBuf, PathBuf)> = links
        .into_iter()
        .map(|(ti, fi, src)| {
            (ti, src, targets[ti].dest.join(targets[ti].fw.files[fi].path()))
        })
        .collect();

    let tiers = Tiers::new(&cli.stage, cli.headroom_gb * GIB)?;
    let total_jobs = jobs.len();
    let prep_total = (targets.len() + link_ops.len()) as u64;
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: (0..jobs.len()).collect(),
            jobs,
            active: 0,
            failed: vec![],
            targets_done: 0,
            jobs_done: 0,
            targets,
        }),
        tiers,
        bytes: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        concurrency: cli.concurrency.0.into(),
        retries: cli.retries,
        prep_done: AtomicU64::new(0),
        prep_total,
    });

    // Setup runs beside the workers, so a slow archive filesystem can't hold up
    // the downloads (which go to the scratch tiers first).
    let setup_threads = cli.setup_threads;
    let prep = {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || {
            mark_complete(&shared, ready);
            prepare(&shared, link_ops, setup_threads);
        })
    };

    // One client per worker, each through its own proxy when there are proxies.
    let mut set = JoinSet::new();
    for i in 0..cli.workers.max(1) {
        let proxies: Vec<String> = cli
            .network
            .proxy
            .get(i % cli.network.proxy.len().max(1))
            .cloned()
            .into_iter()
            .collect();
        let client = Arc::new(build_client(&cli.network, proxies)?);
        set.spawn(worker(shared.clone(), client));
    }

    let status = {
        let shared = shared.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            loop {
                let (jobs_done, active, targets_done) = {
                    let st = shared.state.lock().unwrap();
                    (st.jobs_done, st.active, st.targets_done)
                };
                let bytes = shared.bytes.load(Ordering::Relaxed);
                let secs = start.elapsed().as_secs_f64().max(1.0);
                let prep_done = shared.prep_done.load(Ordering::Relaxed);
                let prep = if prep_done < shared.prep_total {
                    format!(", setup {prep_done}/{}", shared.prep_total)
                } else {
                    String::new()
                };
                eprintln!(
                    "sync: {jobs_done}/{total_jobs} jobs, {active} active, {targets_done} \
                     versions complete, {} GB downloaded ({:.0} MB/s){prep}",
                    gb(bytes),
                    bytes as f64 / secs / 1e6,
                );
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        })
    };

    let mut prep = prep;
    let interrupted = tokio::select! {
        _ = ctrl_c() => true,
        _ = async {
            while set.join_next().await.is_some() {}
            // Setup may outlast the downloads (a slow archive filesystem).
            let _ = (&mut prep).await;
        } => false,
    };
    status.abort();
    if interrupted {
        shared.stop.store(true, Ordering::Relaxed);
        set.abort_all();
        eprintln!("Interrupted. Rerun the same command to resume.");
        // Leave now. Returning would make the runtime wait for blocking work already
        // in flight (copies onto a slow array, setup), which can take minutes, and
        // further Ctrl-C would do nothing. Everything here resumes: copies go through
        // `.part`, partial downloads stay in the stage, and the lock is released on
        // exit.
        std::process::exit(130);
    }

    let st = shared.state.lock().unwrap();
    eprintln!(
        "sync: done. {} versions completed, {} GB downloaded, {} failures",
        st.targets_done,
        gb(shared.bytes.load(Ordering::Relaxed)),
        st.failed.len(),
    );
    if !st.failed.is_empty() {
        for f in &st.failed {
            eprintln!("  {f}");
        }
        bail!("{} downloads failed; rerun to retry", st.failed.len());
    }

    Ok(())
}
