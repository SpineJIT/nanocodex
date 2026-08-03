"""Docker optimizations for the local Harbor loop."""

import asyncio
import hashlib
import json
import os
import re
import shlex
from pathlib import Path
from typing import Any, override

from harbor.constants import PACKAGE_CACHE_DIR
from harbor.environments.docker.docker import DockerEnvironment
from harbor.environments.docker.utils import (
    default_docker_platform,
    docker_image_exists,
    ensure_docker_image_built,
)
from harbor.models.trial.config import ServiceVolumeConfig

_TOOLBOX_ROOT = "/opt/nanocodex-toolbox"
_VERIFIER_ROOT = "/opt/nanocodex-verifier"
_TOOLBOX_BUILD_LOCK = asyncio.Lock()
_TOOLBOX_IMAGES: dict[tuple[Path, str], str] = {}
_TASK_IMAGE_LOCKS: dict[tuple[str, str], asyncio.Lock] = {}
_TASK_IMAGES: dict[tuple[str, str], str] = {}
_TASK_IMAGE_RECORDS_DIR = Path(".nanocodex/harbor/task-images")
_CONTENT_HASH_RE = re.compile(r"^[0-9a-f]{64}$")


def _immutable_task_identity(environment_dir: Path) -> str | None:
    """Return an identity only for Harbor's content-addressed package cache."""
    try:
        relative = environment_dir.resolve().relative_to(PACKAGE_CACHE_DIR.resolve())
    except ValueError:
        return None
    parts = relative.parts
    if len(parts) != 4 or parts[-1] != "environment":
        return None
    if not _CONTENT_HASH_RE.fullmatch(parts[-2]):
        return None
    return "/".join(parts[:-1])


def _task_image_record_path(identity: str, platform: str) -> Path:
    key = hashlib.sha256(f"{identity}\0{platform}".encode()).hexdigest()
    return _TASK_IMAGE_RECORDS_DIR / f"{key}.json"


async def _prepared_task_image(identity: str, platform: str) -> str | None:
    path = _task_image_record_path(identity, platform)
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
        if record.get("identity") != identity or record.get("platform") != platform:
            return None
        image = record.get("image")
        if not isinstance(image, str) or not image:
            return None
    except (AttributeError, json.JSONDecodeError, OSError):
        return None
    return image if await docker_image_exists(image) else None


def _record_task_image(identity: str, platform: str, image: str) -> None:
    path = _task_image_record_path(identity, platform)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        json.dumps(
            {"version": 1, "identity": identity, "platform": platform, "image": image},
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


async def _ensure_task_image(
    *,
    environment_name: str,
    environment_dir: Path,
    dockerfile_path: Path,
    platform: str,
    logger: Any,
) -> str:
    identity = _immutable_task_identity(environment_dir)
    cache_identity = identity or str(environment_dir.resolve())
    key = (cache_identity, platform)
    lock = _TASK_IMAGE_LOCKS.setdefault(key, asyncio.Lock())
    async with lock:
        if image := _TASK_IMAGES.get(key):
            return image
        if identity and (image := await _prepared_task_image(identity, platform)):
            _TASK_IMAGES[key] = image
            return image
        image = await ensure_docker_image_built(
            docker_name=f"nanocodex/{environment_name}-task",
            docker_build_context=environment_dir,
            dockerfile_path=dockerfile_path,
            build_args={},
            platform=platform,
            logger=logger,
        )
        _TASK_IMAGES[key] = image
        if identity:
            _record_task_image(identity, platform, image)
        return image


def _toolbox_mount_setup_command(
    *,
    toolbox_root: str = _TOOLBOX_ROOT,
    verifier_root: str = _VERIFIER_ROOT,
    node_modules_root: str = "/usr/share/nodejs",
) -> str:
    toolbox_verifier_root = f"{toolbox_root}{_VERIFIER_ROOT}"
    toolbox_node_modules = f"{toolbox_root}/usr/share/nodejs"
    verifier = shlex.quote(verifier_root)
    toolbox_verifier = shlex.quote(toolbox_verifier_root)
    node_modules = shlex.quote(node_modules_root)
    toolbox_modules = shlex.quote(toolbox_node_modules)
    return (
        f"if [ -e {verifier} ] || [ -L {verifier} ]; then "
        f'test "$(readlink {verifier})" = {toolbox_verifier}; '
        f"else ln -s {toolbox_verifier} {verifier}; fi; "
        f"if [ ! -e {node_modules} ] && [ ! -L {node_modules} ]; then "
        f"ln -s {toolbox_modules} {node_modules}; "
        f"elif [ -d {node_modules} ] && [ ! -L {node_modules} ]; then "
        f"for toolbox_node_entry in {toolbox_modules}/*; do "
        '[ -e "$toolbox_node_entry" ] || continue; '
        f'task_node_entry={node_modules}/${{toolbox_node_entry##*/}}; '
        'if [ ! -e "$task_node_entry" ] && [ ! -L "$task_node_entry" ]; then '
        'ln -s "$toolbox_node_entry" "$task_node_entry"; fi; '
        "done; fi"
    )


class FastDockerEnvironment(DockerEnvironment):
    """Cache native task images and mount one shared verifier toolbox."""

    def __init__(
        self,
        *args: Any,
        toolbox_dockerfile: str | None = "evals/pytest/Dockerfile",
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._toolbox_dockerfile = (
            Path(toolbox_dockerfile).resolve() if toolbox_dockerfile else None
        )

    @override
    async def start(self, force_build: bool) -> None:
        if self._toolbox_dockerfile is not None:
            task_dockerfile = self.environment_dir / "Dockerfile"
            if not task_dockerfile.is_file():
                raise RuntimeError(
                    "verifier toolbox caching requires the task's environment/Dockerfile"
                )

            platform = await default_docker_platform()

            async def ensure_toolbox_image() -> str:
                key = (self._toolbox_dockerfile, platform)
                async with _TOOLBOX_BUILD_LOCK:
                    if image := _TOOLBOX_IMAGES.get(key):
                        return image
                    image = await ensure_docker_image_built(
                        docker_name="nanocodex/verifier-toolbox",
                        docker_build_context=self._toolbox_dockerfile.parent,
                        dockerfile_path=self._toolbox_dockerfile,
                        build_args={},
                        platform=platform,
                        logger=self.logger,
                    )
                    _TOOLBOX_IMAGES[key] = image
                    return image

            task_image, toolbox_image = await asyncio.gather(
                _ensure_task_image(
                    environment_name=self.environment_name,
                    environment_dir=self.environment_dir,
                    dockerfile_path=task_dockerfile,
                    platform=platform,
                    logger=self.logger,
                ),
                ensure_toolbox_image(),
            )
            self._mounts = [
                mount
                for mount in self._mounts
                if mount.get("target") not in {_TOOLBOX_ROOT, _VERIFIER_ROOT}
            ]
            self._mounts.extend(
                [
                    ServiceVolumeConfig(
                        type="image",
                        source=toolbox_image,
                        target=_TOOLBOX_ROOT,
                        read_only=True,
                    )
                ]
            )
            self.task_env_config.docker_image = task_image
            self._env_vars.prebuilt_image_name = task_image
            force_build = False
        await super().start(force_build)
        if self._toolbox_dockerfile is not None:
            await self.exec(
                _toolbox_mount_setup_command(),
                user="root",
            )

    @override
    async def _run_docker_compose_command(
        self, command: list[str], *args: Any, **kwargs: Any
    ) -> Any:
        if command and command[0] in {"down", "stop"}:
            command = [*command, "--timeout", "0"]
        return await super()._run_docker_compose_command(command, *args, **kwargs)
