// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Materialize the same changes as `diff` without decoding or patching their contents.
use std::path::Path;
use std::sync::Arc;

use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use super::diff::DiffError;
use super::diff::DiffSide;
use super::diff::collect_diff_changes;
use super::diff::read_diff_bytes;
use super::diff::resolve_diff_states;
use crate::change::NodeChange;
use crate::errors::WriteRequired;
use crate::fs::filesystem_provider::InstanceOperation;
use crate::fs::filesystem_provider::InstanceOperationImpl;
use crate::fs::filesystem_provider::with_operation;
use crate::node::NodeFlags;
use crate::repository::RepositoryContext;
use crate::state::State;
use crate::util::path::RelativePath;
use crate::util::path::is_path_inside_repository;

/// Paths below the export's `left` and `right` directories; an absent side is an
/// addition or deletion. Keeping the original names also supports directory tools.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiffExportEntry {
    pub path: String,
    pub left: Option<String>,
    pub right: Option<String>,
    pub working: bool,
}

pub async fn export(
    repository: Arc<RepositoryContext>,
    source: Option<String>,
    target: Option<String>,
    paths: Vec<RelativePath>,
    staged: bool,
    output: &Path,
) -> Result<(), DiffError> {
    // This read call may only write outside the repository, like `file write`.
    if is_path_inside_repository(repository.require_path()?, &output.to_string_lossy()) {
        return Err(WriteRequired.into());
    }
    let (source, mut target) = resolve_diff_states(repository.clone(), source, target).await?;
    if staged {
        let (current, staged, _) = State::deserialize_current_and_staged(repository.clone())
            .await
            .forward::<DiffError>("Reading staged revision")?;
        target = Some(staged.unwrap_or(current));
    }
    let changes = collect_diff_changes(repository.clone(), &source, target.as_ref(), paths).await?;
    let driver = lore_io::IoDriver::global();
    for side in ["left", "right"] {
        driver
            .create_dir_all(output.join(side))
            .await
            .internal("Creating diff directory")?;
    }
    let emit = async |target: DiffSide<'_>, staged_working: Option<&Arc<InstanceOperationImpl>>| {
        export_changes(
            repository.clone(),
            &source,
            target,
            &changes,
            output,
            staged_working,
        )
        .await
    };
    let entries = match target {
        Some(target) if staged => {
            with_operation(repository.file_system(), async |operation| {
                emit(DiffSide::Revision(&target), Some(&operation)).await
            })
            .await?
        }
        Some(target) => emit(DiffSide::Revision(&target), None).await?,
        None => {
            with_operation(repository.file_system(), async |operation| {
                emit(DiffSide::Working(&operation), None).await
            })
            .await?
        }
    };
    let manifest = serde_json::to_vec(&entries).internal("Serializing diff manifest")?;
    write_export_file(&output.join("manifest.json"), manifest.into()).await
}

async fn export_changes(
    repository: Arc<RepositoryContext>,
    source: &Arc<State>,
    target: DiffSide<'_>,
    changes: &[NodeChange],
    output: &Path,
    staged_working: Option<&Arc<InstanceOperationImpl>>,
) -> Result<Vec<DiffExportEntry>, DiffError> {
    let mut entries = Vec::new();
    for change in changes {
        // Lore stages paths, not snapshots. A staged path reads the working
        // bytes commit would capture; unstaged paths retain their revision bytes.
        let staged_operation =
            staged_working.filter(|_| change.to.flags.contains(NodeFlags::Staged));
        let target = staged_operation.map_or(target, DiffSide::Working);
        let from_file = change.from.flags.contains(NodeFlags::File);
        let to_file =
            if staged_operation.is_some() && change.action == crate::change::FileAction::Delete {
                false
            } else {
                match target {
                    DiffSide::Revision(_) => change.to.flags.contains(NodeFlags::File),
                    DiffSide::Working(operation) => operation
                        .file_info(change.path())
                        .await
                        .forward::<DiffError>("Reading diff file information")?
                        .is_file(),
                }
            };
        if !from_file && !to_file {
            continue;
        }
        let from_path = change.move_source().unwrap_or(change.path());
        let left = if from_file {
            Some(read_diff_bytes(repository.clone(), DiffSide::Revision(source), from_path).await?)
        } else {
            None
        };
        let right = if to_file {
            Some(read_diff_bytes(repository.clone(), target, change.path()).await?)
        } else {
            None
        };
        if left == right && from_path == change.path() {
            continue;
        }
        if let Some(bytes) = left.as_ref() {
            write_export_file(&output.join("left").join(from_path.as_str()), bytes.clone()).await?;
        }
        if let Some(bytes) = right.as_ref() {
            write_export_file(
                &output.join("right").join(change.path().as_str()),
                bytes.clone(),
            )
            .await?;
        }
        entries.push(DiffExportEntry {
            path: change.path().to_string(),
            left: left.map(|_| from_path.to_string()),
            right: right.map(|_| change.path().to_string()),
            working: matches!(target, DiffSide::Working(_)),
        });
    }
    Ok(entries)
}

// Only writes the export directory outside the repository, checked by `export`.
#[allow(clippy::disallowed_methods)]
async fn write_export_file(path: &Path, bytes: bytes::Bytes) -> Result<(), DiffError> {
    let driver = lore_io::IoDriver::global();
    driver
        .create_dir_all(path.parent().expect("export file has a parent"))
        .await
        .internal("Creating diff directory")?;
    // Export destinations were checked above and belong to the client's temporary directory.
    // Use a blocking task to support files larger than the driver's whole-file helper limit.
    let path = path.to_owned();
    crate::lore_spawn_blocking!(move || std::fs::write(path, bytes))
        .await
        .internal("Joining diff file write")?
        .internal("Writing diff file")?;
    Ok(())
}
