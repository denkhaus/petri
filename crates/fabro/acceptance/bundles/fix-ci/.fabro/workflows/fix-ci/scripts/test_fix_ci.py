from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


WORKFLOW_ROOT = Path(__file__).resolve().parent.parent
MODULE_PATH = Path(__file__).with_name("fix_ci.py")
SPEC = importlib.util.spec_from_file_location("fix_ci", MODULE_PATH)
assert SPEC and SPEC.loader
fix_ci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fix_ci)


class ParsePullRequestNumberTest(unittest.TestCase):
    def test_accepts_pull_request_url(self) -> None:
        self.assertEqual(
            fix_ci.parse_pull_request_number(
                "https://github.com/veniceai/interface/pull/13033",
                "veniceai/interface",
            ),
            13033,
        )

    def test_accepts_number(self) -> None:
        self.assertEqual(
            fix_ci.parse_pull_request_number("13033", "veniceai/interface"),
            13033,
        )
        self.assertEqual(
            fix_ci.parse_pull_request_number("#13033", "veniceai/interface"),
            13033,
        )

    def test_treats_branch_as_branch(self) -> None:
        self.assertIsNone(
            fix_ci.parse_pull_request_number(
                "fabro/run/01M0ZTK9787B0G3JJT5R4HQFH1",
                "veniceai/interface",
            )
        )

    def test_rejects_other_repository_url(self) -> None:
        self.assertIsNone(
            fix_ci.parse_pull_request_number(
                "https://github.com/example/interface/pull/13033",
                "veniceai/interface",
            )
        )


class TargetAdapterTest(unittest.TestCase):
    def test_interface_is_the_only_supported_target(self) -> None:
        profile = fix_ci.load_profile("interface")

        self.assertEqual(profile["repository"], "veniceai/interface")
        self.assertEqual(profile["baseBranch"], "main")
        self.assertEqual(profile["instructions"], ["AGENTS.md"])
        with self.assertRaises(fix_ci.FixCIError):
            fix_ci.load_profile("outerface")

    def test_runtime_cleanup_preserves_checkpoint_anchor(self) -> None:
        previous_workflow = fix_ci.WORKFLOW_ROOT
        previous_runtime = fix_ci.RUNTIME_DIRECTORY
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "workflow"
            runtime = workflow / "runtime"
            runtime.mkdir(parents=True)
            (runtime / ".gitkeep").write_text("\n")
            (runtime / "stale.json").write_text("{}\n")
            (runtime / "target").mkdir()
            try:
                fix_ci.WORKFLOW_ROOT = workflow
                fix_ci.RUNTIME_DIRECTORY = runtime

                fix_ci.clean_runtime()

                self.assertTrue((runtime / ".gitkeep").is_file())
                self.assertFalse((runtime / "stale.json").exists())
                self.assertFalse((runtime / "target").exists())
            finally:
                fix_ci.WORKFLOW_ROOT = previous_workflow
                fix_ci.RUNTIME_DIRECTORY = previous_runtime

    def test_target_commit_uses_factory_workflow_author(self) -> None:
        previous_factory = fix_ci.FACTORY_ROOT
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            try:
                subprocess.run(
                    ["git", "init", "-q", "--initial-branch=main"],
                    cwd=root,
                    check=True,
                )
                subprocess.run(
                    ["git", "config", "user.name", "Verified Maintainer"],
                    cwd=root,
                    check=True,
                )
                subprocess.run(
                    ["git", "config", "user.email", "maintainer@example.com"],
                    cwd=root,
                    check=True,
                )
                (root / "workflow.toml").write_text("_version = 1\n")
                subprocess.run(["git", "add", "workflow.toml"], cwd=root, check=True)
                subprocess.run(
                    ["git", "commit", "-qm", "workflow"], cwd=root, check=True
                )
                fix_ci.FACTORY_ROOT = root

                self.assertEqual(
                    fix_ci.factory_commit_identity(),
                    ("Verified Maintainer", "maintainer@example.com"),
                )
            finally:
                fix_ci.FACTORY_ROOT = previous_factory


class LintCommandTest(unittest.TestCase):
    def test_lints_current_changed_source_files_in_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            previous_target = fix_ci.TARGET_ROOT
            root = Path(directory)
            (root / "existing.ts").write_text("export {};\n")
            (root / "support.ts").write_text("export {};\n")
            try:
                fix_ci.TARGET_ROOT = root
                with mock.patch.object(
                    fix_ci,
                    "changed_files_since",
                    return_value=["existing.ts", "support.ts", "README.md"],
                ):
                    command = fix_ci.lint_command({"merge_base": "a" * 40})
            finally:
                fix_ci.TARGET_ROOT = previous_target

        self.assertEqual(command[-2:], ["existing.ts", "support.ts"])

    def test_lint_fix_uses_its_path_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            previous_target = fix_ci.TARGET_ROOT
            root = Path(directory)
            (root / "existing.ts").write_text("export {};\n")
            try:
                fix_ci.TARGET_ROOT = root
                command = fix_ci.lint_command(
                    {"merge_base": "a" * 40},
                    fix=True,
                    paths=["existing.ts", "README.md"],
                )
            finally:
                fix_ci.TARGET_ROOT = previous_target

        self.assertNotIn("lint", command)
        self.assertIn("--fix", command)
        self.assertIn("--report-unused-disable-directives-severity", command)
        self.assertEqual(command[-2:], ["--", "existing.ts"])


