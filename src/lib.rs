#![deny(warnings)]

extern crate cargo_metadata;
#[macro_use]
extern crate failure;
extern crate clap;
extern crate regex;
extern crate rustc_cfg;
extern crate rustc_demangle;
extern crate rustc_version;
extern crate walkdir;
extern crate toml;
extern crate serde;

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf, Component};
use std::process::{Command, Stdio};
use std::{env, str};

use cargo_metadata::{Message, parse_messages, Artifact};
use clap::{App, AppSettings, Arg};
use rustc_cfg::Cfg;
use walkdir::WalkDir;

mod llvm;
mod postprocess;

#[derive(Clone, Copy, PartialEq)]
pub enum Tool {
    Nm,
    Objcopy,
    Objdump,
    Profdata,
    Readobj,
    Size,
    Strip,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Nm => "nm",
            Tool::Objcopy => "objcopy",
            Tool::Objdump => "objdump",
            Tool::Profdata => "profdata",
            Tool::Readobj => "readobj",
            Tool::Size => "size",
            Tool::Strip => "strip",
        }
    }

    // Whether this tool requires the project to be previously built
    fn needs_build(self) -> bool {
        match self {
            Tool::Nm | Tool::Objcopy | Tool::Objdump | Tool::Size | Tool::Readobj | Tool::Strip => true,
            Tool::Profdata /* ? */ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Endian {
    Little,
    Big,
}

/// Execution context
// TODO this should be some sort of initialize once, read-only singleton
pub struct Context {
    cfg: Cfg,
    /// Final compilation target
    target: String,
}

/// Search for `file` in `path` and its parent directories
fn search<'p>(path: &'p Path, file: &str) -> Option<&'p Path> {
    path.ancestors().find(|dir| dir.join(file).exists())
}

fn parse<T>(path: &Path) -> Result<T, failure::Error>
where
    T: for<'de> serde::Deserialize<'de>,
{
    use std::fs::File;
    use std::io::Read;
    use toml::de;

    let mut s = String::new();
    File::open(path)?.read_to_string(&mut s)?;
    Ok(de::from_str(&s)?)
}

impl Context {
    /* Constructors */
    /// Get a context structure from a built artifact.
    fn from_artifact(artifact: &Artifact) -> Result<Self, failure::Error> {
        // Get target from artifact. Ideally, the artifact should really contain
        // the target triple. Sadly, it doesn't. So as an approximation, we
        // extract it from the filename path.
        let metadata = cargo_metadata::MetadataCommand::new().exec()?;

        // Should always succeed.
        let target_path = artifact.filenames[0].strip_prefix(metadata.target_directory)?;
        let target_name = if let Some(Component::Normal(path)) = target_path.components().next() {
            let path = path.to_string_lossy();
            // TODO: How will custom profiles impact this?
            if path == "debug" || path == "release" {
                // Looks like this artifact was built for the host.
                rustc_version::version_meta()?.host
            } else {
                // The artifact
                path.to_string()
            }
        } else {
            unreachable!();
        };

        Self::from_target_name(&target_name)
    }

    /// Get a context structure from a provided target flag, used when cargo
    /// was not used to build the binary.
    fn from_flag(target_flag: Option<&str>) -> Result<Self, failure::Error> {
        let metadata = cargo_metadata::MetadataCommand::new().exec().ok();

        let meta = rustc_version::version_meta()?;
        let host = meta.host;
        let host_target_name = host;

        // Get the "default" target override in .cargo/config.
        let mut config_target_name = None;
        let config: toml::Value;

        let root_dir = if let Some(metadata) = metadata {
            metadata.workspace_root
        } else {
            std::env::current_dir()?
        };

        if let Some(path) = search(&root_dir, ".cargo/config") {
            config = parse(&path.join(".cargo/config"))?;
            config_target_name = config.get("build")
                .and_then(|build| build.get("target"))
                .and_then(|target| target.as_str());
        }

        // Find the actual target.
        let target_name = target_flag.or(config_target_name).unwrap_or(&host_target_name);

        Self::from_target_name(target_name)
    }

    fn from_target_name(target_name: &str) -> Result<Self, failure::Error> {
        let cfg = Cfg::of(target_name)?;

        Ok(Context {
            cfg,
            target: target_name.to_string(),
        })
    }

    fn rustc_cfg(&self) -> &Cfg {
        &self.cfg
    }

