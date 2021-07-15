#![feature(rustc_private)]
#![allow(clippy::too_many_lines)]

extern crate rustc_session;

use cargo::core::{FeatureValue, Package, PackageId, Source, SourceId, Workspace};
use cargo::sources::RegistrySource;
use cargo::util::interning::InternedString;
use curl::easy::Easy;
use log::debug;
use rustc_session::getopts;
use serde::Deserialize;
use std::collections::HashSet;
use std::{
    env, io,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::{Arc, RwLock},
};

pub type Result<T> = cargo::util::CargoResult<T>;

#[derive(Debug, Deserialize)]
struct Invocation {
    package_name: String,
    target_kind: Vec<String>,
    outputs: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct BuildPlan {
    invocations: Vec<Invocation>,
}

/// Main entry point.
///
/// Parse CLI arguments, handle their semantics, and provide for proper error handling.
fn main() {
    if env_logger::try_init().is_err() {
        eprintln!("ERROR: could not initialize logger");
    }

    let mut config = match cargo::Config::default() {
        Ok(cfg) => cfg,
        Err(e) => panic!("can't obtain cargo config: {:?}", e),
    };

    let opts = cli::options();

    let matches = match cli::parse_args(&opts) {
        Ok(m) => m,
        Err(f) => cli::exit_with_error(&config, f.into()),
    };

    if matches.opt_present("h") {
        cli::print_help(&opts);
        return;
    }

    if matches.opt_present("V") {
        cli::print_version();
        return;
    }

    if let Err(e) = cli::validate_args(&matches) {
        cli::exit_with_error(&config, e);
    }

    let config_res = config.configure(
        0,                        // verbose
        matches.opt_present("q"), // quiet
        None,                     // color
        false,                    // frozen
        false,                    // locked
        matches.opt_present("offline"),
        &None, // target_dir
        &[],   // unstable_flags
        &[],   // cli_config
    );

    if let Err(e) = config_res {
        cli::exit_with_error(&config, e);
    }

    if let Err(e) = run(&config, &matches) {
        cli::exit_with_error(&config, e);
    }
}

/// Obtain two versions of the same crate, the "current" version, and the
/// "stable" version, compile them both into `rlib`s, and report the breaking
/// introduced in the "current" version with respect to the "stable" version.
// TODO: possibly reduce the complexity by finding where some info can be taken from directly
fn run(config: &cargo::Config, matches: &getopts::Matches) -> Result<()> {
    use cargo::util::important_paths::find_root_manifest_for_wd;
    debug!("running cargo-semver");

    let explain = matches.opt_present("e");
    let compact = matches.opt_present("compact");
    let json = matches.opt_present("json");

    // Obtain WorkInfo for the "current"
    let current = if let Some(name_and_version) = matches.opt_str("C") {
        // -C "name:version" requires fetching the appropriate package:
        WorkInfo::remote(config, &PackageNameAndVersion::parse(&name_and_version)?)?
    } else if let Some(path) = matches.opt_str("c").map(PathBuf::from) {
        // -c "local_path":
        WorkInfo::local(config, &find_root_manifest_for_wd(&path)?)?
    } else {
        // default: if neither -c / -C are used, use the workspace at the
        // current working directory:
        WorkInfo::local(config, &find_root_manifest_for_wd(config.cwd())?)?
    };
    let name = current.package.name().to_owned();

    if !current.package.targets().iter().any(|t| t.is_lib()) {
        return Err(anyhow::anyhow!(
            "package `{}` lacks required [lib] target",
            &name
        ));
    }

    // TODO: JSON output here
    if matches.opt_present("show-public") {
        let (current_rlib, current_deps_output) =
            current.rlib_and_dep_output(config, &name, true, matches)?;

        let mut child = Command::new("rust-semver-public");
        child
            .arg("--crate-type=lib")
            .args(&["--extern", &*format!("new={}", current_rlib.display())])
            .args(&[format!("-L{}", current_deps_output.display())]);

        if let Some(target) = matches.opt_str("target") {
            child.args(&["--target", &target]);
        }

        let mut child = child
            .arg("-")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::Error::msg(format!("could not spawn rustc: {}", e)))?;

        if let Some(ref mut stdin) = child.stdin {
            stdin.write_fmt(format_args!(
                "#[allow(unused_extern_crates)] \
                 extern crate new;"
            ))?;
        } else {
            return Err(anyhow::Error::msg(
                "could not pipe to rustc (wtf?)".to_owned(),
            ));
        }

        let exit_status = child
            .wait()
            .map_err(|e| anyhow::Error::msg(format!("failed to wait for rustc: {}", e)))?;

        return if exit_status.success() {
            Ok(())
        } else {
            Err(anyhow::Error::msg("rustc-semver-public errored".to_owned()))
        };
    }

