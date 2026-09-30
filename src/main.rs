use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use rayon::prelude::*;
use retro_specter::complexity::Language;
use retro_specter::repos::RepoArgs;
use retro_specter::{analyze, dump, git, report, usage};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

/// Retrospective analysis of merged PRs in git history.
#[derive(Parser)]
#[command(version, arg_required_else_help = true)]
struct Cli {
    /// Worker threads (default: number of CPUs).
    #[arg(short, long, global = true)]
    jobs: Option<usize>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure the indentation-based complexity each PR added or removed (C#, Python).
    Complexity(ComplexityArgs),
    /// Write the raw dump: one JSON object per PR and per commit, with which PR each commit
    /// belongs to. Aggregators (`ai`, your own scripts) read this instead of git.
    Dump(DumpArgs),
    /// Aggregate AI usage (Co-Authored-By trailers, "Generated with" lines) and lines
    /// added/removed from dump files. Never touches git.
    Ai(AiArgs),
}

#[derive(Args)]
struct DumpArgs {
    #[command(flatten)]
    repos: RepoArgs,

    /// Write one `<repo>.jsonl` per repository into this directory (created if needed).
    /// Without it, a single repository is written to stdout; several need this.
    #[arg(short, long, value_name = "DIR")]
    out_dir: Option<PathBuf>,
}

#[derive(Args)]
struct AiArgs {
    /// Dump files, or directories of `*.jsonl` (default: read stdin).
    files: Vec<PathBuf>,

    #[arg(short, long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Aggregate per merged PR (models of all its commits combined, most commits first;
    /// commits outside any PR are reported separately) or per individual commit.
    #[arg(long, value_enum, default_value_t = usage::Unit::Pr)]
    by: usage::Unit,

    /// List every row (table format) instead of only the summary.
    #[arg(long)]
    list: bool,
}

#[derive(Args)]
struct ComplexityArgs {
    #[command(flatten)]
    repos: RepoArgs,

    #[arg(short, long, value_enum, default_value_t = Format::Table)]
    format: Format,

    /// Show per-file rows (table) or one row per file (csv).
    #[arg(long)]
    files: bool,

    /// Show added/removed/modified functions and classes under each file (table),
    /// or one row per changed function (csv). Implies --files.
    #[arg(long)]
    functions: bool,

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
    match cli.command {
        Command::Complexity(args) => complexity(&args),
        Command::Dump(args) => dump_cmd(&args),
        Command::Ai(args) => ai(&args),
    }
}

fn dump_cmd(cli: &DumpArgs) -> Result<()> {
    let repos = cli.repos.open_repos()?;
    let Some(dir) = &cli.out_dir else {
        anyhow::ensure!(
            repos.len() == 1,
            "{} repositories found; pass --out-dir to write one file per repository",
            repos.len()
        );
        let records = dump::dump_repo(&repos[0], &cli.repos)?;
        let mut out = BufWriter::new(io::stdout().lock());
        dump::write_lines(&mut out, &records)?;
        return Ok(out.flush()?);
    };
    let mut used = std::collections::HashSet::new();
    for repo in &repos {
        let file = dump::file_name(repo, &mut used);
        // A failing repository leaves its previous dump untouched.
        match dump::dump_repo(repo, &cli.repos).and_then(|r| {
            dump::write_file(dir, &file, &r)?;
            Ok(r.len())
        }) {
            Ok(n) => eprintln!(
                "{}: {n} records -> {}",
                repo.name(),
                dir.join(&file).display()
            ),
            Err(e) if repos.len() > 1 => eprintln!("skipping {}: {e:#}", repo.name()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn ai(cli: &AiArgs) -> Result<()> {
    let records = dump::read(&cli.files)?;
    let all = usage::rows(&records, cli.by);

    let mut out = BufWriter::new(io::stdout().lock());
    match cli.format {
        Format::Table => usage::table(&mut out, &all, cli.by, cli.list)?,
        Format::Csv => usage::csv(&mut out, &all)?,
        Format::Json => {
            serde_json::to_writer_pretty(&mut out, &all)?;
            writeln!(out)?;
        }
        Format::Jsonl => {
            for r in &all {
                serde_json::to_writer(&mut out, r)?;
                writeln!(out)?;
            }
        }
    }
    out.flush()?;
    Ok(())
}

fn complexity(cli: &ComplexityArgs) -> Result<()> {
    let repos = cli.repos.open_repos()?;

    let mut results = Vec::new();
    for repo in &repos {
        match analyze_repo(cli, repo) {
            Ok(r) => results.extend(r),
            Err(e) if repos.len() > 1 => eprintln!("skipping {}: {e:#}", repo.name()),
            Err(e) => return Err(e),
        }
    }

    let detail = if cli.functions {
        report::Detail::Function
    } else if cli.files {
        report::Detail::File
    } else {
        report::Detail::Pr
    };
    let mut out = BufWriter::new(io::stdout().lock());
    match cli.format {
        Format::Table => report::table(&mut out, &results, detail, cli.top)?,
        Format::Csv => report::csv(&mut out, &results, detail)?,
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

fn analyze_repo(cli: &ComplexityArgs, repo: &git::Repo) -> Result<Vec<analyze::PrResult>> {
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

    let selected = cli.repos.select(repo)?;

    Ok(selected
        .par_iter()
        .map_init(
            || repo.cat_file(),
            |cat, (commit, n)| {
                let cat = cat.as_mut().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                analyze::analyze_commit(repo, cat, commit, *n, &opts)
            },
        )
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|r| cli.include_empty || !r.files.is_empty())
        .collect())
}
