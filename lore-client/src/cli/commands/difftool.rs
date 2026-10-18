// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use clap::Args;
use lore::call_delegation::run_command;
use lore::file::DiffExportEntry;
use lore::file::LoreFileDiffExportArgs;
use lore::interface::LoreEvent;
use lore::interface::LoreGlobalArgs;
use lore::interface::LoreString;
use serde::Deserialize;

use crate::cli::EventCallbackExt;
use crate::cli::EventCallbackFn;
use crate::config::config;
use crate::eprintln;
use crate::println;
use crate::util::convert_paths_and_targets;

#[lore_macro::test_pub]
#[derive(Args, Default)]
pub struct DifftoolArgs {
    // Shadow the inherited short flags in this subcommand so Git's -d/-t
    // remain available. The shared IDs still propagate --debug/--time globally.
    /// Enable debug output
    #[arg(long, global = true)]
    debug: bool,
    #[arg(long, global = true, hide = true)]
    time: bool,
    /// Revision to compare from (defaults to the current revision, as with lore diff)
    #[arg(long, value_name = "revision")]
    source: Option<String>,
    /// Revision to compare to (defaults to the working tree)
    #[arg(long, value_name = "revision", conflicts_with = "staged")]
    target: Option<String>,
    /// Compare changes selected for commit (Lore stages paths, so their current contents are used)
    #[arg(long, visible_alias = "cached")]
    staged: bool,
    /// Paths to compare; defaults to the whole repository
    #[arg(value_name = "paths", conflicts_with = "targets")]
    paths: Vec<String>,
    /// Read paths from a targets file
    #[arg(long, value_name = "file")]
    targets: Option<String>,
    /// Diff tool to launch (see --tool-help)
    #[arg(short = 't', long, value_name = "tool")]
    tool: Option<String>,
    /// Shell command to run with the two file paths appended
    #[arg(short = 'x', long, value_name = "command")]
    extcmd: Option<String>,
    /// List built-in and configured tools
    #[arg(long)]
    tool_help: bool,
    /// Do not prompt before launching each tool
    #[arg(short = 'y', long, overrides_with = "prompt")]
    no_prompt: bool,
    /// Prompt before each invocation, overriding configuration
    #[arg(long, overrides_with = "no_prompt")]
    prompt: bool,
    /// Compare directories containing all changed files, without prompting
    #[arg(short = 'd', long)]
    dir_diff: bool,
    /// Select diff.guitool instead of diff.tool
    #[arg(short = 'g', long, overrides_with = "no_gui")]
    gui: bool,
    /// Override selection of a graphical tool
    #[arg(long, overrides_with = "gui")]
    no_gui: bool,
    /// Stop and return the tool's exit code on failure
    #[arg(long, overrides_with = "no_trust_exit_code")]
    trust_exit_code: bool,
    /// Ignore nonzero tool exit codes
    #[arg(long, overrides_with = "trust_exit_code")]
    no_trust_exit_code: bool,
    /// Link the right directory to working files (default on Unix)
    #[arg(long, overrides_with = "no_symlinks")]
    symlinks: bool,
    /// Copy working files into the right directory and copy edits back afterwards
    #[arg(long, overrides_with = "symlinks")]
    no_symlinks: bool,
    /// Start at this repository-relative path, moving earlier paths to the end
    #[arg(long, value_name = "path", conflicts_with = "skip_to")]
    rotate_to: Option<String>,
    /// Start at this repository-relative path and skip earlier paths
    #[arg(long, value_name = "path")]
    skip_to: Option<String>,
}

#[lore_macro::test_pub]
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExternalToolsConfig {
    #[serde(default)]
    diff: ToolSelection,
    #[serde(default)]
    merge: ToolSelection,
    #[serde(default)]
    difftool: ToolSettings,
    #[serde(default)]
    mergetool: ToolSettings,
}

#[lore_macro::test_pub]
#[derive(Debug, Default, Deserialize)]
struct ToolSelection {
    tool: Option<String>,
    guitool: Option<String>,
}

#[lore_macro::test_pub]
#[derive(Debug, Default, Deserialize)]
struct ToolSettings {
    prompt: Option<bool>,
    #[serde(rename = "trustExitCode", alias = "trust_exit_code")]
    trust_exit_code: Option<bool>,
    #[serde(rename = "guiDefault", alias = "gui_default")]
    gui_default: Option<GuiDefault>,
    #[serde(flatten)]
    tools: HashMap<String, ToolConfig>,
}

#[lore_macro::test_pub]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GuiDefault {
    Enabled(bool),
    Mode(GuiMode),
}

#[lore_macro::test_pub]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum GuiMode {
    Auto,
}

#[lore_macro::test_pub]
#[derive(Debug, Default, Deserialize)]
struct ToolConfig {
    cmd: Option<String>,
    path: Option<String>,
}

