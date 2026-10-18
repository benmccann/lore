// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

use clap::CommandFactory;
use clap::Parser;
use lore::file::DiffExportEntry;
use lore_client::cli::LoreCli;
use lore_client::cli::LoreCommands;
use lore_client::commands::difftool::*;

#[test]
fn git_short_options_coexist_with_global_options() {
    LoreCli::command().debug_assert();
    // Markdown generation builds subcommands independently of inherited globals.
    let _ = clap_markdown::help_markdown::<LoreCli>();
    let cli = LoreCli::try_parse_from([
        "lore", "--debug", "difftool", "-d", "-t", "meld", "-y", "--time", "--", "a b.txt",
    ])
    .unwrap();
    assert!(cli.debug);
    assert!(cli.time);
    let Some(LoreCommands::Difftool(args)) = cli.command else {
        panic!("wrong command")
    };
    assert!(args.dir_diff);
    assert_eq!(args.tool.as_deref(), Some("meld"));
    assert_eq!(args.paths, ["a b.txt"]);
    assert!(args.no_prompt);
}

#[test]
fn opposite_flags_use_last_value_and_staged_conflicts_with_target() {
    let cli = LoreCli::try_parse_from([
        "lore",
        "difftool",
        "--prompt",
        "-y",
        "--gui",
        "--no-gui",
        "--trust-exit-code",
        "--no-trust-exit-code",
        "--cached",
    ])
    .unwrap();
    let Some(LoreCommands::Difftool(args)) = cli.command else {
        panic!("wrong command")
    };
    assert!(args.no_prompt && !args.prompt && args.no_gui && !args.gui);
    assert!(args.no_trust_exit_code && !args.trust_exit_code && args.staged);
    assert!(LoreCli::try_parse_from(["lore", "difftool", "--staged", "--target", "1"]).is_err());
}

#[test]
fn configuration_fallbacks_and_explicit_overrides() {
    let config: ExternalToolsConfig = toml::from_str(
        r#"
        [diff]
        tool = "custom"
        [merge]
        guitool = "meld"
        [difftool]
        prompt = false
        trustExitCode = true
        guiDefault = "auto"
        [mergetool.custom]
        cmd = 'compare "$LOCAL" "$REMOTE"'
        [mergetool.meld]
        path = "/some path/meld"
    "#,
    )
    .unwrap();
    let args = DifftoolArgs::default();
    let tool = resolve_tool(&args, &config, false, |_| false).unwrap();
    assert_eq!(tool.name, "custom");
    assert!(!tool.prompt && tool.trust_exit_code);
    assert!(matches!(
        tool.command,
        ToolCommand::Shell {
            append_paths: false,
            ..
        }
    ));
    let tool = resolve_tool(&args, &config, true, |_| false).unwrap();
    assert_eq!(tool.name, "meld");
    assert!(
        matches!(tool.command, ToolCommand::Builtin { ref program, .. } if program == "/some path/meld")
    );
    let args = DifftoolArgs {
        tool: Some("vimdiff".into()),
        prompt: true,
        no_trust_exit_code: true,
        ..Default::default()
    };
    let tool = resolve_tool(&args, &config, true, |_| false).unwrap();
    assert_eq!(tool.name, "vimdiff");
    assert!(tool.prompt && !tool.trust_exit_code);
}

#[test]
fn extcmd_overrides_tools_and_missing_tools_are_errors() {
    let config = ExternalToolsConfig::default();
    let args = DifftoolArgs {
        tool: Some("invalid".into()),
        extcmd: Some("my command".into()),
        ..Default::default()
    };
    assert!(matches!(
        resolve_tool(&args, &config, false, |_| false)
            .unwrap()
            .command,
        ToolCommand::Shell {
            append_paths: true,
            ..
        }
    ));
    assert!(resolve_tool(&DifftoolArgs::default(), &config, false, |_| false).is_err());
    let args = DifftoolArgs {
        extcmd: None,
        ..args
    };
    assert!(resolve_tool(&args, &config, false, |_| true).is_err());
}

#[test]
fn rotate_and_skip_require_a_changed_path() {
    let mut entries: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|path| DiffExportEntry {
            path: path.into(),
            left: None,
            right: Some(path.into()),
            working: true,
        })
        .collect();
    order_entries(&mut entries, Some("b"), None).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        ["b", "c", "a"]
    );
    order_entries(&mut entries, None, Some("a")).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(order_entries(&mut entries, Some("missing"), None).is_err());
}

#[test]
fn directory_copyback_preserves_concurrent_edits() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let copy = temp.path().join("copy");
    let before = blake3::hash(b"before");
    std::fs::write(&original, b"before").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(&copy, b"tool edit").unwrap();
    copy_back(&original, &copy, &before).unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), b"tool edit");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    std::fs::write(&original, b"concurrent edit").unwrap();
    assert!(copy_back(&original, &copy, &before).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), b"concurrent edit");
    std::fs::write(&copy, b"before").unwrap();
    copy_back(&original, &copy, &before).unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), b"concurrent edit");
}

#[cfg(unix)]
#[test]
fn shell_commands_receive_literal_paths_environment_and_exit_status() {
    let temp = tempfile::tempdir().unwrap();
    let left = temp.path().join("left '$(false); .txt");
    let right = temp.path().join("right space.txt");
    std::fs::write(&left, b"left\0\xff").unwrap();
    let tool = Tool {
        name: "test".into(),
        prompt: false,
        trust_exit_code: false,
        command: ToolCommand::Shell {
            command: "cp".into(),
            append_paths: true,
        },
    };
    assert_eq!(launch(&tool, &left, &right, "file name").unwrap(), 0);
    assert_eq!(std::fs::read(&right).unwrap(), b"left\0\xff");
    let mut tool = Tool { command: ToolCommand::Shell {
        command: "test \"$BASE\" = \"$MERGED\" && test \"$MERGED\" = 'file name' && cmp \"$LOCAL\" \"$REMOTE\"".into(), append_paths: false,
    }, ..tool };
    assert_eq!(launch(&tool, &left, &right, "file name").unwrap(), 0);
    tool.command = ToolCommand::Shell {
        command: "exit 23".into(),
        append_paths: false,
    };
    assert_eq!(launch(&tool, Path::new(""), Path::new(""), "").unwrap(), 0);
    tool.trust_exit_code = true;
    assert_eq!(launch(&tool, Path::new(""), Path::new(""), "").unwrap(), 23);
    tool.trust_exit_code = false;
    tool.command = ToolCommand::Shell {
        command: "exit 127".into(),
        append_paths: false,
    };
    assert_eq!(
        launch(&tool, Path::new(""), Path::new(""), "").unwrap(),
        127
    );
}
