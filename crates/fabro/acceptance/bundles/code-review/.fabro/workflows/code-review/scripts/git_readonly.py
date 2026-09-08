#!/usr/bin/env python3
"""Restricted read-only Git entry point for code-review agents.

Duplicated from the security-review workflow so this directory installs alone.
The wrapper does not invoke a shell, ignores global Git configuration, disables
external diff helpers, and accepts only the history-reading subcommands the
review agents need.
"""

from __future__ import annotations

import sys


safe_path = getattr(sys.flags, "safe_path", sys.flags.isolated)
if not sys.flags.isolated or not safe_path:
    print(
        "git_readonly.py: Python isolated mode is required; invoke with python3 -I",
        file=sys.stderr,
    )
    sys.exit(2)

import os
import subprocess
from pathlib import Path
from typing import Dict, List, Sequence


ALLOWED_SUBCOMMANDS = {
    "blame",
    "diff",
    "log",
    "show",
}
FORBIDDEN_ARGUMENTS = {
    "--config-env",
    "--exec-path",
    "--ext-diff",
    "--no-index",
    "--output",
    "--paginate",
    "--show-signature",
    "--textconv",
    "--upload-pack",
    "--write",
    "-c",
}
FORBIDDEN_BLAME_ARGUMENTS = {
    "--contents",
    "--ignore-revs-file",
    "-S",
}
SAFE_GIT_CONFIGURATION = {
    "blame.ignoreRevsFile": "",
    "core.alternateRefsCommand": "false",
    "core.askPass": "false",
    "core.attributesFile": os.devnull,
    "core.editor": "false",
    "core.excludesFile": os.devnull,
    "core.fsmonitor": "",
    "core.gitProxy": "false",
    "core.hooksPath": os.devnull,
    "core.pager": "cat",
    "core.sshCommand": "false",
    "diff.external": "false",
    "diff.orderFile": os.devnull,
    "gpg.format": "openpgp",
    "gpg.openpgp.program": "false",
    "gpg.program": "false",
    "gpg.ssh.allowedSignersFile": os.devnull,
    "gpg.ssh.defaultKeyCommand": "false",
    "gpg.ssh.program": "false",
    "gpg.ssh.revocationFile": os.devnull,
    "interactive.diffFilter": "false",
    "mailmap.file": os.devnull,
    "pager.blame": "false",
    "pager.diff": "false",
    "pager.log": "false",
    "pager.show": "false",
    "protocol.allow": "never",
    "protocol.ext.allow": "never",
    "protocol.file.allow": "never",
    "submodule.recurse": "false",
}
SAFE_ENVIRONMENT = {
    "GIT_ATTR_NOSYSTEM": "1",
    "GIT_CONFIG": os.devnull,
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_SYSTEM": os.devnull,
    "GIT_PAGER": "cat",
    "GIT_TERMINAL_PROMPT": "0",
    "LANG": "C.UTF-8",
    "LC_ALL": "C.UTF-8",
    "PAGER": "cat",
    "PATH": os.defpath,
}
MAX_ARGUMENTS = 200
MAX_ARGUMENT_LENGTH = 4096
# Git runs these per-driver keys as commands. Their middle segment is chosen by
# the repository, so they cannot be disabled by a fixed key list and are instead
# neutralized for every driver the repository actually configures.
EXECUTABLE_DRIVER_KEYS = {
    "filter": ("clean", "process", "smudge"),
    "diff": ("command", "textconv"),
}


class GitWrapperError(RuntimeError):
    """A rejected wrapper request."""


def validate_arguments(argv: Sequence[str]) -> List[str]:
    if not argv:
        raise GitWrapperError(
            "usage: git_readonly.py <diff|show|log|blame> [arguments]"
        )
    if len(argv) > MAX_ARGUMENTS:
        raise GitWrapperError("too many Git arguments")

    subcommand = argv[0]
    if subcommand not in ALLOWED_SUBCOMMANDS:
        raise GitWrapperError(f"Git subcommand is not allowed: {subcommand!r}")

    validated: List[str] = []
    for argument in argv[1:]:
        if "\0" in argument or "\n" in argument or "\r" in argument:
            raise GitWrapperError("Git arguments cannot contain control characters")
        if len(argument) > MAX_ARGUMENT_LENGTH:
            raise GitWrapperError("a Git argument exceeds the length limit")
        option_name = argument.split("=", 1)[0]
        if option_name in FORBIDDEN_ARGUMENTS or (
            option_name.startswith("--")
            and option_name != "--"
            and any(
                forbidden.startswith(option_name)
                for forbidden in FORBIDDEN_ARGUMENTS
                if forbidden.startswith("--")
            )
        ):
            raise GitWrapperError(f"Git option is not allowed: {option_name!r}")
        if argument.startswith("-O"):
            raise GitWrapperError("Git diff order files are not allowed")
        if subcommand == "blame" and (
            option_name in FORBIDDEN_BLAME_ARGUMENTS
            or (
                option_name.startswith("--")
                and option_name != "--"
                and any(
                    forbidden.startswith(option_name)
                    for forbidden in FORBIDDEN_BLAME_ARGUMENTS
                    if forbidden.startswith("--")
                )
            )
            or argument.startswith("-S")
        ):
            raise GitWrapperError(f"Git blame option is not allowed: {option_name!r}")
        validated.append(argument)
    if subcommand == "diff":
        path_operands = [arg for arg in validated if not arg.startswith("-")]
        if len(path_operands) >= 2 and all(
            Path(operand).exists() for operand in path_operands[:2]
        ):
            raise GitWrapperError(
                "Git diff cannot compare two working-tree file operands"
            )
    for argument in validated:
        reject_operand_outside_repository(argument)
    return [subcommand, *validated]