const BUILTIN_TOOLS: &[&str] = &[
    "araxis",
    "bc",
    "bc3",
    "bc4",
    "codecompare",
    "deltawalker",
    "diffmerge",
    "diffuse",
    "ecmerge",
    "emerge",
    "examdiff",
    "guiffy",
    "gvimdiff",
    "kdiff3",
    "kompare",
    "meld",
    "nvimdiff",
    "opendiff",
    "p4merge",
    "smerge",
    "tkdiff",
    "vimdiff",
    "vscode",
    "winmerge",
    "xxdiff",
];

#[lore_macro::test_pub]
#[derive(Debug)]
enum ToolCommand {
    Builtin { program: String, args: Vec<String> },
    Shell { command: String, append_paths: bool },
}

#[lore_macro::test_pub]
#[derive(Debug)]
struct Tool {
    name: String,
    command: ToolCommand,
    prompt: bool,
    trust_exit_code: bool,
}

#[lore_macro::test_pub]
fn resolve_tool(
    args: &DifftoolArgs,
    settings: &ExternalToolsConfig,
    display: bool,
    available: impl Fn(&str) -> bool,
) -> Result<Tool, io::Error> {
    let gui = if args.gui {
        true
    } else if args.no_gui {
        false
    } else {
        match &settings.difftool.gui_default {
            Some(GuiDefault::Enabled(value)) => *value,
            Some(GuiDefault::Mode(GuiMode::Auto)) => display,
            None => false,
        }
    };
    let prompt = if args.prompt {
        true
    } else if args.no_prompt {
        false
    } else {
        settings
            .difftool
            .prompt
            .or(settings.mergetool.prompt)
            .unwrap_or(true)
    };
    let trust_exit_code = if args.trust_exit_code {
        true
    } else if args.no_trust_exit_code {
        false
    } else {
        settings.difftool.trust_exit_code.unwrap_or(false)
    };
    if let Some(command) = &args.extcmd {
        if command.trim().is_empty() {
            return Err(io::Error::other("The external command is empty"));
        }
        return Ok(Tool {
            name: command.clone(),
            command: ToolCommand::Shell {
                command: command.clone(),
                append_paths: true,
            },
            prompt,
            trust_exit_code,
        });
    }
    let configured = args
        .tool
        .as_ref()
        .or_else(|| {
            if gui {
                settings
                    .diff
                    .guitool
                    .as_ref()
                    .or(settings.merge.guitool.as_ref())
            } else {
                None
            }
        })
        .or(settings.diff.tool.as_ref())
        .or(settings.merge.tool.as_ref());
    let name = if let Some(name) = configured {
        name.clone()
    } else {
        let candidates: &[&str] = if gui || display {
            &[
                "meld", "opendiff", "kdiff3", "tkdiff", "xxdiff", "gvimdiff", "diffuse", "kompare",
                "p4merge", "bc", "winmerge", "vimdiff",
            ]
        } else {
            &["vimdiff", "nvimdiff", "emerge"]
        };
        candidates
            .iter()
            .find(|name| {
                builtin_command(name).is_some_and(|(program, _)| {
                    available(program) || (program == "bcomp" && available("bcompare"))
                })
            })
            .ok_or_else(|| {
                io::Error::other(
                    "No diff tool found; use --tool, --extcmd, or configure diff.tool in cli.toml",
                )
            })?
            .to_string()
    };
    let diff = settings.difftool.tools.get(&name);
    let merge = settings.mergetool.tools.get(&name);
    let command = if let Some(command) = diff
        .and_then(|tool| tool.cmd.as_ref())
        .or_else(|| merge.and_then(|tool| tool.cmd.as_ref()))
    {
        if command.trim().is_empty() {
            return Err(io::Error::other("The configured diff command is empty"));
        }
        ToolCommand::Shell {
            command: command.clone(),
            append_paths: false,
        }
    } else {
        let (program, arguments) = builtin_command(&name).ok_or_else(|| {
            io::Error::other(format!(
                "Unknown diff tool '{name}'; configure difftool.{name}.cmd in cli.toml"
            ))
        })?;
        let program = if program == "bcomp" && !available(program) && available("bcompare") {
            "bcompare"
        } else {
            program
        };
        let program = diff
            .and_then(|tool| tool.path.as_ref())
            .or_else(|| merge.and_then(|tool| tool.path.as_ref()))
            .map_or(program, String::as_str)
            .to_owned();
        ToolCommand::Builtin {
            program,
            args: arguments.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    };
    Ok(Tool {
        name,
        command,
        prompt,
        trust_exit_code,
    })
}

fn builtin_command(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match name {
        "araxis" => ("compare", &["-wait", "-2"]),
        "bc" | "bc3" | "bc4" => ("bcomp", &[]),
        "codecompare" => ("CodeCompare", &[]),
        "deltawalker" => ("DeltaWalker", &[]),
        "diffmerge" => ("diffmerge", &[]),
        "diffuse" => ("diffuse", &[]),
        "ecmerge" => ("ecmerge", &["--default", "--mode=diff2"]),
        "emerge" => ("emacs", &["-f", "emerge-files-command"]),
        "examdiff" => ("ExamDiff.com", &["-nh"]),
        "guiffy" => ("guiffy", &[]),
        "gvimdiff" => ("gvim", &["-R", "-f", "-d"]),
        "kdiff3" => ("kdiff3", &[]),
        "kompare" => ("kompare", &[]),
        "meld" => ("meld", &[]),
        "nvimdiff" => ("nvim", &["-R", "-f", "-d"]),
        "opendiff" => ("opendiff", &[]),
        "p4merge" => ("p4merge", &[]),
        "smerge" => ("smerge", &["mergetool"]),
        "tkdiff" => ("tkdiff", &[]),
        "vimdiff" => ("vim", &["-R", "-f", "-d"]),
        "vscode" => ("code", &["--wait", "--diff"]),
        "winmerge" => ("WinMergeU", &["-u", "-e", "-wl"]),
        "xxdiff" => ("xxdiff", &[]),
        _ => return None,
    })
}

