"""Source-level contracts for the Harbor nanocodex adapter."""

import asyncio
import json
import logging
import os
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import yaml
from harbor.models.agent.context import AgentContext

from harbor_adapter.agent import (
    DEFAULT_MODEL,
    SUPPORTED_MODELS,
    NanocodexAgent,
    _cli_tools_install_command,
    _remote_binary_install_command,
)
from harbor_adapter.codex import ParityCodexAgent
from harbor_adapter import environment as environment_adapter
from harbor_adapter.environment import (
    _ensure_task_image,
    _immutable_task_identity,
    _toolbox_mount_setup_command,
)
from harbor_adapter.verifier import (
    _VERIFIER_OVERLAY_PATH_VALIDATION,
    _toolbox_library_path_setup_command,
)


class CliToolInstallContractTests(unittest.TestCase):
    def test_leaderboard_install_provisions_the_codex_cli_toolset_and_cas(self) -> None:
        command = _cli_tools_install_command(install_node=True)

        for package in (
            "ca-certificates",
            "curl",
            "bash",
            "nodejs",
            "npm",
            "ripgrep",
        ):
            self.assertIn(package, command)
        for package_manager in ("apk add", "apt-get install", "yum install"):
            self.assertIn(package_manager, command)
        for executable in ("curl", "bash", "node", "npm", "rg"):
            self.assertIn(f"command -v {executable}", command)
        self.assertLess(command.index("command -v curl"), command.index("apk add"))
        self.assertIn("PATH=$PATH:/opt/nanocodex-verifier/bin", command)
        self.assertIn(
            "/opt/nanocodex-toolbox/etc/ssl/certs/ca-certificates.crt",
            command,
        )
        self.assertIn("/etc/pki/tls/certs/ca-bundle.crt", command)
        self.assertIn(
            '"/opt/nanocodex-toolbox/usr/share/nodejs"',
            command,
        )
        self.assertIn('rm "$node_module_entry"', command)

    def test_node_policy_keeps_node_and_npm_optional(self) -> None:
        command = _cli_tools_install_command(install_node=False)

        self.assertNotIn("nodejs", command)
        self.assertNotIn("/opt/nanocodex-toolbox/usr/share/nodejs", command)
        self.assertNotIn("command -v node", command)
        self.assertNotIn("command -v npm", command)
        for package in ("ca-certificates", "curl", "bash", "ripgrep"):
            self.assertIn(package, command)

    def test_preinstalled_tools_skip_package_managers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            certificate = root / "ca-certificates.crt"
            certificate.write_text("test certificate", encoding="utf-8")
            marker = root / "package-manager-ran"
            for command in ("curl", "bash", "rg", "node", "npm"):
                executable = bin_dir / command
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            for manager in ("apk", "apt-get", "yum"):
                executable = bin_dir / manager
                executable.write_text(
                    f"#!/bin/sh\ntouch {marker}\nexit 99\n", encoding="utf-8"
                )
                executable.chmod(0o755)

            command = _cli_tools_install_command(install_node=True).replace(
                "/etc/ssl/certs/ca-certificates.crt", str(certificate)
            )
            subprocess.run(
                ["sh", "-c", command],
                check=True,
                env={"PATH": f"{bin_dir}:/usr/bin:/bin"},
            )

            self.assertFalse(marker.exists())

    def test_native_curl_without_native_ca_uses_package_manager(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            task_certificate = root / "task-ca-certificates.crt"
            toolbox_certificate = root / "toolbox-ca-certificates.crt"
            toolbox_certificate.write_text("test certificate", encoding="utf-8")
            marker = root / "package-manager-ran"
            for command in ("curl", "bash", "rg"):
                executable = bin_dir / command
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            apt_get = bin_dir / "apt-get"
            apt_get.write_text(
                "#!/bin/sh\n"
                f"touch {marker}\n"
                f"printf 'test certificate' > {task_certificate}\n",
                encoding="utf-8",
            )
            apt_get.chmod(0o755)

            command = _cli_tools_install_command(install_node=False).replace(
                "/opt/nanocodex-toolbox/etc/ssl/certs/ca-certificates.crt",
                str(toolbox_certificate),
            ).replace(
                "/etc/ssl/certs/ca-certificates.crt",
                str(task_certificate),
            )
            subprocess.run(
                ["sh", "-c", command],
                check=True,
                env={"PATH": f"{bin_dir}:/usr/bin:/bin"},
            )

            self.assertTrue(marker.exists())
            self.assertTrue(task_certificate.exists())

    def test_toolbox_curl_with_toolbox_ca_skips_package_managers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            toolbox_bin = root / "toolbox-bin"
            bin_dir.mkdir()
            toolbox_bin.mkdir()
            task_certificate = root / "missing-task-ca-certificates.crt"
            toolbox_certificate = root / "toolbox-ca-certificates.crt"
            toolbox_certificate.write_text("test certificate", encoding="utf-8")
            marker = root / "package-manager-ran"
            for command in ("bash", "rg"):
                executable = bin_dir / command
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            curl = toolbox_bin / "curl"
            curl.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            curl.chmod(0o755)
            apt_get = bin_dir / "apt-get"
            apt_get.write_text(
                f"#!/bin/sh\ntouch {marker}\nexit 99\n", encoding="utf-8"
            )
            apt_get.chmod(0o755)

            command = (
                _cli_tools_install_command(install_node=False)
                .replace("/opt/nanocodex-verifier/bin", str(toolbox_bin))
                .replace(
                    "/opt/nanocodex-toolbox/etc/ssl/certs/ca-certificates.crt",
                    str(toolbox_certificate),
                )
                .replace(
                    "/etc/ssl/certs/ca-certificates.crt",
                    str(task_certificate),
                )
            )
            subprocess.run(
                ["/bin/sh", "-c", command],
                check=True,
                env={"PATH": str(bin_dir)},
            )

            self.assertFalse(marker.exists())
            self.assertFalse(task_certificate.exists())

    def test_yum_ca_bundle_is_an_accepted_native_certificate_location(self) -> None:
        command = _cli_tools_install_command(install_node=False)

        self.assertIn(
            "test -s /etc/ssl/certs/ca-certificates.crt || "
            "test -s /etc/pki/tls/certs/ca-bundle.crt",
            command,
        )

    def test_missing_tool_uses_package_manager_then_rechecks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            certificate = root / "ca-certificates.crt"
            certificate.write_text("test certificate", encoding="utf-8")
            marker = root / "package-manager-ran"
            for command in ("curl", "bash"):
                executable = bin_dir / command
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            apt_get = bin_dir / "apt-get"
            apt_get.write_text(
                "#!/bin/sh\n"
                f"touch {marker}\n"
                f"printf '#!/bin/sh\\nexit 0\\n' > {bin_dir / 'rg'}\n"
                f"chmod 0755 {bin_dir / 'rg'}\n",
                encoding="utf-8",
            )
            apt_get.chmod(0o755)

            command = _cli_tools_install_command(install_node=False).replace(
                "/etc/ssl/certs/ca-certificates.crt", str(certificate)
            )
            subprocess.run(
                ["sh", "-c", command],
                check=True,
                env={"PATH": f"{bin_dir}:/usr/bin:/bin"},
            )

            self.assertTrue(marker.exists())
            self.assertTrue((bin_dir / "rg").exists())

    def test_agent_install_applies_the_tool_policy_before_uploading(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "nanocodex"
            binary.touch()
            agent = object.__new__(NanocodexAgent)
            agent._binary_path = binary
            agent._install_node = True
            agent.exec_as_root = AsyncMock()
            environment = SimpleNamespace(upload_file=AsyncMock())

            asyncio.run(agent.install(environment))

        install_command = agent.exec_as_root.await_args_list[0].args[1]
        self.assertIn("ca-certificates", install_command)
        self.assertIn("nodejs", install_command)
        self.assertEqual(
            agent.exec_as_root.await_args_list[0].kwargs["env"],
            {"DEBIAN_FRONTEND": "noninteractive"},
        )
        environment.upload_file.assert_awaited_once_with(binary, agent._BINARY)
        self.assertEqual(
            agent.exec_as_root.await_args_list[1].args[1],
            "chmod 0755 /installed-agent/nanocodex",
        )

    def test_remote_binary_install_downloads_and_verifies_before_installing(
        self,
    ) -> None:
        checksum = "a" * 64
        command = _remote_binary_install_command(
            binary_url="https://example.test/nanocodex",
            binary_sha256=checksum,
            destination="/installed-agent/nanocodex",
        )

        self.assertIn("curl --fail --location --retry 5", command)
        self.assertIn("https://example.test/nanocodex", command)
        self.assertIn("sha256sum", command)
        self.assertIn(checksum, command)
        self.assertIn("chmod 0755 /installed-agent/nanocodex", command)

    def test_remote_binary_install_skips_controller_upload(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._binary_path = Path("/missing-on-controller")
        agent._binary_url = "https://example.test/nanocodex"
        agent._binary_sha256 = "b" * 64
        agent._install_node = True
        agent.exec_as_root = AsyncMock()
        environment = SimpleNamespace(upload_file=AsyncMock())

        asyncio.run(agent.install(environment))

        environment.upload_file.assert_not_awaited()
        self.assertEqual(agent.exec_as_root.await_count, 2)
        download_command = agent.exec_as_root.await_args_list[1].args[1]
        self.assertIn(agent._binary_url, download_command)
        self.assertIn(agent._binary_sha256, download_command)


class ModelContractTests(unittest.TestCase):
    def test_supported_models_are_exactly_sol_and_luna(self) -> None:
        self.assertEqual(
            SUPPORTED_MODELS,
            {"gpt-5.6-sol", "gpt-5.6-luna"},
        )
        self.assertEqual(DEFAULT_MODEL, "gpt-5.6-sol")
        self.assertEqual(NanocodexAgent._api_model_name(None), DEFAULT_MODEL)
        self.assertEqual(
            NanocodexAgent._api_model_name("openai/gpt-5.6-luna"),
            "gpt-5.6-luna",
        )

    def test_run_arguments_forward_the_selected_model(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._model = "gpt-5.6-luna"
        agent._effort = "low"
        agent._fast_mode = False
        agent._web_search = False
        agent._subagents = False

        self.assertEqual(
            agent._run_arguments("test prompt")[2:4],
            ["--model", "gpt-5.6-luna"],
        )

    def test_run_arguments_forward_fast_mode_explicitly(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._model = DEFAULT_MODEL
        agent._effort = "low"
        agent._web_search = False
        agent._subagents = False

        agent._fast_mode = True
        arguments = agent._run_arguments("test prompt")
        fast_mode = arguments.index("--fast-mode")
        self.assertEqual(arguments[fast_mode : fast_mode + 2], ["--fast-mode", "true"])

        agent._fast_mode = False
        arguments = agent._run_arguments("test prompt")
        fast_mode = arguments.index("--fast-mode")
        self.assertEqual(arguments[fast_mode : fast_mode + 2], ["--fast-mode", "false"])

    def test_constructor_rejects_models_outside_the_supported_pair(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "supports only gpt-5.6-luna, gpt-5.6-sol"):
                NanocodexAgent(
                    logs_dir=Path(directory),
                    model_name="openai/gpt-5.6-terra",
                    extra_env={"OPENAI_API_KEY": "test-key"},
                )


class RolloutContractTests(unittest.TestCase):
    def test_eval_runs_always_disable_rollout_persistence(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._model = DEFAULT_MODEL
        agent._effort = "low"
        agent._web_search = False
        agent._subagents = False

        arguments = agent._run_arguments("test prompt")
        rollouts = arguments.index("--rollouts")

        self.assertEqual(arguments[rollouts : rollouts + 2], ["--rollouts", "false"])


class WebSearchContractTests(unittest.TestCase):
    def test_run_arguments_always_pass_web_search_explicitly(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._model = DEFAULT_MODEL
        agent._effort = "low"
        agent._fast_mode = False

        agent._web_search = True
        agent._subagents = False
        self.assertEqual(
            agent._run_arguments("test prompt")[-6:],
            ["--web-search", "true", "--subagents", "false", "--", "test prompt"],
        )

        agent._web_search = False
        agent._subagents = True
        self.assertEqual(
            agent._run_arguments("test prompt")[-6:],
            ["--web-search", "false", "--subagents", "true", "--", "test prompt"],
        )

    def test_run_arguments_protect_a_prompt_that_starts_with_a_hyphen(self) -> None:
        agent = object.__new__(NanocodexAgent)
        agent._model = DEFAULT_MODEL
        agent._effort = "low"
        agent._fast_mode = False
        agent._web_search = False
        agent._subagents = False

        self.assertEqual(
            agent._run_arguments("- benchmark instruction")[-2:],
            ["--", "- benchmark instruction"],
        )

    def test_terminal_bench_disables_web_search(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        config = yaml.safe_load(
            (repository / "evals" / "terminal-bench-2.yaml").read_text(encoding="utf-8")
        )

        self.assertIs(config["agents"][0]["kwargs"]["web_search"], False)

    def test_fast_eval_enables_priority_processing(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        config = yaml.safe_load(
            (repository / "evals" / "terminal-bench-2-1-fast.yaml").read_text(
                encoding="utf-8"
            )
        )

        kwargs = config["agents"][0]["kwargs"]
        self.assertIs(kwargs["fast_mode"], True)
        self.assertIs(kwargs["web_search"], False)

    def test_terminal_bench_arms_do_not_enable_subagents(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        for filename in (
            "terminal-bench-2-1.yaml",
            "terminal-bench-2-1-high-failures.yaml",
        ):
            config = yaml.safe_load(
                (repository / "evals" / filename).read_text(encoding="utf-8")
            )
            self.assertNotIn("subagents", config["agents"][0]["kwargs"])

class ContextParityContractTests(unittest.TestCase):
    def test_history_eval_arms_use_the_same_context_files(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        nanocodex_config = yaml.safe_load(
            (repository / "evals" / "history-derived.yaml").read_text(encoding="utf-8")
        )
        codex_config = yaml.safe_load(
            (repository / "evals" / "history-derived-codex.yaml").read_text(
                encoding="utf-8"
            )
        )

        nanocodex_kwargs = nanocodex_config["agents"][0]["kwargs"]
        codex_kwargs = codex_config["agents"][0]["kwargs"]
        self.assertEqual(
            nanocodex_kwargs["system_prompt_path"],
            codex_kwargs["system_prompt_path"],
        )
        self.assertEqual(
            nanocodex_kwargs["agents_md_path"], codex_kwargs["agents_md_path"]
        )
        self.assertEqual(
            codex_config["agents"][0]["import_path"],
            "harbor_adapter.codex:ParityCodexAgent",
        )

    def test_codex_override_points_at_uploaded_system_prompt(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as directory:
            agent = ParityCodexAgent(
                logs_dir=Path(directory),
                model_name="openai/test-model",
                system_prompt_path=repository / "crates/nanocodex-oai-api/prompts/system.md",
                agents_md_path=repository / "evals/history-derived/AGENTS.md",
                reasoning_effort="low",
                web_search="disabled",
            )

        self.assertIn(
            '-c model_instructions_file="/tmp/nanocodex-system-prompt.md"',
            agent.build_cli_flags(),
        )


class VerifierOverlayContractTests(unittest.TestCase):
    def test_task_toolchain_precedes_cached_verifier_fallbacks(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("export PATH=$PATH:/opt/nanocodex-verifier/bin", verifier)
        self.assertNotIn("export PATH=/opt/nanocodex-verifier/bin:$PATH", verifier)

    def test_regex_chess_uses_the_exact_cached_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/chess/3.13", dockerfile)
        self.assertIn("chess==1.11.2", dockerfile)
        self.assertIn('"-p 3.13 -w pytest==8.4.1 -w chess==1.11.2 ', verifier)
        self.assertIn(
            "verifier_pythonpath={_CHESS_VERIFIER_SITE_PACKAGES}/3.13", verifier
        )

    def test_torch_parallelism_uses_exact_shared_cached_overlays(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertEqual(dockerfile.count("torch==2.7.0"), 1)
        self.assertIn("--target /opt/nanocodex-verifier/torch270/3.13", dockerfile)
        self.assertIn("--target /opt/nanocodex-verifier/transformers455/3.13", dockerfile)
        self.assertIn("transformers==4.55.0", dockerfile)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w torch==2.7.0 '
            '-w transformers==4.55.0 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_TRANSFORMERS455_VERIFIER_SITE_PACKAGES}/3.13:",
            verifier,
        )
        self.assertIn("{_TORCH270_VERIFIER_SITE_PACKAGES}/3.13 ;;", verifier)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w torch==2.7.0 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_TORCH270_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_overlay_validation_accepts_each_colon_separated_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            transformers = root / "transformers455" / "3.13"
            torch = root / "torch270" / "3.13"
            transformers.mkdir(parents=True)
            torch.mkdir(parents=True)
            validator = (
                'validate() { local verifier_pythonpath="$1" '
                "verifier_overlay_path= verifier_overlay_ifs=; "
                f"{_VERIFIER_OVERLAY_PATH_VALIDATION}"
                '}; validate "$1"'
            )

            for overlay in (str(torch), f"{transformers}:{torch}"):
                with self.subTest(overlay=overlay):
                    result = subprocess.run(
                        ["bash", "-c", validator, "bash", overlay],
                        check=False,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)

            missing = root / "missing" / "3.13"
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    validator,
                    "bash",
                    f"{transformers}:{missing}",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 127)
            self.assertIn(str(missing), result.stderr)

    def test_uvx_python_inherits_task_and_toolbox_library_paths(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        with tempfile.TemporaryDirectory() as directory:
            library_dir = Path(directory) / "verifier root" / "lib"
            library_dir.mkdir(parents=True)

            setup = _toolbox_library_path_setup_command(str(library_dir))
            script = (
                f"{setup}\n"
                'printf "%s\\n" "$verifier_toolbox_library_path"\n'
                'printf "%s\\n" "${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}'
                '$verifier_toolbox_library_path"'
            )
            task_library_path = "/task/lib:/agent/lib"
            result = subprocess.run(
                ["bash", "-c", script],
                check=True,
                capture_output=True,
                text=True,
                env={**os.environ, "LD_LIBRARY_PATH": task_library_path},
            )

        toolbox_library_path, combined_library_path = result.stdout.splitlines()
        expected_toolbox_path = str(library_dir)
        self.assertEqual(toolbox_library_path, expected_toolbox_path)
        self.assertEqual(
            combined_library_path,
            f"{task_library_path}:{expected_toolbox_path}",
        )
        self.assertIn(
            'env LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}',
            verifier,
        )
        self.assertIn('$verifier_toolbox_library_path" ', verifier)

        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "apt-get install --yes --no-install-recommends libgl1", dockerfile
        )
        self.assertIn("/opt/nanocodex-verifier/lib/$library", dockerfile)
        self.assertNotIn("libc.so.6", dockerfile)

    def test_filter_js_uses_its_exact_cached_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/filter-js/3.13", dockerfile)
        self.assertIn("selenium==4.38.0", dockerfile)
        self.assertIn("bs4==0.0.2", dockerfile)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w selenium==4.38.0 '
            '-w bs4==0.0.2 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_FILTER_JS_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_reshard_c4_uses_its_exact_cached_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/datasets360/3.13", dockerfile)
        self.assertIn("datasets==3.6.0", dockerfile)
        self.assertIn("tqdm==4.67.1", dockerfile)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w datasets==3.6.0 '
            '-w tqdm==4.67.1 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_DATASETS360_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_mteb_retrieve_uses_its_exact_cached_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertNotIn("mteb==1.36.8", dockerfile)
        self.assertIn(
            '"-p 3.13 -w pytest==8.4.1 -w mteb==1.36.8 ',
            verifier,
        )
        self.assertIn('-w pytest-json-ctrf==0.3.5 pytest "*) ;; ', verifier)

    def test_mcmc_stan_uses_its_exact_pytest_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/pytest834/3.13", dockerfile)
        self.assertIn("pytest==8.3.4", dockerfile)
        self.assertIn('"-p 3.13 -w pytest==8.3.4 ', verifier)
        self.assertIn(
            "verifier_pythonpath={_PYTEST834_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_mips_tasks_and_path_tracing_share_the_exact_pillow_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/pillow1121/3.13", dockerfile)
        self.assertEqual(dockerfile.count("pillow==11.2.1"), 1)
        self.assertIn("numpy==2.3.1", dockerfile)
        self.assertIn('"install -y curl python3-pillow"', verifier)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w numpy==2.3.1 '
            '-w pillow==11.2.1 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w pillow==11.2.1 '
            '-w numpy==2.3.1 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertEqual(
            verifier.count(
                "verifier_pythonpath={_PILLOW1121_VERIFIER_SITE_PACKAGES}/3.13"
            ),
            2,
        )

    def test_bn_and_financial_document_tasks_share_the_pandas_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/bn/3.13", dockerfile)
        self.assertIn("pandas==2.3.2", dockerfile)
        self.assertIn("scipy==1.16.1", dockerfile)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w pandas==2.3.2 '
            '-w scipy==1.16.1 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn("verifier_pythonpath={_BN_VERIFIER_SITE_PACKAGES}/3.13", verifier)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w pandas==2.3.2 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertEqual(
            verifier.count("verifier_pythonpath={_BN_VERIFIER_SITE_PACKAGES}/3.13"),
            2,
        )

    def test_feal_uses_its_exact_setuptools_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/setuptools809/3.13", dockerfile)
        self.assertIn("setuptools==80.9.0", dockerfile)
        self.assertIn('"install -y curl gcc"', verifier)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 -w setuptools==80.9.0 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_SETUPTOOLS809_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_pytorch_cli_uses_its_exact_cpu_index_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/pytorch-cli/3.13", dockerfile)
        self.assertIn("--index https://download.pytorch.org/whl/cpu", dockerfile)
        self.assertIn("--index-strategy unsafe-best-match", dockerfile)
        self.assertIn("torch==2.7.1", dockerfile)
        self.assertIn("torchvision==0.22.1", dockerfile)
        self.assertIn("opencv-python==4.11.0.86", dockerfile)
        self.assertIn('"install -y curl ffmpeg libsm6 libxext6"', verifier)
        self.assertIn(
            """'"--index https://download.pytorch.org/whl/cpu '
            '--index-strategy unsafe-best-match -p 3.13 '
            '-w torch==2.7.1 -w torchvision==0.22.1 '
            '-w numpy==2.3.1 -w opencv-python==4.11.0.86 '
            '-w pytest==8.4.1 -w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_PYTORCH_CLI_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_native_build_tasks_use_the_exact_cached_toolchain(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )
        toolbox_exec = (
            repository / "evals" / "pytest" / "bin" / "toolbox-exec"
        ).read_text(encoding="utf-8")

        self.assertIn('"install -y curl gcc"', verifier)
        self.assertIn("        gcc \\", dockerfile)
        self.assertIn("        libc6-dev", dockerfile)
        self.assertIn("for command in as bash curl expect gcc git ld", dockerfile)
        self.assertIn('export GCC_EXEC_PREFIX="$root/usr/lib/gcc/"', toolbox_exec)
        self.assertIn('set -- "--sysroot=$root" "$@"', toolbox_exec)
        self.assertIn(
            'LD_LIBRARY_PATH="${LD_LIBRARY_PATH:+$LD_LIBRARY_PATH:}$library_path"',
            toolbox_exec,
        )

    def test_install_windows_311_uses_its_exact_cached_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )
        toolbox_exec = (
            repository / "evals" / "pytest" / "bin" / "toolbox-exec"
        ).read_text(encoding="utf-8")

        self.assertIn(
            "--target /opt/nanocodex-verifier/install-windows-311/3.13",
            dockerfile,
        )
        self.assertIn("opencv-python==4.11.0.86", dockerfile)
        self.assertIn("numpy==2.3.1", dockerfile)
        self.assertIn("pytesseract==0.3.13", dockerfile)
        self.assertIn('"update -qq"', verifier)
        self.assertIn('"install -y mtools socat vncsnapshot tesseract-ocr"', verifier)
        self.assertIn("mcopy mdir", dockerfile)
        self.assertIn("socat ssh sshpass tesseract vim vncsnapshot", dockerfile)
        self.assertIn("TESSDATA_PREFIX", toolbox_exec)
        self.assertIn(
            """'"-p 3.13 -w pytest==8.4.1 '
            '-w opencv-python==4.11.0.86 -w numpy==2.3.1 '
            '-w pytesseract==0.3.13 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "{_INSTALL_WINDOWS_311_VERIFIER_SITE_PACKAGES}/3.13",
            verifier,
        )

    def test_train_fasttext_uses_its_exact_python311_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/fasttext/3.11", dockerfile)
        self.assertIn("scikit-learn==1.7.0", dockerfile)
        self.assertIn("fasttext-wheel==0.9.2", dockerfile)
        self.assertIn("numpy==1.24.0", dockerfile)
        self.assertIn(
            """'"-p 3.11 -w pytest==8.4.1 -w scikit-learn==1.7.0 '
            '-w fasttext-wheel==0.9.2 -w numpy==1.24.0 '
            '-w pytest-json-ctrf==0.3.5 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_FASTTEXT_VERIFIER_SITE_PACKAGES}/3.11",
            verifier,
        )
        self.assertIn("verifier_uvx_python={_MANAGED_VERIFIER_PYTHON_311}", verifier)
        self.assertIn(
            "verifier_uvx_base_pythonpath={_BASE_VERIFIER_SITE_PACKAGES}/3.11",
            verifier,
        )

    def test_git_webserver_supports_its_absolute_expect_shebang(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("expect", dockerfile)
        self.assertIn('"install -y expect"', verifier)
        self.assertIn(
            "ln -s /opt/nanocodex-verifier/bin/expect /usr/bin/expect", verifier
        )
        self.assertIn("trap verifier_cleanup EXIT", verifier)
        self.assertIn("readlink /usr/bin/expect", verifier)
        self.assertIn('"/opt/nanocodex-verifier/bin/expect" ]; then', verifier)

    def test_sam_cell_seg_uses_its_exact_python311_overlay(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        verifier = (repository / "harbor_adapter" / "verifier.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("--target /opt/nanocodex-verifier/sam-cell-seg/3.11", dockerfile)
        self.assertIn("torch==2.5.1", dockerfile)
        self.assertIn("torchvision==0.20.1", dockerfile)
        self.assertIn("timm==1.0.19", dockerfile)
        self.assertIn("opencv-python==4.12.0.88", dockerfile)
        self.assertIn("shapely==2.1.1", dockerfile)
        self.assertIn(
            "git+https://github.com/ChaoningZhang/MobileSAM.git@"
            "34bbbfdface3c18e5221aa7de6032d7220c6c6a1",
            dockerfile,
        )
        self.assertIn('"install -y curl git libgl1"', verifier)
        self.assertIn(
            """'"--index https://download.pytorch.org/whl/cpu '
            '--index-strategy unsafe-best-match -p 3.11 '
            '-w torch==2.5.1 -w torchvision==0.20.1 '
            '-w pandas==2.3.2 -w timm==1.0.19 '
            '-w opencv-python==4.12.0.88 -w shapely==2.1.1 '
            '-w pytest==8.4.1 -w pytest-json-ctrf==0.3.5 '
            '-w git+https://github.com/ChaoningZhang/MobileSAM.git@'
            '34bbbfdface3c18e5221aa7de6032d7220c6c6a1 pytest "*) '""",
            verifier,
        )
        self.assertIn(
            "verifier_pythonpath={_SAM_CELL_SEG_VERIFIER_SITE_PACKAGES}/3.11",
            verifier,
        )
        self.assertIn("verifier_uvx_python={_TASK_VERIFIER_PYTHON}", verifier)
        self.assertIn(
            "verifier_uvx_base_pythonpath={_BASE_VERIFIER_SITE_PACKAGES}/3.11",
            verifier,
        )


class EnvironmentToolboxContractTests(unittest.TestCase):
    def test_toolbox_exposes_every_agent_install_fallback(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        dockerfile = (repository / "evals" / "pytest" / "Dockerfile").read_text(
            encoding="utf-8"
        )
        toolbox_exec = (
            repository / "evals" / "pytest" / "bin" / "toolbox-exec"
        ).read_text(encoding="utf-8")

        self.assertIn("as bash curl", dockerfile)
        self.assertIn("node npm readelf rg", dockerfile)
        self.assertIn("nodejs \\\n        npm", dockerfile)
        self.assertIn('npm)', toolbox_exec)
        self.assertIn('npm/bin/npm-cli.js', toolbox_exec)

    def test_node_modules_fast_path_and_merge_preserve_task_entries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            toolbox = root / "toolbox"
            toolbox_verifier = toolbox / "opt" / "nanocodex-verifier"
            toolbox_modules = toolbox / "usr" / "share" / "nodejs"
            toolbox_verifier.mkdir(parents=True)
            (toolbox_modules / "font-awesome").mkdir(parents=True)
            (toolbox_modules / "acorn").mkdir()

            for has_task_modules in (False, True):
                with self.subTest(has_task_modules=has_task_modules):
                    task_root = root / f"task-{has_task_modules}"
                    verifier = task_root / "opt" / "nanocodex-verifier"
                    task_modules = task_root / "usr" / "share" / "nodejs"
                    bash = task_root / "bin" / "bash"
                    verifier.parent.mkdir(parents=True)
                    task_modules.parent.mkdir(parents=True)
                    bash.parent.mkdir(parents=True)
                    if has_task_modules:
                        task_modules.mkdir()
                        owned_module = task_modules / "font-awesome"
                        owned_module.mkdir()
                        (owned_module / "task-owned").write_text(
                            "preserve", encoding="utf-8"
                        )

                    subprocess.run(
                        [
                            "sh",
                            "-c",
                            _toolbox_mount_setup_command(
                                toolbox_root=str(toolbox),
                                verifier_root=str(verifier),
                                node_modules_root=str(task_modules),
                                bash_path=str(bash),
                            ),
                        ],
                        check=True,
                    )

                    self.assertEqual(verifier.readlink(), toolbox_verifier)
                    if has_task_modules:
                        self.assertFalse(task_modules.is_symlink())
                        self.assertEqual(
                            (task_modules / "font-awesome" / "task-owned").read_text(
                                encoding="utf-8"
                            ),
                            "preserve",
                        )
                        self.assertEqual(
                            (task_modules / "acorn").readlink(),
                            toolbox_modules / "acorn",
                        )
                    else:
                        self.assertEqual(task_modules.readlink(), toolbox_modules)
                    self.assertEqual(bash.readlink(), verifier / "bin" / "bash")


class TaskImageCacheContractTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        environment_adapter._TASK_IMAGES.clear()
        environment_adapter._TASK_IMAGE_LOCKS.clear()

    def tearDown(self) -> None:
        environment_adapter._TASK_IMAGES.clear()
        environment_adapter._TASK_IMAGE_LOCKS.clear()

    async def test_prepared_package_image_survives_a_fresh_process_cache(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_cache = root / "packages"
            environment_dir = (
                package_cache / "org" / "task" / ("a" * 64) / "environment"
            )
            environment_dir.mkdir(parents=True)
            dockerfile = environment_dir / "Dockerfile"
            dockerfile.touch()
            records = root / "records"
            build = AsyncMock(return_value="nanocodex/task:prepared")

            with (
                patch.object(environment_adapter, "PACKAGE_CACHE_DIR", package_cache),
                patch.object(environment_adapter, "_TASK_IMAGE_RECORDS_DIR", records),
                patch.object(environment_adapter, "ensure_docker_image_built", build),
                patch.object(
                    environment_adapter,
                    "docker_image_exists",
                    AsyncMock(return_value=True),
                ) as image_exists,
            ):
                first = await _ensure_task_image(
                    environment_name="task",
                    environment_dir=environment_dir,
                    dockerfile_path=dockerfile,
                    platform="linux/amd64",
                    logger=logging.getLogger(__name__),
                )
                environment_adapter._TASK_IMAGES.clear()
                second = await _ensure_task_image(
                    environment_name="task",
                    environment_dir=environment_dir,
                    dockerfile_path=dockerfile,
                    platform="linux/amd64",
                    logger=logging.getLogger(__name__),
                )

            self.assertEqual(first, "nanocodex/task:prepared")
            self.assertEqual(second, first)
            build.assert_awaited_once()
            image_exists.assert_awaited_once_with(first)
            self.assertEqual(len(list(records.glob("*.json"))), 1)

    async def test_missing_prepared_image_rebuilds_and_replaces_record(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_cache = root / "packages"
            environment_dir = (
                package_cache / "org" / "task" / ("b" * 64) / "environment"
            )
            environment_dir.mkdir(parents=True)
            dockerfile = environment_dir / "Dockerfile"
            dockerfile.touch()
            records = root / "records"

            with (
                patch.object(environment_adapter, "PACKAGE_CACHE_DIR", package_cache),
                patch.object(environment_adapter, "_TASK_IMAGE_RECORDS_DIR", records),
                patch.object(
                    environment_adapter,
                    "ensure_docker_image_built",
                    AsyncMock(side_effect=["old-image", "new-image"]),
                ) as build,
                patch.object(
                    environment_adapter,
                    "docker_image_exists",
                    AsyncMock(return_value=False),
                ),
            ):
                await _ensure_task_image(
                    environment_name="task",
                    environment_dir=environment_dir,
                    dockerfile_path=dockerfile,
                    platform="linux/arm64",
                    logger=logging.getLogger(__name__),
                )
                environment_adapter._TASK_IMAGES.clear()
                image = await _ensure_task_image(
                    environment_name="task",
                    environment_dir=environment_dir,
                    dockerfile_path=dockerfile,
                    platform="linux/arm64",
                    logger=logging.getLogger(__name__),
                )

            self.assertEqual(image, "new-image")
            self.assertEqual(build.await_count, 2)
            record = json.loads(next(records.glob("*.json")).read_text())
            self.assertEqual(record["image"], "new-image")

    def test_mutable_local_task_does_not_receive_a_persistent_identity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            environment_dir = Path(directory) / "environment"
            environment_dir.mkdir()

            self.assertIsNone(_immutable_task_identity(environment_dir))


class InterruptedRunContractTests(unittest.TestCase):
    @staticmethod
    def _agent(logs_dir: Path, *, interrupted: bool) -> NanocodexAgent:
        agent = object.__new__(NanocodexAgent)
        agent.logs_dir = logs_dir
        agent._run_interrupted = interrupted
        agent._run_failed = False
        agent.logger = logging.getLogger("harbor_adapter.test_agent")
        return agent

    @staticmethod
    def _write_partial_stream(logs_dir: Path) -> None:
        prompt = {"instruction": "test"}
        event = {
            "protocol_version": 1,
            "request_id": "request-1",
            "seq": 1,
            "type": "run.started",
            "payload": {},
        }
        (logs_dir / "input.jsonl").write_text(
            json.dumps(prompt) + "\n", encoding="utf-8"
        )
        (logs_dir / "events.jsonl").write_text(
            json.dumps(event) + "\n", encoding="utf-8"
        )

    def test_partial_stream_remains_invalid_after_normal_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            self._write_partial_stream(logs_dir)
            agent = self._agent(logs_dir, interrupted=False)

            with self.assertRaisesRegex(
                RuntimeError, "expected exactly one terminal event, found 0"
            ):
                agent.populate_context_post_run(AgentContext())

    def test_partial_stream_is_best_effort_after_run_cancellation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            self._write_partial_stream(logs_dir)
            agent = self._agent(logs_dir, interrupted=True)
            context = AgentContext()

            agent.populate_context_post_run(context)

            self.assertTrue(context.is_empty())

    def test_empty_stream_is_best_effort_after_agent_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            (logs_dir / "input.jsonl").write_text(
                json.dumps({"instruction": "test"}) + "\n", encoding="utf-8"
            )
            (logs_dir / "events.jsonl").write_text("", encoding="utf-8")
            agent = self._agent(logs_dir, interrupted=False)
            agent._run_failed = True
            context = AgentContext()

            agent.populate_context_post_run(context)

            self.assertTrue(context.is_empty())

    def test_malformed_stream_remains_invalid_after_normal_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            self._write_partial_stream(logs_dir)
            (logs_dir / "events.jsonl").write_text("not-json\n", encoding="utf-8")
            agent = self._agent(logs_dir, interrupted=False)

            with self.assertRaises(RuntimeError):
                agent.populate_context_post_run(AgentContext())

    def test_recovery_suppresses_a_repeated_normal_exit_validation_failure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs_dir = Path(directory)
            self._write_partial_stream(logs_dir)
            (logs_dir / "events.jsonl").write_text("not-json\n", encoding="utf-8")
            agent = self._agent(logs_dir, interrupted=False)

            with self.assertRaises(RuntimeError):
                agent.populate_context_post_run(AgentContext())

            context = AgentContext()
            agent.populate_context_post_run(context)

            self.assertTrue(context.is_empty())

    def test_jsonl_read_retries_a_slowly_propagated_final_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            complete = {"type": "run.completed", "payload": {}}
            path.write_text('{"type":"run.com', encoding="utf-8")

            def finish_write() -> None:
                # Longer than the old adapter's complete retry window.
                time.sleep(0.6)
                path.write_text(json.dumps(complete) + "\n", encoding="utf-8")

            writer = threading.Thread(target=finish_write)
            writer.start()
            try:
                self.assertEqual(NanocodexAgent._read_jsonl(path), [complete])
            finally:
                writer.join()

    def test_jsonl_read_rejects_stable_malformed_records_immediately(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.jsonl"
            path.write_text("not-json\n", encoding="utf-8")

            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "failed to read JSONL"):
                NanocodexAgent._read_jsonl(path)

            self.assertLess(time.monotonic() - started, 0.5)


class RunCancellationContractTests(unittest.IsolatedAsyncioTestCase):
    async def test_run_atomically_publishes_captured_stdout_on_the_host(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = object.__new__(NanocodexAgent)
            agent.logs_dir = Path(directory)
            agent.context_id = None
            agent.session_id = "session-1"
            agent._model = "test-model"
            agent._effort = "low"
            agent._web_search = False
            agent._subagents = False
            agent._agents_md_path = None
            agent._stage_api_key = AsyncMock()
            agent._remove_staged_api_key = AsyncMock()
            stream = '{"type":"run.completed","payload":{}}\n'
            agent.exec_as_agent = AsyncMock(
                return_value=SimpleNamespace(stdout=stream, stderr="")
            )
            environment = SimpleNamespace(capabilities=SimpleNamespace(mounted=True))
            event_spool = agent.logs_dir / "events.jsonl.tmp"
            event_spool.write_text(stream, encoding="utf-8")

            await agent.run("test", environment, AgentContext())

            command = agent.exec_as_agent.await_args.args[1]
            self.assertIn("set -o pipefail", command)
            self.assertIn('SSL_CERT_FILE="$ca_bundle"', command)
            self.assertIn(
                "/opt/nanocodex-toolbox/etc/ssl/certs/ca-certificates.crt",
                command,
            )
            self.assertIn('tee "$events_tmp"', command)
            self.assertNotIn("tee /logs/agent/events.jsonl", command)
            self.assertNotIn('mv "$events_tmp"', command)
            self.assertEqual(
                (agent.logs_dir / "events.jsonl").read_text(encoding="utf-8"),
                stream,
            )
            self.assertFalse((agent.logs_dir / "events.jsonl.host.tmp").exists())
            self.assertFalse(event_spool.exists())

    def test_nonzero_exit_publishes_captured_stdout_before_classification(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = object.__new__(NanocodexAgent)
            agent.logs_dir = Path(directory)
            agent.logger = logging.getLogger("harbor_adapter.test_agent")
            agent._compiled_error_patterns = []
            stream = '{"type":"run.failed","payload":{}}\n'
            result = SimpleNamespace(return_code=1, stdout=stream, stderr="")

            error = agent._classify_exec_error("test-command", result)

            self.assertIsInstance(error, RuntimeError)
            self.assertEqual(
                (agent.logs_dir / "events.jsonl").read_text(encoding="utf-8"),
                stream,
            )
            self.assertFalse((agent.logs_dir / "events.jsonl.host.tmp").exists())

    async def test_cancellation_is_recorded_and_reraised(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = object.__new__(NanocodexAgent)
            agent.logs_dir = Path(directory)
            agent.context_id = None
            agent.session_id = "session-1"
            agent._model = "test-model"
            agent._effort = "low"
            agent._web_search = False
            agent._stage_api_key = AsyncMock()
            agent._remove_staged_api_key = AsyncMock()
            agent.exec_as_agent = AsyncMock(side_effect=asyncio.CancelledError())
            environment = SimpleNamespace(capabilities=SimpleNamespace(mounted=True))

            with self.assertRaises(asyncio.CancelledError):
                await agent.run("test", environment, AgentContext())

            self.assertTrue(agent._run_interrupted)
            agent._remove_staged_api_key.assert_awaited_once_with(environment)


if __name__ == "__main__":
    unittest.main()
