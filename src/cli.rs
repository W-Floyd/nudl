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

#[derive(Debug, Subcommand)]
pub enum Command {
    List(ListCli),
    Download(DownloadCli),
    Verify(VerifyCli),
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