    fn tool(&self, tool: Tool, target: &str) -> Command {
        let mut c = Command::new(format!("rust-{}", tool.name()));

        if tool == Tool::Objdump {
            let arch_name = llvm::arch_name(self.rustc_cfg(), target);

            if arch_name == "thumb" {
                // `-arch-name=thumb` doesn't produce the right output so instead we pass
                // `-triple=$target`, which contains more information about the target
                c.args(&["-triple", target]);
            } else {
                c.args(&["-arch-name", arch_name]);
            }
        }

        c
    }
}

#[cfg(target_os = "windows")]
fn exe(name: &str) -> Cow<str> {
    format!("{}.exe", name).into()
}

#[cfg(not(target_os = "windows"))]
fn exe(name: &str) -> Cow<str> {
    name.into()
}

pub fn run(tool: Tool, examples: Option<&str>) -> Result<i32, failure::Error> {
    let name = tool.name();
    let needs_build = tool.needs_build();

    let app = App::new(format!("cargo-{}", name));
    let about = format!(
        "Proxy for the `llvm-{}` tool shipped with the Rust toolchain.",
        name
    );
    let after_help = format!(
        "\
The arguments specified *after* the `--` will be passed to the proxied tool invocation.

To see all the flags the proxied tool accepts run `cargo-{} -- -help`.{}",
        name,
        examples.unwrap_or("")
    );
    let app = app
        .about(&*about)
        .version(env!("CARGO_PKG_VERSION"))
        .setting(AppSettings::TrailingVarArg)
        .setting(AppSettings::DontCollapseArgsInUsage)
        // as this is used as a Cargo subcommand the first argument will be the name of the binary
        // we ignore this argument
        .arg(Arg::with_name("binary-name").hidden(true))
        .arg(
            Arg::with_name("target")
                .long("target")
                .takes_value(true)
                .value_name("TRIPLE")
                .help("Target triple for which the code is compiled"),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .help("Use verbose output"),
        )
        .arg(Arg::with_name("--").short("-").hidden_short_help(true))
        .arg(Arg::with_name("args").multiple(true))
        .after_help(&*after_help);

    let matches = if needs_build {
        app.arg(
            Arg::with_name("bin")
                .long("bin")
                .takes_value(true)
                .value_name("NAME")
                .help("Build only the specified binary"),
        )
        .arg(
            Arg::with_name("example")
                .long("example")
                .takes_value(true)
                .value_name("NAME")
                .help("Build only the specified example"),
        )
        .arg(
            Arg::with_name("lib")
                .long("lib")
                .help("Build only this package's library"),
        )
        .arg(
            Arg::with_name("release")
                .long("release")
                .help("Build artifacts in release mode, with optimizations"),
        )
        .arg(
            Arg::with_name("features")
                .long("features")
                .takes_value(true)
                .value_name("FEATURES")
                .help("Space-separated list of features to activate"),
        )
        .arg(
            Arg::with_name("all-features")
                .long("all-features")
                .takes_value(false)
                .help("Activate all available features"),
        )
    } else {
        app
    }
    .get_matches();

    let verbose = matches.is_present("verbose");
    let target_flag = matches.value_of("target");

    fn at_least_two_are_true(a: bool, b: bool, c: bool) -> bool {
        if a {
            b || c
        } else {
            b && c
        }
    }

    let bin = matches.is_present("bin");
    let example = matches.is_present("example");
    let lib = matches.is_present("lib");

    if at_least_two_are_true(bin, example, lib) {
        bail!("Only one of `--bin`, `--example` or `--lib` must be specified")
    }

    let should_build = bin || example || lib;

    let artifact = if should_build {
        let mut cargo = Command::new("cargo");
        cargo.arg("build");

        // NOTE we do *not* use `project.target()` here because Cargo will figure things out on
        // its own (i.e. it will search and parse .cargo/config, etc.)
        if let Some(target) = target_flag {
            cargo.args(&["--target", target]);
        }

        if matches.is_present("all-features") {
            cargo.arg("--all-features");
        } else if let Some(features) = matches.value_of("features") {
            cargo.args(&["--features", features]);
        }

        let artifact_name = if bin {
            let bin_name = matches.value_of("bin").unwrap();
            cargo.args(&["--bin", bin_name]);
            bin_name
        } else if example {
            let example_name = matches.value_of("example").unwrap();
            cargo.args(&["--example", example_name]);
            example_name
        } else if lib {
            let lib_name = matches.value_of("lib").unwrap();
            cargo.args(&["--lib", lib_name]);
            lib_name
        } else {
            unreachable!();
        };

        if matches.is_present("release") {
            cargo.arg("--release");
        }

        cargo.arg("--message-format=json");
        cargo.stdout(Stdio::piped());

        if verbose {
            eprintln!("{:?}", cargo);
        }

        let mut child = cargo.spawn()?;

        let stdout = child.stdout.take().unwrap();
        let mut wanted_artifact = None;
        for message in parse_messages(stdout) {
            let message = message?;
            match message {
                Message::CompilerArtifact(artifact) => {
                    if artifact.target.name == artifact_name {
                        wanted_artifact = Some(artifact.clone());
                    }
                },
                Message::CompilerMessage(msg) => {
                    if let Some(rendered) = msg.message.rendered {
                        print!("{}", rendered);
                    }
                },
                _ => (),
            }
        }

        let status = child.wait()?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }

        Some(wanted_artifact.expect("Cargo to compile the wanted artifact"))
    } else {
        None
    };

    let mut tool_args = vec![];
    if let Some(arg) = matches.value_of("--") {
        tool_args.push(arg);
    }

    if let Some(args) = matches.values_of("args") {
        tool_args.extend(args);
    }

    let ctxt = if let Some(artifact) = &artifact {
        Context::from_artifact(artifact)?
    } else {
        Context::from_flag(target_flag)?
    };

    let mut lltool = ctxt.tool(tool, &ctxt.target);

    // Extra flags
    match tool {
        Tool::Readobj => {
            // The default output style of `readobj` is JSON-like, which is not user friendly, so we
            // change it to the human readable GNU style
            lltool.arg("-elf-output-style=GNU");
        }
        Tool::Nm | Tool::Objcopy | Tool::Objdump | Tool::Profdata | Tool::Size | Tool::Strip => {}
    }

    // Artifact
    if let Some(artifact) = &artifact {
        let file = match &artifact.executable {
            // Example and bins have an executable
            Some(val) => val,
            // Libs have an rlib and an rmeta. We want the rlib, which always
            // comes first in the filenames array after some quick testing.
            //
            // We could instead look for files ending in .rlib, but that would
            // fail for cdylib and other fancy crate kinds.
            None => &artifact.filenames[0]
        };

        match tool {
            // for some tools we change the CWD (current working directory) and
            // make the artifact path relative. This makes the path that the
            // tool will print easier to read. e.g. `libfoo.rlib` instead of
            // `/home/user/rust/project/target/$T/debug/libfoo.rlib`.
            Tool::Objdump | Tool::Nm | Tool::Readobj | Tool::Size => {
                lltool
                    .current_dir(file.parent().unwrap())
                    .arg(file.file_name().unwrap());
            }
            Tool::Objcopy | Tool::Profdata | Tool::Strip => {
                lltool.arg(file);
            }
        }
    }

    // User flags
    lltool.args(&tool_args);

    if verbose {
        eprintln!("{:?}", lltool);
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    let output = lltool.stderr(Stdio::inherit()).output()?;

    // post process output
    let pp_output = match tool {
        Tool::Objdump | Tool::Nm | Tool::Readobj => postprocess::demangle(&output.stdout),
        Tool::Size => postprocess::size(&output.stdout),
        Tool::Objcopy | Tool::Profdata | Tool::Strip => output.stdout.into(),
    };

    stdout.write_all(&*pp_output)?;

    if output.status.success() {
        Ok(0)
    } else {
        Ok(output.status.code().unwrap_or(1))
    }
}

pub fn forward(tool: &str) -> Result<i32, failure::Error> {
    let path = search_tool(tool)?;

    // NOTE(`skip`) the first argument is the name of the binary (e.g. `rust-nm`)
    let status = Command::new(path).args(env::args().skip(1)).status()?;

    if status.success() {
        Ok(0)
    } else {
        Ok(status.code().unwrap_or(101))
    }
}

fn search_tool(tool: &str) -> Result<PathBuf, failure::Error> {
    let sysroot = String::from_utf8(
        Command::new("rustc")
            .arg("--print")
            .arg("sysroot")
            .output()?
            .stdout,
    )?;

    for entry in WalkDir::new(sysroot.trim()) {
        let entry = entry?;

        if entry.file_name() == &*exe(tool) {
            return Ok(entry.into_path());
        }
    }

    bail!(
        "`llvm-tools-preview` component is missing or empty. Install it with `rustup component \
         add llvm-tools-preview`"
    );
}