fn executable_available(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let path = dir.join(program);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
            #[cfg(not(unix))]
            {
                path.is_file() || path.with_extension("exe").is_file()
            }
        })
    })
}

#[lore_macro::test_pub]
fn tool_command(tool: &Tool, left: &Path, right: &Path, merged: &str) -> Command {
    let mut command = match &tool.command {
        ToolCommand::Builtin { program, args } => {
            let mut command = Command::new(program);
            command.args(args).arg(left).arg(right);
            if tool.name == "smerge" {
                command.arg("-o").arg(merged);
            }
            command
        }
        ToolCommand::Shell {
            command,
            append_paths,
        } => {
            // Like Git, custom commands use a POSIX shell on every platform. Paths
            // are shell parameters, never interpolated into executable shell text.
            let mut shell = Command::new("sh");
            shell.arg("-c");
            if *append_paths {
                shell
                    .arg(format!("{command} \"$@\""))
                    .arg("lore-difftool")
                    .arg(left)
                    .arg(right);
            } else {
                shell.arg(command);
            }
            shell
        }
    };
    command
        .env("LOCAL", left)
        .env("REMOTE", right)
        .env("MERGED", merged)
        .env("BASE", merged);
    command
}

pub fn handle_difftool(globals: LoreGlobalArgs, args: &DifftoolArgs) -> u8 {
    match run_difftool(globals, args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn run_difftool(globals: LoreGlobalArgs, args: &DifftoolArgs) -> Result<u8, Box<dyn Error>> {
    let settings = &config().external_tools;
    if args.tool_help {
        println!("Built-in diff tools:");
        for name in BUILTIN_TOOLS {
            println!("  {name}");
        }
        let mut custom: Vec<_> = settings
            .difftool
            .tools
            .keys()
            .chain(settings.mergetool.tools.keys())
            .collect();
        custom.sort();
        custom.dedup();
        for name in custom {
            println!("  {name} (configured)");
        }
        return Ok(0);
    }
    let root = std::path::absolute(globals.repository_path())?;
    let temp = tempfile::Builder::new()
        .prefix("lore-difftool-")
        .tempdir()?;
    let output = temp.path().join("diff");
    let paths = if args.paths.is_empty() {
        None
    } else {
        Some(args.paths.clone())
    };
    let export_args = LoreFileDiffExportArgs {
        paths: convert_paths_and_targets(&paths, &args.targets),
        source_revision: LoreString::from(&args.source),
        target_revision: LoreString::from(&args.target),
        staged: u8::from(args.staged),
        output: output.as_path().into(),
    };
    let callback = (Box::new(|_: &LoreEvent| {}) as EventCallbackFn).with_defaults();
    let status = run_command(globals, export_args.into(), Some(callback));
    if status != 0 {
        return Ok(status as u8);
    }
    let mut entries: Vec<DiffExportEntry> =
        serde_json::from_slice(&std::fs::read(output.join("manifest.json"))?)?;
    order_entries(
        &mut entries,
        args.rotate_to.as_deref(),
        args.skip_to.as_deref(),
    )?;
    if entries.is_empty() {
        return Ok(0);
    }
    let mut tool = resolve_tool(
        args,
        settings,
        std::env::var_os("DISPLAY").is_some(),
        executable_available,
    )?;
    if config().non_interactive {
        tool.prompt = false;
    }
    if args.dir_diff {
        // Remove skipped files from the directory view too.
        let all: Vec<DiffExportEntry> =
            serde_json::from_slice(&std::fs::read(output.join("manifest.json"))?)?;
        for entry in all
            .iter()
            .filter(|entry| !entries.iter().any(|kept| kept.path == entry.path))
        {
            if let Some(path) = &entry.left {
                std::fs::remove_file(output.join("left").join(path))?;
            }
            if let Some(path) = &entry.right {
                std::fs::remove_file(output.join("right").join(path))?;
            }
        }
        let symlinks = !args.no_symlinks && (args.symlinks || cfg!(unix));
        let mut copies = Vec::new();
        for entry in entries.iter().filter(|entry| entry.working) {
            if let Some(path) = &entry.right {
                let copy = output.join("right").join(path);
                let original = root.join(path);
                if symlinks {
                    std::fs::remove_file(&copy)?;
                    symlink_file(&original, &copy)?;
                } else {
                    copies.push((original, copy.clone(), fingerprint(&copy)?));
                }
            }
        }
        let status = launch(&tool, &output.join("left"), &output.join("right"), ".")?;
        for (original, copy, before) in copies {
            if let Err(error) = copy_back(&original, &copy, &before) {
                let saved = temp.keep();
                return Err(io::Error::other(format!(
                    "{error}; diff files preserved at {}",
                    saved.display()
                ))
                .into());
            }
        }
        return Ok(status);
    }
    let empty = temp.path().join("empty");
    for (index, entry) in entries.iter().enumerate() {
        if tool.prompt && !prompt(&tool.name, &entry.path, index + 1, entries.len())? {
            continue;
        }
        // A tool may have edited the empty side of the preceding deletion.
        std::fs::write(&empty, [])?;
        let left = entry
            .left
            .as_ref()
            .map_or_else(|| empty.clone(), |path| output.join("left").join(path));
        let right = entry.right.as_ref().map_or_else(
            || empty.clone(),
            |path| {
                if entry.working {
                    root.join(path)
                } else {
                    output.join("right").join(path)
                }
            },
        );
        let status = launch(&tool, &left, &right, &entry.path)?;
        if status != 0 {
            return Ok(status);
        }
    }
    Ok(0)
}

#[lore_macro::test_pub]
fn order_entries(
    entries: &mut Vec<DiffExportEntry>,
    rotate: Option<&str>,
    skip: Option<&str>,
) -> io::Result<()> {
    if let Some(path) = rotate.or(skip) {
        let index = entries
            .iter()
            .position(|entry| entry.path == path)
            .ok_or_else(|| io::Error::other(format!("No changed file named '{path}'")))?;
        if rotate.is_some() {
            entries.rotate_left(index);
        } else {
            entries.drain(..index);
        }
    }
    Ok(())
}

fn prompt(tool: &str, path: &str, index: usize, total: usize) -> io::Result<bool> {
    println!("\nViewing ({index}/{total}): '{path}'");
    crate::print!("Launch '{tool}' [Y/n]? ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(!answer.trim_start().starts_with(['n', 'N']))
}

#[lore_macro::test_pub]
fn launch(tool: &Tool, left: &Path, right: &Path, merged: &str) -> io::Result<u8> {
    let mut command = tool_command(tool, left, right, merged);
    let status = if tool.name == "opendiff" && matches!(tool.command, ToolCommand::Builtin { .. }) {
        // FileMerge detaches; its inherited stdout stays open until the window closes.
        let mut child = command.stdout(std::process::Stdio::piped()).spawn()?;
        io::copy(
            &mut child.stdout.take().expect("piped stdout"),
            &mut io::stdout(),
        )?;
        child.wait()?
    } else {
        command.status()?
    };
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
    };
    #[cfg(not(unix))]
    let code = status.code();
    let code = code.and_then(|code| u8::try_from(code).ok()).unwrap_or(1);
    // Git treats shell execution errors as fatal even without --trust-exit-code.
    Ok(if tool.trust_exit_code || code >= 126 {
        code
    } else {
        0
    })
}

fn symlink_file(original: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(original, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(original, link)
    }
}

#[lore_macro::test_pub]
fn copy_back(original: &Path, copy: &Path, before: &blake3::Hash) -> io::Result<()> {
    // Deleting a temporary side does not request deletion from the working tree.
    if !copy.try_exists()? {
        return Ok(());
    }
    let after = fingerprint(copy)?;
    if &after == before {
        return Ok(());
    }
    let current = fingerprint(original)?;
    if current == after {
        return Ok(());
    }
    if &current != before {
        return Err(io::Error::other(format!(
            "Working file changed while the tool was open: {}",
            original.display()
        )));
    }
    // Keep the working file's permissions (including its executable bit).
    let mut source = std::fs::File::open(copy)?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(original)?;
    io::copy(&mut source, &mut destination)?;
    Ok(())
}

fn fingerprint(path: &Path) -> io::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize())
}
