#!/usr/bin/env python3
"""生成 rpi 预编译扩展分发清单（protocol v1）。

两段式，配合 CI 的并行矩阵：

1. 每个平台 job 跑 `fragment`，产出只含自己平台条目的片段 JSON。
2. 最后的合并 job 跑 `merge`，把所有片段合成一个 `rpi-extension.json`。

清单字段契约见 https://github.com/pi-rust/rpi-extension/blob/main/docs/rpi-extension.schema.json
（`schema_version` / `repository` / `tag` / `source_commit` / `sdk` / `targets`）。

注意：本文件生成的清单放在 **Release 资产**里，供 `rpi install` 下载校验；
仓库根目录那份给人读的收录清单（`schemaVersion` / `type` / `install`）是另一份，
不要混用。
"""

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys

SCHEMA_VERSION = 1


def fail(message):
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def digest(path):
    return hashlib.file_digest(path.open("rb"), "sha256").hexdigest()


def cargo_metadata(manifest_path):
    """读取包元数据（需要 resolve 才能定位 sdk 依赖）。"""
    args = ["cargo", "metadata", "--format-version", "1", "--locked"]
    if manifest_path:
        args.extend(["--manifest-path", str(manifest_path)])
    return json.loads(subprocess.check_output(args, text=True, encoding="utf-8"))


def package_of(metadata, name):
    for package in metadata.get("packages", []):
        if package.get("name") == name:
            return package
    fail(f"package {name} not found in cargo metadata")


def sdk_identity(metadata, package):
    """返回 `{"version": ..., "plugin_abi": ...}`。

    plugin_abi 从 SDK 源码里的常量读，保证与真正编译进动态库的 ABI 一致。
    """
    by_id = {item["id"]: item for item in metadata["packages"]}
    node = next(item for item in metadata["resolve"]["nodes"] if item["id"] == package["id"])
    sdk_id = next(dep["pkg"] for dep in node["deps"] if dep["name"] == "rpi_plugin_sdk")
    sdk = by_id[sdk_id]
    source = pathlib.Path(sdk["manifest_path"]).parent / "src/lib.rs"
    match = re.search(
        r"pub const RPI_PLUGIN_ABI_VERSION_UNIFIED:\s*u32\s*=\s*(\d+)",
        source.read_text(encoding="utf-8"),
    )
    if not match:
        fail(f"cannot determine the plugin ABI of SDK {sdk['version']}")
    return {"version": sdk["version"], "plugin_abi": int(match.group(1))}


def write_json(path, value):
    path = pathlib.Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def normalize_repository(value):
    r"""把 `owner/repo` 或各种 GitHub URL 统一成清单要求的 `https://github.com/owner/repo`。

    清单 schema 的 `repository` 只接受完整 HTTPS URL（`^https://github\.com/[^/]+/[^/]+$`），
    不能写 `owner/repo`。
    """
    repo = value.strip()
    for prefix in ("git+",):
        repo = repo.removeprefix(prefix)
    repo = repo.removesuffix(".git").rstrip("/")
    if "://" in repo:
        marker = "github.com/"
        index = repo.find(marker)
        if index == -1:
            fail(f"repository is not on GitHub: {value}")
        repo = repo[index + len(marker) :]
    parts = [part for part in repo.split("/") if part]
    if len(parts) != 2:
        fail(f"repository must be owner/repo or a GitHub URL: {value}")
    return f"https://github.com/{parts[0]}/{parts[1]}"


def command_fragment(args):
    metadata = cargo_metadata(args.manifest_path)
    package = package_of(metadata, args.package)
    repository = normalize_repository(args.repository)
    archive = pathlib.Path(args.archive).resolve()
    if not archive.is_file():
        fail(f"archive is missing: {archive}")

    # 库文件名必须与压缩包内的条目一致，且是扁平文件名（安装器要求）。
    # 按 `--target` 推导，不能用宿主三元组：交叉构建时两者不同。
    library_name = args.library or package_library_name(metadata, package, args.target)
    library_sha = args.library_sha256 or library_sha_from_archive(archive, library_name)

    tag = f"{package['name']}-v{package['version']}"
    write_json(
        args.output,
        {
            "schema_version": SCHEMA_VERSION,
            "name": package["name"],
            "version": package["version"],
            "repository": repository,
            "tag": tag,
            "source_commit": args.source_commit,
            "sdk": sdk_identity(metadata, package),
            "target": args.target,
            "distribution": {
                "url": f"{repository}/releases/download/{tag}/{archive.name}",
                "format": "zip" if archive.suffix == ".zip" else "tar.gz",
                "size": archive.stat().st_size,
                "sha256": digest(archive),
                "library": {"path": library_name, "sha256": library_sha},
            },
        },
    )


def package_library_name(metadata, package, target):
    """按目标三元组推导库文件名（Windows 无 `lib` 前缀，其余平台有）。"""
    library_target = next(
        (t for t in package["targets"] if "cdylib" in t["crate_types"]), None
    )
    if library_target is None:
        fail(f"{package['name']} has no cdylib target")
    stem = library_target["name"]
    if "windows" in target:
        return f"{stem}.dll"
    if "apple" in target:
        return f"lib{stem}.dylib"
    return f"lib{stem}.so"


def library_sha_from_archive(archive, library_name):
    """从归档里取出库条目并算 SHA-256（不信任工作区里的 target 目录）。"""
    import tarfile
    import zipfile

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as bundle:
            try:
                data = bundle.read(library_name)
            except KeyError:
                fail(f"{archive.name} does not contain {library_name}")
    else:
        with tarfile.open(archive) as bundle:
            member = next((m for m in bundle.getmembers() if m.name == library_name), None)
            if member is None:
                fail(f"{archive.name} does not contain {library_name}")
            data = bundle.extractfile(member).read()
    return hashlib.sha256(data).hexdigest()


def command_merge(args):
    fragments = sorted(pathlib.Path(args.fragments).glob("*.json"))
    if not fragments:
        fail(f"no fragments found in {args.fragments}")

    loaded = [json.loads(path.read_text(encoding="utf-8")) for path in fragments]
    identity = ("schema_version", "name", "version", "repository", "tag", "source_commit", "sdk")
    first = loaded[0]
    for fragment in loaded[1:]:
        if any(fragment[key] != first[key] for key in identity):
            fail(f"platform fragment identity differs: {fragment['target']}")

    manifest = {key: first[key] for key in identity}
    manifest["targets"] = {fragment["target"]: fragment["distribution"] for fragment in loaded}
    write_json(args.output, manifest)
    print(
        f"wrote {args.output} for {first['name']} {first['version']} "
        f"({len(manifest['targets'])} target(s): {', '.join(manifest['targets'])})"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    fragment = sub.add_parser("fragment", help="record one platform archive")
    fragment.add_argument("--target", required=True, help="exact Rust target triple")
    fragment.add_argument("--archive", required=True)
    fragment.add_argument("--repository", required=True, help="owner/repo")
    fragment.add_argument("--source-commit", required=True)
    fragment.add_argument("--package", default="rpi-opencode-extras")
    fragment.add_argument("--manifest-path")
    fragment.add_argument("--library", help="override the library filename inside the archive")
    fragment.add_argument(
        "--library-sha256", help="override the library digest (defaults to reading the archive)"
    )
    fragment.add_argument("--output", required=True)
    fragment.set_defaults(func=command_fragment)

    merge = sub.add_parser("merge", help="merge platform fragments into rpi-extension.json")
    merge.add_argument("--fragments", required=True)
    merge.add_argument("--output", required=True)
    merge.set_defaults(func=command_merge)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