class LintScopeTest(unittest.TestCase):
    def test_rejects_a_path_added_by_lint(self) -> None:
        state = {"merge_base": "a" * 40}
        with mock.patch.object(
            fix_ci,
            "changed_files_since",
            return_value=["src/security.ts", "src/unrelated.ts"],
        ):
            with self.assertRaisesRegex(
                fix_ci.FixCIError, "lint auto-fix changed files outside its input"
            ):
                fix_ci.assert_lint_did_not_expand_scope(state, ["src/security.ts"])

    def test_accepts_the_same_or_fewer_paths(self) -> None:
        state = {"merge_base": "a" * 40}
        with mock.patch.object(
            fix_ci, "changed_files_since", return_value=["src/security.ts"]
        ):
            fix_ci.assert_lint_did_not_expand_scope(
                state, ["src/security.ts", "src/security.test.ts"]
            )


class FactoryIntegrityTest(unittest.TestCase):
    def test_rejects_a_changed_factory_commit(self) -> None:
        with mock.patch.object(fix_ci, "factory_commit", return_value="b" * 40):
            with self.assertRaisesRegex(
                fix_ci.FixCIError, "Factory commit changed"
            ):
                fix_ci.assert_factory_unchanged({"factoryCommit": "a" * 40})

    def test_rejects_tracked_factory_changes(self) -> None:
        with (
            mock.patch.object(fix_ci, "factory_commit", return_value="a" * 40),
            mock.patch.object(
                fix_ci,
                "run",
                return_value=subprocess.CompletedProcess([], 0, " M workflow.fabro\n", ""),
            ),
        ):
            with self.assertRaisesRegex(
                fix_ci.FixCIError, "tracked Factory files changed"
            ):
                fix_ci.assert_factory_unchanged({"factoryCommit": "a" * 40})


class FactoryWorkflowContractTest(unittest.TestCase):
    def test_factory_config_targets_interface_without_auto_pr(self) -> None:
        config = (WORKFLOW_ROOT / "workflow.toml").read_text()

        self.assertIn('target_repository = "interface"', config)
        self.assertIn('"veniceai/interface"', config)
        self.assertIn('contents = "write"', config)
        self.assertIn('pull_requests = "read"', config)
        self.assertEqual(config.count("enabled = false"), 2)

    def test_every_target_operation_uses_the_runtime_adapter(self) -> None:
        graph = (WORKFLOW_ROOT / "workflow.fabro").read_text()

        self.assertIn(
            "cd .fabro/workflows/fix-ci/runtime/target && node --version",
            graph,
        )
        self.assertIn('--repository \\"{{ inputs.target_repository }}\\"', graph)
        self.assertIn('--target \\"{{ inputs.target }}\\"', graph)
        self.assertEqual(graph.count("scripts/fix_ci.py"), 5)
        self.assertIn('retry_target="fix"', graph)
        self.assertIn(
            'final_verify -> publish [condition="outcome=succeeded"]', graph
        )
        self.assertNotIn("/tmp/fix-ci", graph)

    def test_each_success_path_requires_a_successful_predecessor(self) -> None:
        graph = (WORKFLOW_ROOT / "workflow.fabro").read_text()

        for edge in (
            "checkout -> setup",
            "setup -> verify",
            "verify -> lint_fix",
            "fix -> verify",
            "lint_fix -> final_verify",
            "final_verify -> publish",
        ):
            self.assertIn(f'{edge} [condition="outcome=succeeded"]', graph)
        for fallback in ("checkout -> exit", "setup -> exit", "fix -> exit"):
            self.assertIn(f"{fallback}\n", graph)
        self.assertIn("publish -> exit\n", graph)
        self.assertNotIn("checkout -> setup -> verify", graph)

    def test_sandbox_uses_the_shared_regular_image(self) -> None:
        dockerfile = (
            WORKFLOW_ROOT.parents[1] / "images/regular/Dockerfile"
        ).read_text()
        config = (WORKFLOW_ROOT / "workflow.toml").read_text()

        self.assertIn("ubuntu-24.04:slim-b253b0b6004f", dockerfile)
        self.assertIn("ARG NODE_VERSION=22.22.0", dockerfile)
        self.assertIn("ARG YARN_VERSION=4.12.0", dockerfile)
        self.assertIn("../../images/regular/Dockerfile", config)

    def test_prompt_confines_repairs_to_the_target_clone(self) -> None:
        prompt = (WORKFLOW_ROOT / "prompts" / "fix.md.j2").read_text()

        self.assertIn("runtime/target", prompt)
        self.assertIn("Read its `AGENTS.md`", prompt)
        self.assertIn("Do not edit the Factory repository", prompt)


if __name__ == "__main__":
    unittest.main()
