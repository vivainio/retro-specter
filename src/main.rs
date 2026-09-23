mod analyze;
mod complexity;
mod git;
mod pr;
mod report;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use complexity::Language;
use rayon::prelude::*;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

/// Walk back through merged PRs in git history and measure the
/// indentation-based complexity each one added or removed (C#, Python).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Revision to start walking back from.
    #[arg(default_value = "HEAD")]
    rev: String,

    /// Path to the git repository.
    #[arg(short = 'C', long, default_value = ".")]
    repo: PathBuf,

    /// Maximum number of PRs to analyze (newest first).
    #[arg(short = 'n', long)]
    max_count: Option<usize>,

    /// Only PRs merged after this date (anything `git log --since` accepts).
    #[arg(long)]
    since: Option<String>,

    /// Only PRs merged before this date.
    #[arg(long)]
    until: Option<String>,

    /// Which first-parent commits count as PRs.
    #[arg(long, value_enum, default_value_t = Mode::Prs)]
    mode: Mode,

    #[arg(short, long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Show per-file rows (table) or one row per file (csv).
    #[arg(long)]
    files: bool,

    /// Only analyze files under this path prefix (repeatable).
    #[arg(long = "path", value_name = "PREFIX")]
    paths: Vec<String>,

    /// Exclude files matching this git pathspec glob (repeatable).
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// Don't exclude generated files (*.Designer.cs, *.g.cs, *_pb2.py, ...).
    #[arg(long)]
    no_default_excludes: bool,

    /// Also list PRs that changed no analyzable files.
    #[arg(long)]
    include_empty: bool,

    /// Columns per tab when measuring indentation.
    #[arg(long, default_value_t = 4)]
    tab_width: usize,

    /// Skip files larger than this many bytes.
    #[arg(long, default_value_t = 1_000_000)]
    max_file_bytes: usize,

    /// Number of entries in the summary top lists (table format; 0 = no summary).
    #[arg(long, default_value_t = 10)]
    top: usize,

    /// Worker threads (default: number of CPUs).
    #[arg(short, long)]
    jobs: Option<usize>,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Mode {
    /// Merge commits, plus commits whose message references a PR (squash merges).
    Prs,
    /// Only merge commits.
    Merges,
    /// Every commit on the first-parent chain.
    All,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
}

const DEFAULT_EXCLUDES: &[&str] = &[
    "*.Designer.cs",
    "*.g.cs",
    "*.g.i.cs",
    "*.generated.cs",
    "*.AssemblyInfo.cs",
    "*_pb2.py",
    "*_pb2_grpc.py",
];

fn main() {
    if let Err(e) = run() {
        if e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(j) = cli.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()?;
    }

    let repo = git::Repo::open(&cli.repo)?;

    let mut pathspecs: Vec<String> = Language::EXTENSIONS
        .iter()
        .map(|(ext, _)| format!("*.{ext}"))
        .collect();
    let excludes = cli.excludes.iter().map(String::as_str).chain(
        DEFAULT_EXCLUDES
            .iter()
            .copied()
            .filter(|_| !cli.no_default_excludes),
    );
    pathspecs.extend(excludes.map(|g| format!(":(exclude){g}")));

    let opts = analyze::Options {
        tab_width: cli.tab_width.max(1),
        max_file_bytes: cli.max_file_bytes,
        paths: cli.paths.clone(),
        pathspecs,
        empty_tree: repo.empty_tree()?,
    };

    let commits = repo.first_parent_log(&cli.rev, cli.since.as_deref(), cli.until.as_deref())?;
    let selected: Vec<(git::Commit, Option<u64>)> = commits
        .into_iter()
        .map(|c| {
            let n = pr::pr_number(&c);
            (c, n)
        })
        .filter(|(c, n)| match cli.mode {
            Mode::All => true,
            Mode::Merges => c.parents.len() > 1,
            Mode::Prs => c.parents.len() > 1 || n.is_some(),
        })
        .take(cli.max_count.unwrap_or(usize::MAX))
        .collect();

    let results: Vec<analyze::PrResult> = selected
        .par_iter()
        .map_init(
            || repo.cat_file(),
            |cat, (commit, n)| {
                let cat = cat.as_mut().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                analyze::analyze_commit(&repo, cat, commit, *n, &opts)
            },
        )
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|r| cli.include_empty || !r.files.is_empty())
        .collect();

    let mut out = BufWriter::new(io::stdout().lock());
    match cli.format {
        Format::Table => report::table(&mut out, &results, cli.files, cli.top)?,
        Format::Csv => report::csv(&mut out, &results, cli.files)?,
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, &results)?;
            writeln!(out)?;
        }
        Format::Jsonl => {
            for r in &results {
                serde_json::to_writer(&mut out, r)?;
                writeln!(out)?;
            }
        }
    }
    out.flush()?;
    Ok(())
}
