#!/usr/bin/env python3
"""Repair deterministic CI failures on an existing target branch."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Mapping, Sequence


WORKFLOW_ROOT = Path(__file__).resolve().parent.parent
FACTORY_ROOT = WORKFLOW_ROOT.parents[2]
RUNTIME_DIRECTORY = WORKFLOW_ROOT / "runtime"
TARGET_ROOT = RUNTIME_DIRECTORY / "target"
STATE_PATH = RUNTIME_DIRECTORY / "state.json"
LOG_DIRECTORY = RUNTIME_DIRECTORY / "logs"
SUMMARY_PATH = RUNTIME_DIRECTORY / "check-summary.txt"
PROFILES_ROOT = WORKFLOW_ROOT / "targets"
PROTECTED_BRANCHES = {"main", "master"}
TARGET_NAME_PATTERN = re.compile(r"^[a-z0-9][a-z0-9-]{0,62}$")


class FixCIError(RuntimeError):
    """A workflow contract failure that must stop the run."""


def emit(**updates: Any) -> None:
    print(json.dumps({"context_updates": updates}, separators=(",", ":")))


def run(
    command: Sequence[str],
    *,
    check: bool = True,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    environment = os.environ.copy() if env is None else dict(env)
    environment.pop("GH_TOKEN", None)
    environment.update({"GIT_TERMINAL_PROMPT": "0", "PAGER": "cat"})
    return subprocess.run(
        list(command),
        check=check,
        cwd=cwd,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def git(*arguments: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run(["git", *arguments], check=check, cwd=TARGET_ROOT)


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise FixCIError(f"required file is missing: {path}") from error
    except json.JSONDecodeError as error:
        raise FixCIError(f"invalid JSON in {path}: {error}") from error


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


def load_profile(target: str) -> dict[str, Any]:
    if not TARGET_NAME_PATTERN.fullmatch(target):
        raise FixCIError(
            "target_repository must be a simple lowercase repository name"
        )
    path = PROFILES_ROOT / f"{target}.json"
    value = read_json(path)
    if not isinstance(value, dict):
        raise FixCIError(f"target profile must be a JSON object: {path}")
    required = {"repository", "baseBranch", "instructions", "requiredScripts"}
    if set(value) != required:
        raise FixCIError(f"target profile has unexpected fields: {path}")
    if value["repository"] != f"veniceai/{target}":
        raise FixCIError(f"target profile identity must match veniceai/{target}")
    if value["baseBranch"] != "main":
        raise FixCIError("the Interface fix-ci adapter requires baseBranch=main")
    if value["instructions"] != ["AGENTS.md"]:
        raise FixCIError("the Interface fix-ci adapter must declare AGENTS.md")
    scripts = value["requiredScripts"]
    if not isinstance(scripts, list) or not all(
        isinstance(script, str) and script for script in scripts
    ):
        raise FixCIError("requiredScripts must be a list of script names")
    return value


def validate_target_contract(profile: Mapping[str, Any]) -> None:
    for instruction in profile["instructions"]:
        if not (TARGET_ROOT / str(instruction)).is_file():
            raise FixCIError(f"target instruction file is missing: {instruction}")
    package = read_json(TARGET_ROOT / "package.json")
    scripts = package.get("scripts") if isinstance(package, dict) else None
    if not isinstance(scripts, dict):
        raise FixCIError("target package.json has no scripts object")
    missing = [name for name in profile["requiredScripts"] if not scripts.get(name)]
    if missing:
        raise FixCIError(
            "target package.json is missing CI scripts: " + ", ".join(missing)
        )
    if not (TARGET_ROOT / ".node-version").is_file():
        raise FixCIError("target .node-version is missing")


def clean_runtime() -> None:
    resolved_runtime = RUNTIME_DIRECTORY.resolve()
    if (
        resolved_runtime.parent != WORKFLOW_ROOT.resolve()
        or resolved_runtime.name != "runtime"
    ):
        raise FixCIError("refusing to clean an unexpected runtime path")
    RUNTIME_DIRECTORY.mkdir(parents=True, exist_ok=True)
    for entry in RUNTIME_DIRECTORY.iterdir():
        if entry.name == ".gitkeep":
            continue
        if entry.parent.resolve() != resolved_runtime:
            raise FixCIError("refusing to clean outside the workflow runtime")
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
        else:
            entry.unlink()


def github_api(path: str) -> Any:
    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        raise FixCIError("GITHUB_TOKEN is required")
    request = urllib.request.Request(
        f"https://api.github.com{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "venice-factory-fix-ci",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        raise FixCIError(
            f"GitHub API request failed with status {error.code}: {path}"
        ) from error


def parse_pull_request_number(target: str, repository: str) -> int | None:
    url_pattern = re.compile(
        rf"https://github\.com/{re.escape(repository)}/pull/"
        r"(?P<number>[1-9][0-9]*)/?"
    )
    url_match = url_pattern.fullmatch(target)
    if url_match:
        return int(url_match.group("number"))
    number_match = re.fullmatch(r"#?(?P<number>[1-9][0-9]*)", target)
    if number_match:
        return int(number_match.group("number"))
    return None


def validate_branch(branch: str) -> None:
    if branch in PROTECTED_BRANCHES:
        raise FixCIError(f"refusing to update protected branch: {branch}")
    result = git("check-ref-format", "--branch", branch, check=False)
    if result.returncode != 0:
        raise FixCIError(f"invalid branch name: {branch}")


def resolve_target(target: str, profile: Mapping[str, Any]) -> dict[str, Any]:
    target = target.strip()
    if not target:
        raise FixCIError("target is required; provide a branch, PR number, or PR URL")
    repository = str(profile["repository"])
    pull_request_number = parse_pull_request_number(target, repository)
    if pull_request_number is None:
        validate_branch(target)
        remote = git("ls-remote", "--exit-code", "--heads", "origin", target)
        initial_sha = remote.stdout.split()[0]
        base_remote = git(
            "ls-remote",
            "--exit-code",
            "--heads",
            "origin",
            str(profile["baseBranch"]),
        )
        base_sha = base_remote.stdout.split()[0]
        return {
            "branch": target,
            "initial_sha": initial_sha,
            "base_sha": base_sha,
            "pull_request": None,
        }

    pull_request = github_api(
        f"/repos/{repository}/pulls/{pull_request_number}"
    )
    if pull_request.get("state") != "open":
        raise FixCIError(f"pull request #{pull_request_number} is not open")
    head = pull_request.get("head") or {}
    if (head.get("repo") or {}).get("full_name") != repository:
        raise FixCIError("fork pull requests are not supported")
    branch = head.get("ref")
    initial_sha = head.get("sha")
    if not isinstance(branch, str) or not isinstance(initial_sha, str):
        raise FixCIError("GitHub returned an incomplete pull request head")
    validate_branch(branch)
    base = pull_request.get("base") or {}
    base_sha = base.get("sha")
    if (base.get("repo") or {}).get("full_name") != repository or not isinstance(
        base_sha, str
    ):
        raise FixCIError("GitHub returned an incomplete pull request base")
    return {
        "branch": branch,
        "initial_sha": initial_sha,
        "base_sha": base_sha,
        "pull_request": pull_request_number,
    }


def factory_commit_identity() -> tuple[str, str]:
    result = run(
        ["git", "show", "-s", "--format=%an%x00%ae", "HEAD"],
        cwd=FACTORY_ROOT,
    )
    parts = result.stdout.rstrip("\n").split("\x00")
    if len(parts) != 2 or not all(part.strip() for part in parts):
        raise FixCIError("Factory HEAD has no usable author identity")
    name, email = (part.strip() for part in parts)
    if any(character in name + email for character in "\r\n\x00"):
        raise FixCIError("Factory HEAD has an invalid author identity")
    return name, email


def factory_commit() -> str:
    return run(["git", "rev-parse", "HEAD"], cwd=FACTORY_ROOT).stdout.strip()


def checkout(target_repository: str, target: str) -> None:
    profile = load_profile(target_repository)
    clean_runtime()
    run(
        [
            "gh",
            "repo",
            "clone",
            str(profile["repository"]),
            str(TARGET_ROOT),
            "--",
            "--filter=blob:none",
            "--no-tags",
            "--branch",
            str(profile["baseBranch"]),
        ],
        cwd=FACTORY_ROOT,
    )
    validate_target_contract(profile)
    state = resolve_target(target, profile)
    branch = state["branch"]
    git("fetch", "--no-tags", "origin", f"refs/heads/{branch}")
    fetched_sha = git("rev-parse", "FETCH_HEAD").stdout.strip()
    if fetched_sha != state["initial_sha"]:
        raise FixCIError("the target branch changed while it was being resolved")
    git("checkout", "-B", branch, fetched_sha)
    git("fetch", "--no-tags", "origin", state["base_sha"])
    if git("rev-parse", "--is-shallow-repository").stdout.strip() == "true":
        git("fetch", "--unshallow", "origin")
    state["merge_base"] = git(
        "merge-base", state["base_sha"], state["initial_sha"]
    ).stdout.strip()
    author_name, author_email = factory_commit_identity()
    git("config", "user.name", author_name)
    git("config", "user.email", author_email)
    guard_script = TARGET_ROOT / "script" / "install-main-branch-guards.sh"
    if not guard_script.is_file():
        raise FixCIError("target main branch guard installer is missing")
    run(["bash", str(guard_script)], cwd=TARGET_ROOT)
    state.update(
        {
            **profile,
            "target": target_repository,
            "targetDirectory": str(TARGET_ROOT.relative_to(FACTORY_ROOT)),
            "factoryCommit": factory_commit(),
        }
    )
    write_json(STATE_PATH, state)
    print(
        f"Checked out {branch} at {fetched_sha}; "
        f"branch changes {len(changed_files_since(state['merge_base']))} paths"
    )
    emit(
        target_repository=target_repository,
        target_slug=profile["repository"],
        target_directory=state["targetDirectory"],
        target_branch=branch,
        target_initial_commit=fetched_sha,
    )


def load_state() -> dict[str, Any]:
    value = read_json(STATE_PATH)
    if not isinstance(value, dict):
        raise FixCIError("target state must be an object")
    return value


def assert_factory_unchanged(state: Mapping[str, Any]) -> None:
    expected_commit = state.get("factoryCommit")
    if not isinstance(expected_commit, str) or not expected_commit:
        raise FixCIError("the Factory commit is missing")
    if factory_commit() != expected_commit:
        raise FixCIError("the Factory commit changed during the repair")
    status = run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=FACTORY_ROOT,
    ).stdout
    if status:
        raise FixCIError("tracked Factory files changed during the repair")


def worktree_fingerprint() -> str:
    digest = hashlib.sha256()
    digest.update(git("diff", "--binary", "HEAD").stdout.encode())
    untracked = git("ls-files", "--others", "--exclude-standard").stdout.splitlines()
    for filename in sorted(untracked):
        path = TARGET_ROOT / filename
        digest.update(filename.encode())
        digest.update(b"\0")
        if path.is_file():
            digest.update(path.read_bytes())
    return digest.hexdigest()


def nul_paths(output: str) -> list[str]:
    return [path for path in output.split("\0") if path]


def changed_files_since(base: str) -> list[str]:
    tracked = nul_paths(
        git(
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            base,
        ).stdout
    )
    untracked = nul_paths(
        git("ls-files", "--others", "--exclude-standard", "-z").stdout
    )
    return sorted(set(tracked + untracked))


def merge_base(state: Mapping[str, Any]) -> str:
    value = state.get("merge_base")
    if not isinstance(value, str) or not value:
        raise FixCIError("the repair merge base is missing")
    return value


def assert_lint_did_not_expand_scope(
    state: Mapping[str, Any], before_paths: list[str]
) -> None:
    unexpected = sorted(
        set(changed_files_since(merge_base(state))) - set(before_paths)
    )
    if unexpected:
        displayed = ", ".join(unexpected[:20])
        suffix = "" if len(unexpected) <= 20 else ", …"
        raise FixCIError(
            "lint auto-fix changed files outside its input: "
            f"{displayed}{suffix}"
        )


def lint_command(
    state: Mapping[str, Any],
    *,
    fix: bool = False,
    paths: list[str] | None = None,
) -> list[str]:
    extensions = (".js", ".jsx", ".ts", ".tsx")
    files = sorted(
        {
            filename
            for filename in (
                paths if paths is not None else changed_files_since(merge_base(state))
            )
            if filename.endswith(extensions) and (TARGET_ROOT / filename).is_file()
        }
    )
    if not files:
        return ["true"]
    command = [
        "yarn",
        "eslint",
        "--cache",
        "--cache-strategy",
        "content",
        "--cache-location",
        ".eslintcache",
    ]
    if fix:
        command.extend(
            ["--fix", "--report-unused-disable-directives-severity", "off"]
        )
    return [*command, "--", *files]


def lint_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "NODE_OPTIONS": "--max-old-space-size=24576",
            "VENICE_NO_MANUAL_MEMO": "error",
            "VENICE_PREFER_ALIAS_IMPORTS": "error",
        }
    )
    return environment


def run_check(
    name: str,
    command: list[str],
    *,
    env: Mapping[str, str] | None = None,
) -> bool:
    result = run(command, check=False, cwd=TARGET_ROOT, env=env)
    LOG_DIRECTORY.mkdir(parents=True, exist_ok=True)
    log_path = LOG_DIRECTORY / f"{name}.log"
    log_path.write_text(result.stdout, encoding="utf-8")
    if result.returncode == 0:
        print(f"PASS {name}")
        return True
    print(f"FAIL {name} (exit {result.returncode}; full log: {log_path})")
    output = result.stdout
    if len(output) > 40_000:
        output = "[output truncated to final 40000 characters]\n" + output[-40_000:]
    print(output)
    return False


def check() -> None:
    state = load_state()
    assert_factory_unchanged(state)
    initial_fingerprint = worktree_fingerprint()
    results: list[tuple[str, bool]] = []
    results.append(
        (
            "eslint-rule-tests",
            run_check("eslint-rule-tests", ["yarn", "test:eslint-rules"]),
        )
    )
    before_workers = worktree_fingerprint()
    workers_passed = run_check("web-workers", ["yarn", "build:web-workers"])
    if workers_passed and worktree_fingerprint() != before_workers:
        workers_passed = False
        message = "Worker compilation changed the worktree. Inspect git diff.\n"
        (LOG_DIRECTORY / "web-workers.log").write_text(message, encoding="utf-8")
        print(f"FAIL web-workers ({message.strip()})")
    results.append(("web-workers", workers_passed))
    results.append(
        ("translations", run_check("translations", ["yarn", "verify-translations"]))
    )
    results.append(("typecheck", run_check("typecheck", ["yarn", "run", "check"])))
    results.append(
        ("eslint", run_check("eslint", lint_command(state), env=lint_environment()))
    )
    results.append(("unit-tests", run_check("unit-tests", ["yarn", "test"])))
    results.append(("clean-worktree", worktree_fingerprint() == initial_fingerprint))
    summary_lines = [
        f"{'PASS' if passed else 'FAIL'} {name}" for name, passed in results
    ]
    SUMMARY_PATH.write_text("\n".join(summary_lines) + "\n", encoding="utf-8")
    print("\nCheck summary")
    print("\n".join(summary_lines))
    if not all(passed for _, passed in results):
        raise SystemExit(1)


def lint_fix() -> None:
    state = load_state()
    assert_factory_unchanged(state)
    before_paths = changed_files_since(merge_base(state))
    LOG_DIRECTORY.mkdir(parents=True, exist_ok=True)
    log_path = LOG_DIRECTORY / "lint-fix.log"
    with log_path.open("w", encoding="utf-8") as log_file:
        process = subprocess.Popen(
            lint_command(state, fix=True, paths=before_paths),
            cwd=TARGET_ROOT,
            env=lint_environment(),
            stdout=log_file,
            stderr=subprocess.STDOUT,
            text=True,
        )
        while True:
            try:
                return_code = process.wait(timeout=30)
                break
            except subprocess.TimeoutExpired:
                print(f"Lint fix still running (full log: {log_path})", flush=True)
    print(log_path.read_text(encoding="utf-8"), end="")
    if return_code != 0:
        raise SystemExit(return_code)
    assert_lint_did_not_expand_scope(state, before_paths)


def publish() -> None:
    state = load_state()
    assert_factory_unchanged(state)
    branch = state.get("branch")
    if not isinstance(branch, str):
        raise FixCIError("target branch is missing")
    validate_branch(branch)
    current_branch = git("branch", "--show-current").stdout.strip()
    if current_branch != branch:
        raise FixCIError(
            f"refusing to publish from {current_branch}; expected {branch}"
        )
    remote = git("ls-remote", "--exit-code", "--heads", "origin", branch)
    remote_sha = remote.stdout.split()[0]
    if remote_sha != state.get("initial_sha"):
        raise FixCIError(
            "the target branch changed during the run; refusing to overwrite it"
        )
    if not git("status", "--porcelain").stdout:
        print("All checks passed and the target branch needed no changes.")
        return
    guard_script = TARGET_ROOT / "script" / "install-main-branch-guards.sh"
    if not guard_script.is_file():
        raise FixCIError("target main branch guard installer is missing")
    run(["bash", str(guard_script)], cwd=TARGET_ROOT)
    git("add", "--all")
    git("commit", "-m", "Fix CI failures")
    git("push", "origin", f"HEAD:refs/heads/{branch}")
    pushed_sha = git("rev-parse", "HEAD").stdout.strip()
    print(f"Pushed {pushed_sha} to {branch}")
    emit(target_branch_updated=True, target_commit=pushed_sha)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    checkout_parser = commands.add_parser("checkout")
    checkout_parser.add_argument("--repository", required=True)
    checkout_parser.add_argument("--target", required=True)
    commands.add_parser("check")
    commands.add_parser("lint-fix")
    commands.add_parser("publish")
    return parser


def main() -> None:
    arguments = build_parser().parse_args()
    if arguments.command == "checkout":
        checkout(arguments.repository, arguments.target)
    elif arguments.command == "check":
        check()
    elif arguments.command == "lint-fix":
        lint_fix()
    elif arguments.command == "publish":
        publish()
    else:
        raise FixCIError(f"invalid command: {arguments.command}")


if __name__ == "__main__":
    try:
        main()
    except (FixCIError, subprocess.CalledProcessError) as error:
        print(f"fix-ci: {error}", file=sys.stderr)
        raise SystemExit(1) from error