    // Obtain WorkInfo for the "stable" version
    let (stable, stable_version) = if let Some(name_and_version) = matches.opt_str("S") {
        // -S "name:version" requires fetching the appropriate package:
        let info = PackageNameAndVersion::parse(&name_and_version)?;
        let version = info.version.to_owned();
        let work_info = WorkInfo::remote(config, &info)?;
        (work_info, version)
    } else if let Some(path) = matches.opt_str("s") {
        // -s "local_path":
        let work_info = WorkInfo::local(config, &PathBuf::from(path))?;
        let version = format!("{}", work_info.package.version());
        (work_info, version)
    } else {
        // default: if neither -s / -S are used, use the current's crate name to find the
        // latest stable version of the crate on crates.io and use that one:
        let stable_crate = find_on_crates_io(&name)?;
        let info = PackageNameAndVersion {
            name: &name,
            version: &stable_crate.max_version,
        };
        let work_info = WorkInfo::remote(config, &info)?;
        (work_info, stable_crate.max_version.clone())
    };

    let (current_rlib, current_deps_output) =
        current.rlib_and_dep_output(config, &name, true, matches)?;
    let (stable_rlib, stable_deps_output) =
        stable.rlib_and_dep_output(config, &name, false, matches)?;

    if matches.opt_present("d") {
        println!(
            "--extern old={} -L{} --extern new={} -L{}",
            stable_rlib.display(),
            stable_deps_output.display(),
            current_rlib.display(),
            current_deps_output.display()
        );
        return Ok(());
    }

    debug!("running rust-semverver on compiled crates");

    let mut child = Command::new("rust-semverver");
    child
        .arg("--crate-type=lib")
        .args(&["--extern", &*format!("old={}", stable_rlib.display())])
        .args(&[format!("-L{}", stable_deps_output.display())])
        .args(&["--extern", &*format!("new={}", current_rlib.display())])
        .args(&[format!("-L{}", current_deps_output.display())]);

    if let Some(target) = matches.opt_str("target") {
        child.args(&["--target", &target]);
    }

    let child = child
        .arg("-")
        .stdin(Stdio::piped())
        .env("RUST_SEMVER_CRATE_VERSION", stable_version)
        .env("RUST_SEMVER_VERBOSE", format!("{}", explain))
        .env("RUST_SEMVER_COMPACT", format!("{}", compact))
        .env("RUST_SEMVER_JSON", format!("{}", json))
        .env(
            "RUST_SEMVER_API_GUIDELINES",
            if matches.opt_present("a") {
                "true"
            } else {
                "false"
            },
        );

    let mut child = child
        .spawn()
        .map_err(|e| anyhow::Error::msg(format!("could not spawn rustc: {}", e)))?;

    if let Some(ref mut stdin) = child.stdin {
        // The order of the `extern crate` declaration is important here: it will later
        // be used to select the `old` and `new` crates.
        stdin.write_fmt(format_args!(
            "#[allow(unused_extern_crates)] \
             extern crate old; \
             #[allow(unused_extern_crates)] \
             extern crate new;"
        ))?;
    } else {
        return Err(anyhow::Error::msg(
            "could not pipe to rustc (wtf?)".to_owned(),
        ));
    }

    let exit_status = child
        .wait()
        .map_err(|e| anyhow::Error::msg(format!("failed to wait for rustc: {}", e)))?;

    if exit_status.success() {
        Ok(())
    } else {
        Err(anyhow::Error::msg("rustc-semverver errored".to_owned()))
    }
}

/// CLI utils
mod cli {
    use cargo::util::CliError;
    use rustc_session::getopts;

