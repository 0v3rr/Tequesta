//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
#[cfg(all(windows, feature = "diagnostics"))]
mod replay;
mod run;
mod shader;
mod translate;
#[cfg(windows)]
mod warm;

use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Tequesta (VIctory): run an Xbox Series title natively on Windows"
)]
struct Cli {
    /// Translate all RDNA2 shaders under GAME_DIR to DXIL in parallel and
    /// pre-compile available D3D12 PSOs into the pipeline library cache.
    #[arg(long, value_name = "GAME_DIR")]
    precompile_shaders: Option<PathBuf>,
    /// Worker threads for --precompile-shaders (default: every logical core).
    #[arg(long, value_parser = parse_jobs)]
    jobs: Option<usize>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found and show why paths were skipped.
        #[arg(short, long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    ///
    /// VICTORY_HOST_WAVE overrides the host wave width, including with --warm.
    /// VICTORY_HOST_MAX_WAVE is a compatibility alias. When both values are
    /// valid, VICTORY_HOST_WAVE takes precedence. Zero means unknown width.
    /// For ordinary translation, VICTORY_HOST_RDNA=1 selects RDNA float settings.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader.
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long, value_parser = parse_jobs)]
        jobs: Option<usize>,
        /// Warm the run's shader cache: a file of shader hashes (the 16
        /// hex digits a run's log names a shader by, anywhere in a line);
        /// the containers with those hashes are translated into the
        /// cache and nothing else is done. Set `VICTORY_HOST_WAVE` and
        /// `VICTORY_VIEW_LIMIT` to what the run's log says of the host.
        #[arg(long)]
        warm: Option<PathBuf>,
    },
    /// Translate every shader found (as `translate` finds them) into the
    /// shader cache, so that no run has to: a few minutes, once per version
    /// of the translator. Translations an earlier build of the same version
    /// cached are made again, and those that came out different counted.
    /// In diagnostics builds, VICTORY_LOG=debug shows skipped paths.
    Shaders {
        paths: Vec<PathBuf>,
        /// Worker threads (default: every logical core).
        #[arg(long, value_parser = parse_jobs)]
        jobs: Option<usize>,
        /// Also write every shader that does not translate to this file,
        /// one line each: hash, where it was found, why.
        #[arg(long)]
        list: Option<PathBuf>,
    },
    /// Make the host pipelines of every pipeline the title ever created
    /// (runs with `VICTORY_PIPELINE_BOOK=1` write them down), so that the
    /// driver's cache on disk has them before a run needs one: once per change
    /// of the translator or the driver. Goes on where it was stopped.
    ///
    /// Pipelines that crash or hang the compiler are listed in `warm-bad.txt`
    /// with the current fingerprint and retried automatically after a driver
    /// or translator change. Set `VICTORY_WARM_BAD=0` to ignore `warm-bad.txt`
    /// for this invocation.
    #[cfg(windows)]
    Warm {
        /// Processes at once (default: two thirds of the logical cores).
        #[arg(long, value_parser = parse_jobs)]
        jobs: Option<usize>,
        /// At most this many pipelines.
        #[arg(long)]
        limit: Option<usize>,
        /// Retry pipelines already made or listed in `warm-bad.txt` too.
        #[arg(long)]
        again: bool,
        /// Only the pipelines earlier runs drew with.
        #[arg(long)]
        used: bool,
        /// (What the processes it starts are given.)
        #[arg(long, hide = true)]
        these: Option<String>,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Run a captured frame's GPU work again (diagnostics builds; see
    /// docs/replay.md).
    #[cfg(all(windows, feature = "diagnostics"))]
    Replay(replay::Replay),
    /// Show the resolved configuration and check it.
    Config,
    /// Analyze a PE32+ image for static recompilation and optionally emit static patches.
    Recomp {
        file: PathBuf,
        /// Write a copy of the PE with safe static console-instruction patches applied.
        #[arg(long)]
        patch_out: Option<PathBuf>,
        /// List discovered console-instruction rewrite sites.
        #[arg(long)]
        list_sites: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn parse_jobs(value: &str) -> Result<usize, &'static str> {
    value
        .parse::<NonZeroUsize>()
        .map(NonZeroUsize::get)
        .map_err(|_| "expected a worker count of 1 or more")
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    if let Some(game_dir) = cli.precompile_shaders {
        return translate::precompile_shaders_and_pipelines(game_dir, cli.jobs);
    }
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        std::process::exit(2);
    };
    match command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            warm,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            let wave = translate::host_wave::select(
                std::env::var_os("VICTORY_HOST_WAVE").as_deref(),
                std::env::var_os("VICTORY_HOST_MAX_WAVE").as_deref(),
                |message| eprintln!("{message}"),
            );
            if let Some(file) = warm {
                return translate::warm(paths, &file, jobs, wave);
            }
            translate::run(paths, validate, pipelines, list, jobs, wave)
        }
        Command::Shaders { paths, jobs, list } => translate::precompile(paths, jobs, list),
        #[cfg(windows)]
        Command::Warm { jobs, limit, again, used, these } => warm::run(jobs, limit, again, used, these),
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args,
        } => {
            let tcfg = victory_core::TequestaConfig::load();
            tcfg.apply_startup_env();
            victory_hle::runtime::set_title_args(title_args);
            run::run(&exe, unimplemented, strict, execute, console)
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        Command::Replay(options) => replay::run(options),
        Command::Config => show_config(),
        Command::Recomp {
            file,
            patch_out,
            list_sites,
        } => run_recomp(&file, patch_out.as_deref(), list_sites),
    }
}

