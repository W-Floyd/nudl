// SPDX-FileCopyrightText: 2024-2026 Andrew Gunnerson
// SPDX-License-Identifier: GPL-3.0-only

use std::{fmt, path::PathBuf, str::FromStr};

use anyhow::bail;
use clap::{Args, Parser, Subcommand, ValueEnum, builder::Styles};
use tracing::Level;

use crate::Selector;

const MAX_CONCURRENCY: u8 = 16;

#[derive(Clone, Copy, Debug)]
pub struct Concurrency(pub u8);

impl FromStr for Concurrency {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let n: u8 = s.parse()?;
        if n == 0 {
            bail!("value cannot be 0");
        } else if n > MAX_CONCURRENCY {
            // Same limit as aria2 to avoid unintentional DoS
            bail!("concurrency too high (>{MAX_CONCURRENCY})");
        }

        Ok(Self(n))
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Brand {
    Hyundai,
    Kia,
    Genesis,
}

impl Brand {
    pub fn as_code_str(self) -> &'static str {
        match self {
            Self::Hyundai => "HM",
            Self::Kia => "KM",
            Self::Genesis => "GN",
        }
    }

    pub fn as_pretty_str(self) -> &'static str {
        match self {
            Self::Hyundai => "Hyundai",
            Self::Kia => "Kia",
            Self::Genesis => "Genesis",
        }
    }
}

impl FromStr for Brand {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "HM" => Ok(Self::Hyundai),
            "KM" => Ok(Self::Kia),
            "GN" => Ok(Self::Genesis),
            m => Err(m.to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    JsonRaw,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_possible_value().ok_or(fmt::Error)?.get_name())
    }
}

#[derive(Debug, Args)]
pub struct FamilyGroup {
    /// Car brand.
    #[arg(short, long)]
    pub brand: Brand,

    /// Car region.
    ///
    /// This is autodetected (likely by GeoIP) if unspecified.
    #[arg(short, long)]
    pub region: Option<String>,
}

#[derive(Debug, Args)]
pub struct FirmwareSelectorGroup {
    /// Select firmware by car model ID.
    ///
    /// The same car model ID may be used for multiple variants, like HEV vs.
    /// PHEV. To disambiguate, use `--version`.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Select firmware by marketing name.
    #[arg(short, long)]
    pub name: Option<String>,

    /// Select firmware by version number.
    #[arg(short, long)]
    pub version: Option<String>,
}

impl FirmwareSelectorGroup {
    pub fn to_selectors(&self) -> Vec<Selector> {
        let mut selectors = vec![];

        if let Some(model) = &self.model {
            selectors.push(Selector::Model(model.clone()));
        }
        if let Some(name) = &self.name {
            selectors.push(Selector::Name(name.clone()));
        }
        if let Some(version) = &self.version {
            selectors.push(Selector::Version(version.clone()));
        }

        selectors
    }
}

#[derive(Debug, Args)]
pub struct NetworkGroup {
    /// Ignore TLS certificate validation for HTTPS connections.
    #[arg(long)]
    pub ignore_tls_validation: bool,

    /// Directory for cached car lists and firmware manifests.
    ///
    /// Defaults to `nudl` under the user cache directory ($XDG_CACHE_HOME, else
    /// ~/.cache, or ~/Library/Caches on macOS).
    #[arg(long, value_name = "DIR", env = "NUDL_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Do not read or write the cache.
    #[arg(long)]
    pub no_cache: bool,

    /// Send requests through a proxy: http://, https://, socks5:// or socks5h://.
    ///
    /// May be repeated (or comma-separated in NUDL_PROXY); requests then rotate
    /// through the proxies, which spreads a download's connections across them.
    /// Credentials in the URL are never logged.
    #[arg(long, value_name = "URL", env = "NUDL_PROXY", value_delimiter = ',')]
    pub proxy: Vec<String>,

    /// Seconds a cached car list stays valid. 0 refreshes it.
    #[arg(long, value_name = "SECONDS", default_value_t = 21_600)]
    pub cache_ttl_list: u64,

    /// Seconds a cached firmware manifest stays valid. 0 refreshes it.
    ///
    /// A manifest is addressed by a per-version code, so it rarely changes.
    #[arg(long, value_name = "SECONDS", default_value_t = 2_592_000)]
    pub cache_ttl_manifest: u64,
}

/// Default cache location, if a home directory can be found.
pub fn default_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
            Some(if cfg!(target_os = "macos") {
                home.join("Library/Caches")
            } else {
                home.join(".cache")
            })
        })?;

    Some(base.join("nudl"))
}