    /// CLI options
    pub fn options() -> getopts::Options {
        let mut opts = getopts::Options::new();

        opts.optflag("h", "help", "print this message and exit");
        opts.optflag("V", "version", "print version information and exit");
        opts.optflag("e", "explain", "print detailed error explanations");
        opts.optflag(
            "q",
            "quiet",
            "surpress regular cargo output, print only important messages",
        );
        opts.optflag(
            "",
            "show-public",
            "print the public types in the current crate given by -c or -C and exit",
        );
        opts.optflag("d", "debug", "print command to debug and exit");
        opts.optflag(
            "a",
            "api-guidelines",
            "report only changes that are breaking according to the API-guidelines",
        );
        opts.optopt(
            "",
            "features",
            "Space-separated list of features to activate",
            "FEATURES",
        );
        opts.optflag("", "all-features", "Activate all available features");
        opts.optflag(
            "",
            "no-default-features",
            "Do not activate the `default` feature",
        );
        opts.optflag(
            "",
            "compact",
            "Only output the suggested version on stdout for further processing",
        );
        opts.optflag(
            "j",
            "json",
            "Output a JSON-formatted description of all collected data on stdout.",
        );
        opts.optopt(
            "s",
            "stable-path",
            "use local path as stable/old crate",
            "PATH",
        );
        opts.optopt(
            "c",
            "current-path",
            "use local path as current/new crate",
            "PATH",
        );
        opts.optopt(
            "S",
            "stable-pkg",
            "use a `name:version` string as stable/old crate",
            "NAME:VERSION",
        );
        opts.optopt(
            "C",
            "current-pkg",
            "use a `name:version` string as current/new crate",
            "NAME:VERSION",
        );
        opts.optflag("", "offline", "Run without accessing the network.");
        opts.optopt("", "target", "Build for the target triple", "<TRIPLE>");
        opts
    }

    /// Parse CLI arguments
    pub fn parse_args(opts: &getopts::Options) -> Result<getopts::Matches, getopts::Fail> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        opts.parse(&args)
    }

    /// Validate CLI arguments
    pub fn validate_args(matches: &getopts::Matches) -> Result<(), anyhow::Error> {
        if (matches.opt_present("s") && matches.opt_present("S"))
            || matches.opt_count("s") > 1
            || matches.opt_count("S") > 1
        {
            let msg = "at most one of `-s,--stable-path` and `-S,--stable-pkg` allowed";
            return Err(anyhow::Error::msg(msg.to_owned()));
        }

        if (matches.opt_present("c") && matches.opt_present("C"))
            || matches.opt_count("c") > 1
            || matches.opt_count("C") > 1
        {
            let msg = "at most one of `-c,--current-path` and `-C,--current-pkg` allowed";
            return Err(anyhow::Error::msg(msg.to_owned()));
        }

        Ok(())
    }

    /// Print a help message
    pub fn print_help(opts: &getopts::Options) {
        // FIXME: pass remaining options to cargo
        let brief = "usage: cargo semver [options]";
        print!("{}", opts.usage(brief));
    }

    /// Print a version message.
    pub fn print_version() {
        println!("{}", env!("CARGO_PKG_VERSION"));
    }

    /// Exit with error `e`.
    pub fn exit_with_error(config: &cargo::Config, e: anyhow::Error) -> ! {
        config
            .shell()
            .set_verbosity(cargo::core::shell::Verbosity::Normal);
        cargo::exit_with_error(CliError::new(e, 1), &mut config.shell());
    }
}

/// A package's name and version.
pub struct PackageNameAndVersion<'a> {
    /// The crate's name.
    pub name: &'a str,
    /// The package's version, as a semver-string.
    pub version: &'a str,
}

impl<'a> PackageNameAndVersion<'a> {
    /// Parses the string "name:version" into `Self`
    pub fn parse(s: &'a str) -> Result<Self> {
        let err = || {
            anyhow::Error::msg(format!(
                "spec has to be of form `name:version` but is `{}`",
                s
            ))
        };
        let mut split = s.split(':');
        let name = split.next().ok_or_else(err)?;
        let version = split.next().ok_or_else(err)?;
        if split.next().is_some() {
            Err(err())
        } else {
            Ok(Self { name, version })
        }
    }
}

/// A specification of a package and it's workspace.
pub struct WorkInfo<'a> {
    /// The package to be compiled.
    pub package: Package,
    /// The package's workspace.
    workspace: Workspace<'a>,
}

impl<'a> WorkInfo<'a> {
    /// Construct a package/workspace pair for the `manifest_path`
    pub fn local(config: &'a cargo::Config, manifest_path: &Path) -> Result<WorkInfo<'a>> {
        let workspace = Workspace::new(manifest_path, config)?;
        let package = workspace.load(manifest_path)?;
        Ok(Self { package, workspace })
    }

    /// Construct a package/workspace pair by fetching the package of a
    /// specified `PackageNameAndVersion` from the `source`.
    pub fn remote(
        config: &'a cargo::Config,
        &PackageNameAndVersion { name, version }: &PackageNameAndVersion,
    ) -> Result<WorkInfo<'a>> {
        let source = {
            let source_id = SourceId::crates_io(config)?;
            let mut source = RegistrySource::remote(source_id, &HashSet::new(), config);

            debug!("source id loaded: {:?}", source_id);

            if !config.offline() {
                let _lock = config.acquire_package_cache_lock()?;
                source.update()?;
            }

            Box::new(source)
        };

        // TODO: fall back to locally cached package instance, or better yet, search for it
        // first.
        let package_id = PackageId::new(name, version, source.source_id())?;
        debug!("(remote) package id: {:?}", package_id);

        let package = source.download_now(package_id, config)?;
        let workspace = Workspace::ephemeral(package.clone(), config, None, false)?;

        Ok(Self { package, workspace })
    }