fn run_recomp(file: &PathBuf, patch_out: Option<&std::path::Path>, list_sites: bool) -> Result<()> {
    let bytes = std::fs::read(file)?;
    let plan = victory_loader::recomp::analyze(&bytes)?;
    println!("{}", plan.format_summary());
    if list_sites {
        for site in &plan.rewrite_sites {
            println!(
                "  rva={:#010x} offset={:#010x} len={} kind={:?} patchable={}",
                site.rva,
                site.file_offset,
                site.len,
                site.kind,
                site.patch.is_some()
            );
        }
    }
    if let Some(out_path) = patch_out {
        victory_core::ensure_outside_repo(out_path)?;
        let (patched, stats) = victory_loader::recomp::apply_static_patches(&bytes, &plan);
        std::fs::write(out_path, patched)?;
        println!(
            "wrote {} (patched_sites={}, patched_bytes={}, deferred_sites={})",
            out_path.display(),
            stats.patched_sites,
            stats.patched_bytes,
            stats.deferred_sites
        );
    }
    Ok(())
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    check_policy(
        cfg.game_dir
            .iter()
            .chain(&cfg.test_titles)
            .chain([&cfg.cache_dir, &cfg.data_dir]),
    )
}

/// Each path that breaks the repository rule, on stderr; an error (so a
/// non-zero exit) when there is one, as the commands that use them stop.
fn check_policy<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    let mut violations = 0;
    for p in paths {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            eprintln!("POLICY VIOLATION: {e}");
            violations += 1;
        }
    }
    anyhow::ensure!(violations == 0, "{violations} policy violation(s)");
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_check_fails_on_a_path_inside_the_repo() {
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().to_path_buf();
        let inside = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(check_policy([&outside]).is_ok());
        let err = check_policy([&outside, &inside, &inside]).unwrap_err();
        assert_eq!(err.to_string(), "2 policy violation(s)");
    }

    #[test]
    fn cli_parses_precompile_shaders_and_subcommands() {
        let cli = Cli::try_parse_from(["victory", "--precompile-shaders", "C:\\Game", "--jobs", "4"]).unwrap();
        assert_eq!(cli.precompile_shaders.as_deref(), Some(std::path::Path::new("C:\\Game")));
        assert_eq!(cli.jobs, Some(4));
        assert!(cli.command.is_none());

        let run_cli = Cli::try_parse_from(["victory", "run", "game.exe", "--execute"]).unwrap();
        assert!(run_cli.precompile_shaders.is_none());
        assert!(matches!(run_cli.command, Some(Command::Run { execute: true, .. })));

        let shaders_cli = Cli::try_parse_from(["victory", "shaders", "C:\\Game"]).unwrap();
        assert!(matches!(shaders_cli.command, Some(Command::Shaders { .. })));

        let recomp_cli = Cli::try_parse_from(["victory", "recomp", "game.exe", "--list-sites"]).unwrap();
        assert!(matches!(
            recomp_cli.command,
            Some(Command::Recomp { list_sites: true, patch_out: None, .. })
        ));
    }
}
