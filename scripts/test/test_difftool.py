# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import json
import os
import shlex
import shutil
import sys
from pathlib import Path

import pytest

pytestmark = [
    pytest.mark.smoke,
    pytest.mark.skipif(shutil.which("sh") is None, reason="custom tools require sh"),
]


def recording_tool(scratch_dir):
    root = scratch_dir("difftool", create=True)
    script = root / "record tool.py"
    log = root / "invocations.jsonl"
    script.write_text(
        """import json, os, sys
from pathlib import Path
left, right = map(Path, sys.argv[2:])
def contents(path):
    if path.is_dir():
        return {str(p.relative_to(path)).replace('\\\\', '/'): p.read_bytes().hex()
                for p in path.rglob('*') if p.is_file()}
    return path.read_bytes().hex()
with open(sys.argv[1], 'a') as log:
    log.write(json.dumps({'left': contents(left), 'right': contents(right),
                         'local': str(left), 'remote': str(right),
                         'merged': os.environ['MERGED'], 'base': os.environ['BASE']}) + '\\n')
if os.environ.get('EDIT_DIFF'):
    edit = right / 'changed.txt' if right.is_dir() else right
    if edit.name == 'changed.txt':
        edit.write_bytes(b'edited by tool\\n')
"""
    )
    # The external-command contract is a POSIX shell, including Git for Windows.
    command = shlex.join([sys.executable, str(script), str(log)])
    return command, log


@pytest.mark.parametrize("directory", [False, True])
def test_difftool_exact_contents_and_working_edits(
    new_lore_repo, scratch_dir, directory
):
    command, log = recording_tool(scratch_dir)
    repo = new_lore_repo(create_repo=False, environment_vars={"EDIT_DIFF": "1"})
    repo.repository_create(offline=True)
    old = {"changed.txt": b"old\n", "deleted.bin": b"\x00\xff", "unchanged": b"same"}
    repo.write_commit_push("initial", old, offline=True)
    root = Path(repo.path)
    (root / "changed.txt").write_bytes(b"new\n")
    (root / "deleted.bin").unlink()
    (root / "added.bin").write_bytes(b"\x00\x80\xfe")
    repo.stage("added.bin", offline=True)
    flags = ["-d", "--no-symlinks"] if directory else ["-y"]
    repo.run(["difftool", *flags, "-x", command], offline=True)
    calls = [json.loads(line) for line in log.read_text().splitlines()]
    expected_left = {
        path: contents.hex() for path, contents in old.items() if path != "unchanged"
    }
    expected_right = {"changed.txt": b"new\n".hex(), "added.bin": b"\x00\x80\xfe".hex()}
    if directory:
        assert len(calls) == 1
        assert calls[0]["left"] == expected_left
        assert calls[0]["right"] == expected_right
    else:
        assert {call["merged"]: (call["left"], call["right"]) for call in calls} == {
            path: (expected_left.get(path, ""), expected_right.get(path, ""))
            for path in expected_left.keys() | expected_right.keys()
        }
        assert all(call["base"] == call["merged"] for call in calls)
    assert (root / "changed.txt").read_bytes() == b"edited by tool\n"
    assert (root / "unchanged").read_bytes() == b"same"
    assert all(not Path(call["local"]).exists() for call in calls)


def test_difftool_revisions_staging_and_path_selection(new_lore_repo, scratch_dir):
    command, log = recording_tool(scratch_dir)
    repo = new_lore_repo(create_repo=False)
    repo.repository_create(offline=True)
    name = "space ' file.txt"
    repo.write_commit_push(
        "initial",
        {name: b"first", "other": b"unchanged", "unstaged": b"old"},
        offline=True,
    )
    first = repo.revision_info(offline=True).signature
    repo.write_files({name: b"staged", "other": b"changed"})
    repo.stage([name, "other"], offline=True)
    repo.write_files({name: b"working", "unstaged": b"new"})
    repo.run(["difftool", "--staged", "-y", "-x", command], offline=True)
    calls = [json.loads(line) for line in log.read_text().splitlines()]
    assert {call["merged"] for call in calls} == {name, "other"}
    repo.run(["difftool", "--cached", "-y", "-x", command, "--", name], offline=True)
    call = json.loads(log.read_text().splitlines()[-1])
    assert (call["left"], call["right"], call["merged"]) == (
        b"first".hex(),
        b"working".hex(),
        name,
    )
    repo.commit("second", offline=True)
    second = repo.revision_info(offline=True).signature
    repo.write_files({name: b"later edit"})
    repo.run(
        ["difftool", "--source", first, "--target", second, "-y", "-x", command, name],
        offline=True,
    )
    call = json.loads(log.read_text().splitlines()[-1])
    assert (call["left"], call["right"]) == (b"first".hex(), b"working".hex())
    assert not Path(call["local"]).exists()
    assert not Path(call["remote"]).exists()
    assert (Path(repo.path) / name).read_bytes() == b"later edit"
    count = len(log.read_text().splitlines())
    repo.run(
        ["difftool", "--source", second, "--target", second, "-y", "-x", command],
        offline=True,
    )
    assert len(log.read_text().splitlines()) == count


@pytest.mark.skipif(
    os.name != "posix", reason="default directory symlinks are Unix-only"
)
def test_difftool_directory_symlinks_edit_working_files(new_lore_repo, scratch_dir):
    command, log = recording_tool(scratch_dir)
    repo = new_lore_repo(create_repo=False, environment_vars={"EDIT_DIFF": "1"})
    repo.repository_create(offline=True)
    repo.write_commit_push("initial", {"changed.txt": b"old"}, offline=True)
    repo.write_files({"changed.txt": b"new"})
    repo.run(["difftool", "-d", "-x", command], offline=True)
    assert (Path(repo.path) / "changed.txt").read_bytes() == b"edited by tool\n"
    assert len(log.read_text().splitlines()) == 1