    /// Obtain the paths to the produced rlib and the dependency output directory.
    pub fn rlib_and_dep_output(
        &self,
        config: &'a cargo::Config,
        name: &str,
        current: bool,
        matches: &getopts::Matches,
    ) -> Result<(PathBuf, PathBuf)> {
        // We don't need codegen-ready artifacts (which .rlib files are) so
        // settle for .rmeta files, which result from `cargo check` mode
        let mode = cargo::core::compiler::CompileMode::Check { test: false };
        let mut opts = cargo::ops::CompileOptions::new(config, mode)?;
        // we need the build plan to find our build artifacts
        opts.build_config.build_plan = true;

        let compile_kind = if let Some(target) = matches.opt_str("target") {
            let target = cargo::core::compiler::CompileTarget::new(&target)?;

            let kind = cargo::core::compiler::CompileKind::Target(target);
            opts.build_config.requested_kinds = vec![kind];
            kind
        } else {
            cargo::core::compiler::CompileKind::Host
        };

        if let Some(s) = matches.opt_str("features") {
            opts.cli_features.features = Rc::new(
                s.split(' ')
                    .map(InternedString::new)
                    .map(FeatureValue::new)
                    .collect(),
            );
        }

        opts.cli_features.all_features = matches.opt_present("all-features");
        opts.cli_features.uses_default_features = !matches.opt_present("no-default-features");

        env::set_var(
            "RUSTFLAGS",
            format!("-C metadata={}", if current { "new" } else { "old" }),
        );

        // Capture build plan from a separate Cargo invocation
        let output = VecWrite(Arc::new(RwLock::new(Vec::new())));

        let mut file_write = cargo::core::Shell::from_write(Box::new(output.clone()));
        file_write.set_verbosity(cargo::core::Verbosity::Quiet);

        let old_shell = std::mem::replace(&mut *config.shell(), file_write);

        cargo::ops::compile(&self.workspace, &opts)?;

        let _ = std::mem::replace(&mut *config.shell(), old_shell);
        let plan_output = output.read()?;

        // actually compile things now
        opts.build_config.build_plan = false;

        let compilation = cargo::ops::compile(&self.workspace, &opts)?;
        env::remove_var("RUSTFLAGS");

        let build_plan: BuildPlan = serde_json::from_slice(&plan_output)
            .map_err(|_| anyhow::anyhow!("Can't read build plan"))?;

        // TODO: handle multiple outputs gracefully
        for i in &build_plan.invocations {
            if let Some(kind) = i.target_kind.get(0) {
                if kind.contains("lib") && i.package_name == name {
                    let deps_output = &compilation.deps_output[&compile_kind];

                    return Ok((i.outputs[0].clone(), deps_output.clone()));
                }
            }
        }

        Err(anyhow::Error::msg("lost build artifact".to_owned()))
    }
}

/// Given a `crate_name`, try to locate the corresponding crate on `crates.io`.
///
/// If no crate with the exact name is present, error out.
pub fn find_on_crates_io(crate_name: &str) -> Result<crates_io::Crate> {
    let mut handle = Easy::new();
    handle.useragent(&format!("rust-semverver {}", env!("CARGO_PKG_VERSION")))?;
    let mut registry =
        crates_io::Registry::new_handle("https://crates.io".to_owned(), None, handle);

    registry
        .search(crate_name, 1)
        .map_err(|e| {
            anyhow::Error::msg(format!(
                "failed to retrieve search results from the registry: {}",
                e
            ))
        })
        .and_then(|(mut crates, _)| {
            crates
                .drain(..)
                .find(|krate| krate.name == crate_name)
                .ok_or_else(|| {
                    anyhow::Error::msg(format!("failed to find a matching crate `{}`", crate_name))
                })
        })
}

/// Thread-safe byte buffer that implements `io::Write`.
#[derive(Clone)]
struct VecWrite(Arc<RwLock<Vec<u8>>>);

impl VecWrite {
    pub fn read(&self) -> io::Result<std::sync::RwLockReadGuard<'_, Vec<u8>>> {
        self.0
            .read()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poison"))
    }
    pub fn write(&self) -> io::Result<std::sync::RwLockWriteGuard<'_, Vec<u8>>> {
        self.0
            .write()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "lock poison"))
    }
}

impl io::Write for VecWrite {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut lock = Self::write(self)?;
        io::Write::write(&mut *lock, data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