def reject_operand_outside_repository(argument: str) -> None:
    """Keep every filesystem operand inside the repository."""
    if argument.startswith("-"):
        return
    if argument.startswith("/") or argument.startswith("\\\\"):
        raise GitWrapperError(
            f"Git operand must be inside the repository: {argument!r}"
        )
    if ".." not in argument.split("/"):
        return
    root = Path.cwd().resolve()
    try:
        (root / argument).resolve().relative_to(root)
    except (ValueError, OSError) as error:
        raise GitWrapperError(
            f"Git operand must be inside the repository: {argument!r}"
        ) from error


def configured_driver_keys(worktree: Path) -> List[str]:
    """Return the executable per-driver keys this repository configures.

    Driver names live in the middle of the key, so a fixed override list cannot
    reach them. Reading the names first lets every configured driver be
    overridden by name. The probe itself runs no repository-supplied command.
    """
    # Legacy GIT_CONFIG points `git config` at a single file, which would hide
    # the repository's own driver names. It does not affect the history-reading
    # subcommands, so only this probe drops it.
    probe_environment = {
        key: value for key, value in SAFE_ENVIRONMENT.items() if key != "GIT_CONFIG"
    }
    try:
        listing = subprocess.run(
            ["git", "-C", str(worktree), "config", "--list", "--name-only", "-z"],
            capture_output=True,
            check=False,
            env=probe_environment,
            stdin=subprocess.DEVNULL,
            text=True,
        )
    except OSError as error:
        raise GitWrapperError(f"could not read Git configuration: {error}") from error
    if listing.returncode != 0:
        return []

    keys: List[str] = []
    for name in listing.stdout.split("\0"):
        section, _, remainder = name.partition(".")
        driver, _, key = remainder.rpartition(".")
        if driver and key in EXECUTABLE_DRIVER_KEYS.get(section, ()):
            keys.append(name)
    return keys


def build_command(argv: Sequence[str]) -> List[str]:
    subcommand, *arguments = validate_arguments(argv)
    command = ["git"]
    for key, value in SAFE_GIT_CONFIGURATION.items():
        command.extend(["-c", f"{key}={value}"])
    command.extend(
        [
            "-C",
            str(Path.cwd().resolve()),
            subcommand,
            "--no-ext-diff",
            "--no-textconv",
            "--no-show-signature",
            *arguments,
        ]
    )
    return command


def build_environment(driver_keys: Sequence[str]) -> Dict[str, str]:
    """Disable each configured driver at Git's highest configuration precedence.

    These overrides travel in the environment rather than in `-c` arguments:
    Git splits a `-c` pair on its first `=`, so a driver name containing `=`
    cannot be overridden that way, while the key/value variables carry the name
    exactly.
    """
    environment = dict(SAFE_ENVIRONMENT)
    for index, key in enumerate(driver_keys):
        environment[f"GIT_CONFIG_KEY_{index}"] = key
        environment[f"GIT_CONFIG_VALUE_{index}"] = ""
    environment["GIT_CONFIG_COUNT"] = str(len(driver_keys))
    return environment


def main(argv: Sequence[str]) -> int:
    try:
        command = build_command(argv)
        environment = build_environment(
            configured_driver_keys(Path.cwd().resolve())
        )
    except GitWrapperError as error:
        print(f"git_readonly.py: {error}", file=sys.stderr)
        return 2

    try:
        result = subprocess.run(
            command,
            env=environment,
            stdin=subprocess.DEVNULL,
            check=False,
        )
    except OSError as error:
        print(f"git_readonly.py: could not run Git: {error}", file=sys.stderr)
        return 2
    return int(result.returncode)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