fn list_additional_help() -> String {
    let styles = Styles::styled();
    let h = styles.get_header();
    let l = styles.get_literal();
    let p = styles.get_placeholder();

    format!(
        "{h}Help:{h:#}

  To list the available firmware versions, run:

    {l}nudl{l:#} {l}list{l:#} {l}-b{l:#} {p}<brand>{p:#}

  The first column contains the model ID. To download the firmware, run:

    {l}nudl{l:#} {l}download{l:#} {l}-b{l:#} {p}<brand>{p:#} {l}-m{l:#} {p}<model>{p:#}

  For more information about download-related options, run:

    {l}nudl{l:#} {l}download{l:#} {l}--help{l:#}
"
    )
}

/// List available firmware.
#[derive(Debug, Parser)]
#[command(after_help = list_additional_help())]
pub struct ListCli {
    #[command(flatten)]
    pub family: FamilyGroup,

    #[command(flatten)]
    pub selector: FirmwareSelectorGroup,

    /// Data output format.
    ///
    /// `text`: Two columns with the model ID and firmware version.
    /// `json`: Normalized data from the server containing more information.
    /// `json-raw`: Raw data from the server.
    #[arg(short, long, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    #[command(flatten)]
    pub network: NetworkGroup,
}

fn download_additional_help() -> String {
    let styles = Styles::styled();
    let h = styles.get_header();
    let l = styles.get_literal();
    let p = styles.get_placeholder();

    format!("{h}Help:{h:#}

  Specify {l}-m{l:#}/{l}--model{l:#} to select the vehicle model. For example:

    {l}nudl{l:#} {l}download{l:#} {l}-b{l:#} {p}<brand>{p:#} {l}-m{l:#} {p}<model>{p:#} {l}-o{l:#} {p}<output directory>{p:#}

  If there are multiple firmware variants for a single model (eg. HEV vs. PHEV),
  the command will fail and print out the list of available downloads as well as
  the additional command line options needed to select a specific one.

  To list the available models, run:

    {l}nudl{l:#} {l}list{l:#} {l}-b{l:#} {p}<brand>{p:#}
")
}

/// Download firmware.
#[derive(Debug, Parser)]
#[command(after_help = download_additional_help())]
pub struct DownloadCli {
    #[command(flatten)]
    pub family: FamilyGroup,

    #[command(flatten)]
    pub selector: FirmwareSelectorGroup,

    /// Output directory.
    #[arg(short, long, value_parser, default_value = ".")]
    pub output: PathBuf,

    /// Download and post-processing concurrency.
    ///
    /// The maximum concurrency allowed is 16.
    #[arg(short, long, default_value = "4")]
    pub concurrency: Concurrency,

    /// Maximum retries during download.
    #[arg(long, default_value = "3")]
    pub retries: u8,

    /// Keep raw unextracted files.
    #[arg(short, long)]
    pub keep_raw: bool,

    /// Write the `<id>.ver` manifest and exit without downloading.
    #[arg(long)]
    pub manifest_only: bool,

    /// Hardlink files already downloaded elsewhere instead of fetching them.
    ///
    /// Every `.ver` file under DIR is read, and any file of this firmware with
    /// the same CRC32 and size is linked into the output directory. May be
    /// repeated. DIR must be on the same filesystem as the output.
    #[arg(long, value_name = "DIR")]
    pub link_from: Vec<PathBuf>,

    /// Skip files that already exist in DIR with the expected size.
    ///
    /// For staging: download into a fast scratch output directory while the
    /// finished files live in DIR (possibly on another filesystem). Skipped
    /// files are neither downloaded nor written to the output directory.
    #[arg(long, value_name = "DIR")]
    pub present_in: Option<PathBuf>,

    /// Move each file to DIR as soon as it is verified.
    ///
    /// The output directory then acts as scratch space (a ramdisk or SSD) and DIR
    /// is the final destination, which may be on another filesystem. Files are
    /// copied through `<name>.part` and deleted from the output directory once
    /// in place. Combine with `--present-in DIR` so a resumed run skips them.
    #[arg(long, value_name = "DIR")]
    pub move_to: Option<PathBuf>,

    /// With `--move-to`, append `sha256<TAB>path` for each moved file to FILE.
    ///
    /// The hash is computed during the copy, so the destination is not read again.
    #[arg(long, value_name = "FILE", requires = "move_to")]
    pub hash_log: Option<PathBuf>,

    /// With `--move-to`, how many files are moved at once.
    #[arg(long, value_name = "N", default_value_t = 2, requires = "move_to")]
    pub move_concurrency: usize,

    /// Smallest file, in bytes, that `--link-from` will link.
    #[arg(long, value_name = "BYTES", default_value_t = 1_048_576)]
    pub link_min_size: u64,

    #[command(flatten)]
    pub network: NetworkGroup,
}

fn verify_additional_help() -> String {
    let styles = Styles::styled();
    let h = styles.get_header();

    format!(
        "{h}Help:{h:#}

  This subcommand is normally not needed because checksums are automatically
  verified during the download process. However, it can be useful for verifying
  checksums after the files have been copied elsewhere, for example to a USB
  drive.

  The vehicle itself will also verify checksums and digital signatures, so there
  is minimal risk of corrupted files causing issues.
"
    )
}

/// Verify CRC32 of existing firmware.
#[derive(Debug, Parser)]
#[command(after_help = verify_additional_help())]
pub struct VerifyCli {
    /// Firmware directory.
    #[arg(short, long, value_parser, default_value = ".")]
    pub directory: PathBuf,

    /// Verification concurrency.
    ///
    /// The maximum concurrency allowed is 16.
    #[arg(short, long, default_value = "4")]
    pub concurrency: Concurrency,
}

/// Keep an archive of firmware up to date.
///
/// Plans every pending firmware across brands and regions, downloads each distinct
/// file once (files shared between versions are hardlinked), and keeps the
/// download workers busy by scheduling per file, not per version. Safe to re-run
/// or interrupt: finished versions are marked with `.complete`, partial files resume.
#[derive(Debug, Parser)]
pub struct SyncCli {
    /// Archive directory. Layout: `<brand>/<region>/<model id>/<version>/`.
    #[arg(long, value_name = "DIR")]
    pub root: PathBuf,

    /// Brands to sync. Default: all.
    #[arg(short, long)]
    pub brand: Vec<Brand>,

    /// Regions to sync. Default: all known regions.
    #[arg(short, long)]
    pub region: Vec<String>,

    /// Model IDs to sync (as `nudl list` shows them). Default: all.
    #[arg(short, long, value_name = "ID")]
    pub model: Vec<String>,

    /// Scratch tier for downloads, as `PATH` or `PATH:CAP_GIB`. May be repeated.
    ///
    /// Tried in order: a file is downloaded into the first tier with room for it
    /// (capacity, free space, and for tmpfs the available RAM), so later tiers only
    /// see overflow. Each finished file is moved into the archive at once. When no
    /// tier fits, the file downloads straight into the archive.
    #[arg(long, value_name = "PATH[:CAP_GIB]")]
    pub stage: Vec<String>,

    /// Files downloading at once. Workers take the proxies in turn.
    #[arg(long, value_name = "N", default_value_t = 4)]
    pub workers: usize,

    /// Downloaded jobs moved from scratch into the archive at once. Downloads don't
    /// wait for these, so a slow archive only makes the scratch tiers fill.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub move_workers: usize,

    /// Connections per file.
    ///
    /// The maximum concurrency allowed is 16.
    #[arg(short, long, default_value = "8")]
    pub concurrency: Concurrency,

    /// Maximum retries during download.
    #[arg(long, default_value = "3")]
    pub retries: u8,

    /// Keep this much free on a stage tier (and of RAM, for tmpfs), in GiB.
    #[arg(long, value_name = "GIB", default_value_t = 16)]
    pub headroom_gb: u64,

    /// Refuse to start if the new data would leave less than this free in the
    /// archive, in GiB.
    #[arg(long, value_name = "GIB", default_value_t = 200)]
    pub min_free_gb: u64,

    /// Start even if the archive filesystem would run low on space.
    #[arg(long)]
    pub ignore_space: bool,

    /// Plan and report what would be downloaded, then stop.
    #[arg(long)]
    pub plan: bool,

    /// Threads writing manifests and linking files already archived, in the
    /// background. More hide the latency of a slow archive filesystem.
    #[arg(long, value_name = "N", default_value_t = 32)]
    pub setup_threads: usize,

    /// Manifests fetched at once while planning.
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub plan_jobs: usize,

    /// Files smaller than this, in bytes, are fetched in one batch per version
    /// instead of one job each. They are still shared between versions: hardlinked
    /// from an existing copy, or from the first version that fetches them.
    #[arg(long, value_name = "BYTES", default_value_t = 1_048_576)]
    pub link_min_size: u64,

    /// Skip the zips linked from update.kia.com notices.
    ///
    /// Those are the only source for firmware the NU API doesn't list (the Niro
    /// Plus taxi's); they go in `<root>/notices/<notice id>/`. Also skipped with
    /// `--model`, any brand but kia, or any region but KR.
    #[arg(long)]
    pub no_notices: bool,

    /// Skip Hyundai Mobis's open-source packages (mobis.com/en/tech/rnd.do).
    ///
    /// Source and licence-notice zips per head unit platform; they go in
    /// `<root>/mobis/`, see `nudl mobis`. Also skipped with `--model`.
    #[arg(long)]
    pub no_mobis: bool,

    #[command(flatten)]
    pub network: NetworkGroup,
}

/// List the zip downloads linked from update.kia.com notices.
///
/// These are firmware the NU API doesn't offer (the Niro Plus taxi's). Prints the
/// notice URL and the zip URL; `sync` downloads them, as does `--download`.
#[derive(Debug, Parser)]
pub struct NoticesCli {
    /// Download the zips missing from `<DIR>/notices/<notice id>/` instead of listing.
    #[arg(long, value_name = "DIR")]
    pub download: Option<PathBuf>,

    /// Parallel ranges per large zip, spread over the proxies.
    ///
    /// The maximum concurrency allowed is 16.
    #[arg(short, long, default_value = "8")]
    pub concurrency: Concurrency,

    /// Maximum retries during download.
    #[arg(long, default_value = "3")]
    pub retries: u8,

    #[command(flatten)]
    pub network: NetworkGroup,
}

/// List Hyundai Mobis's open-source packages, or download them.
///
/// One row per model and platform: the model, the platform, and the source and
/// licence-notice zip names. Many models share a platform, so `--download` fetches
/// each distinct file once into `<DIR>/mobis/<file id>/`, with an `index.tsv`.
#[derive(Debug, Parser)]
pub struct MobisCli {
    /// Download the files missing from `<DIR>/mobis/` instead of listing.
    #[arg(long, value_name = "DIR")]
    pub download: Option<PathBuf>,

    /// Files downloading at once. The server can't split a file into ranges.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub workers: usize,

    /// Maximum retries per file.
    #[arg(long, default_value = "3")]
    pub retries: u8,

    #[command(flatten)]
    pub network: NetworkGroup,
}

/// Replace duplicate files in an archive with hardlinks.
///
/// Files with the same CRC32 and size (from each version's `.ver`) are the same
/// file. Those held in separate inodes are replaced by hardlinks to one copy, which
/// frees the space and the inodes. Only finished versions are touched. Takes the
/// same lock as `sync`, so it can't run beside one. Dry run unless `--apply`.
#[derive(Debug, Parser)]
pub struct RelinkCli {
    /// Archive directory. Layout: `<brand>/<region>/<model id>/<version>/`.
    #[arg(long, value_name = "DIR")]
    pub root: PathBuf,

    /// Do it. Without this, only report what would change.
    #[arg(long)]
    pub apply: bool,

    /// Compare the contents of each file with the copy it will link to before
    /// replacing it, so a CRC32 collision can't merge two different files. Reads both
    /// copies, which is slow on a large archive.
    #[arg(long)]
    pub verify: bool,

    /// Ignore files smaller than this, in bytes.
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub min_size: u64,

    /// Threads running the filesystem work. Each operation can wait on a slow disk.
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub threads: usize,
}

/// Write the archive index: which files are the same inode.
///
/// `<root>/.nudl-index.gz` lets tools that relate firmware packages to one another
/// (which share storage, what deleting one would free) skip stat-ing the archive.
/// `sync` keeps it current for the versions it finishes and `relink` refreshes it, so
/// this is for making it the first time or rebuilding it after the archive was changed
/// by something else. It stats every file once.
#[derive(Debug, Parser)]
pub struct IndexCli {
    /// Archive directory. Layout: `<brand>/<region>/<model id>/<version>/`.
    #[arg(long, value_name = "DIR")]
    pub root: PathBuf,

    /// Threads running the stats. Each can wait on a slow disk.
    #[arg(long, value_name = "N", default_value_t = 32)]
    pub threads: usize,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    List(ListCli),
    Download(DownloadCli),
    Verify(VerifyCli),
    Sync(SyncCli),
    Notices(NoticesCli),
    Mobis(MobisCli),
    Relink(RelinkCli),
    Index(IndexCli),
}

fn cli_additional_help() -> String {
    let styles = Styles::styled();
    let h = styles.get_header();
    let l = styles.get_literal();
    let p = styles.get_placeholder();

    format!(
        "{h}Help:{h:#}

  For additional information about a subcommand, run:

    {l}nudl{l:#} {p}<COMMAND>{p:#} {l}--help{l:#}
"
    )
}

#[derive(Debug, Parser)]
#[command(version, after_help = cli_additional_help())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Lowest log message severity to output.
    #[arg(long, global = true, value_name = "LEVEL", default_value_t = Level::INFO)]
    pub log_level: Level,
}
