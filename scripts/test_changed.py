#!/usr/bin/env python3
"""只跑「这次改了的东西」相关的测试。

## 为什么需要它

`cargo test --workspace` 现在是 **1147 个用例**（`--list` 口径，2026-11-10 实测）。
纯执行约 **18 秒**；但**改一行源码**会触发重新编译，全量变成 **约 29 秒**
（`touch crates/xremail-core/src/lib.rs` 实测）。改一行注释也跑一遍全量，
等于把最贵的一步做成了最高频的一步。

（这段以前写的是「790 个用例、几分钟一次」——那是 2026-10 的数字，早就过时了。
用例数一路涨到 1147，但**耗时反而降下来了**：早期 `xremail-testkit` 有一个
`1:*` 被展开成 1.79 亿个元素的坑，把一次全量拖到几分钟，后来修掉了。）

这个脚本做的事：**从 git 的改动文件反推「哪些包被影响」，只测那些包。**

判据是**反向依赖**：改了 `xremail-store`，那所有 `use xremail_store` 的包
都必须重测——它们吃的是它的 API，行为变了它们才可能坏。
反过来，只改了 `xremail-diagnostics` 这一个叶子包，就没有别的包要陪跑。

## 用法

    python scripts/test_changed.py               # 与 HEAD 比（含工作区/暂存）
    python scripts/test_changed.py --base main   # 与某个分支比
    python scripts/test_changed.py --staged      # 只比暂存区
    python scripts/test_changed.py --all          # 无视改动，跑全量（发版前用）
    python scripts/test_changed.py --dry-run      # 只打印计划，不执行

## 它不替代什么

* **发版 / 推 PR 前仍然要跑 `--all`**。增量判断只看「改动文件」，
  不看「本次改动是否悄悄破坏了别的 crate 的语义假设」。
* **改动落在根 `Cargo.toml` / `Cargo.lock` 时自动退化成全量**——
  依赖版本一换，影响面就是整个 workspace，这时候省不得。

## 前端

`apps/shell/src/**` 或 Tauri 侧有改动 → 跑 `tsc` + `vitest` + `vite build`。
Tauri 侧动了一定连前端一起跑：IPC 的线格式是两边共享的，Rust 那边改了
字段名，TS 这边只会静默拿到 `undefined`，只有类型检查能抓住。

只改了 `docs/**` 或 `*.md` → 只跑文档链接检查，Rust 与前端一行不碰。
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# 改这些文件等于「影响整个 workspace」，不用再算依赖图了。
WHOLE_WORKSPACE = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"}

# 前端工具链的输入。改任何一个都要重跑前端三件套。
FRONTEND_ROOTS = ("apps/shell/src/",)
FRONTEND_FILES = {
    "apps/shell/package.json",
    "apps/shell/package-lock.json",
    "apps/shell/tsconfig.json",
    "apps/shell/vite.config.ts",
    "apps/shell/index.html",
}

# 桌面外壳的 Rust 侧。改它要连前端一起跑（见模块文档）。
TAURI_ROOT = "apps/shell/src-tauri/"


def run(cmd: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd, cwd=REPO, text=True, capture_output=True, check=check
    )


def changed_files(args: argparse.Namespace) -> list[str]:
    """改动过的文件（仓库相对路径，正斜杠）。"""
    if args.base:
        cmd = ["git", "diff", "--name-only", f"{args.base}...HEAD"]
        out = run(cmd).stdout
    elif args.staged:
        out = run(["git", "diff", "--name-only", "--cached"]).stdout
    else:
        # 工作区（含暂存）的改动；如果工作区干净，退回「最后一个提交改了什么」。
        out = run(["git", "diff", "--name-only", "HEAD"]).stdout
        if not out.strip():
            out = run(["git", "diff", "--name-only", "HEAD~1", "HEAD"], check=False).stdout

    files = [line.strip() for line in out.splitlines() if line.strip()]
    # 未跟踪的新文件也算法改动，否则新加的用例文件会被漏掉。
    untracked = run(["git", "ls-files", "--others", "--exclude-standard"], check=False).stdout
    files.extend(line.strip() for line in untracked.splitlines() if line.strip())
    return sorted(set(files))


def workspace_map() -> tuple[dict[str, Path], dict[str, set[str]], dict[str, set[str]]]:
    """返回 (包名→目录, 包名→本地依赖, 包名→反向依赖)。"""
    meta = json.loads(
        run(["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"]).stdout
    )
    dirs: dict[str, Path] = {}
    deps: dict[str, set[str]] = {}
    local = {p["name"] for p in meta["packages"]}

    for package in meta["packages"]:
        name = package["name"]
        dirs[name] = Path(package["manifest_path"]).parent
        deps[name] = {
            d["name"] for d in package["dependencies"] if d.get("path") and d["name"] in local
        }

    reverse: dict[str, set[str]] = {name: set() for name in dirs}
    for name, direct in deps.items():
        for dep in direct:
            reverse[dep].add(name)
    return dirs, deps, reverse


def owning_package(path: str, dirs: dict[str, Path]) -> str | None:
    """文件属于哪个包（取最长前缀，避免 `crates/xremail-sync` 抢走 `-sync-smoke`）。"""
    target = (REPO / path).resolve()
    best: tuple[int, str] | None = None
    for name, directory in dirs.items():
        try:
            target.relative_to(directory.resolve())
        except ValueError:
            continue
        depth = len(directory.parts)
        if best is None or depth > best[0]:
            best = (depth, name)
    return best[1] if best else None


def affected(
    files: list[str], dirs: dict[str, Path], reverse: dict[str, set[str]]
) -> tuple[set[str], bool, bool]:
    """返回 (要测的包, 要不要跑前端, 要不要跑文档检查)。"""
    packages: set[str] = set()
    frontend = False
    docs = False
    everything = False

    for path in files:
        if path in WHOLE_WORKSPACE:
            everything = True
            continue
        if path in FRONTEND_FILES or path.startswith(FRONTEND_ROOTS):
            frontend = True
            continue
        if path.startswith(TAURI_ROOT):
            frontend = True  # 线格式两边共享
        if path.endswith(".md") and path.startswith("docs/"):
            docs = True
            continue
        name = owning_package(path, dirs)
        if name:
            packages.add(name)

    if everything:
        return set(dirs), True, True

    # 加上所有（传递地）依赖它们的包。
    closure = set(packages)
    queue = list(packages)
    while queue:
        current = queue.pop()
        for dependent in reverse.get(current, ()):
            if dependent not in closure:
                closure.add(dependent)
                queue.append(dependent)

    return closure, frontend or everything, docs


def main() -> int:
    # Windows 控制台默认是 GBK/CP936，中文会在管道里变成问号。
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")

    parser = argparse.ArgumentParser(description="只跑受本次改动影响的测试")
    parser.add_argument("--base", help="与这个 git 引用比较（例如 main）")
    parser.add_argument("--staged", action="store_true", help="只比较暂存区")
    parser.add_argument("--all", action="store_true", help="无视改动，跑全量")
    parser.add_argument("--dry-run", action="store_true", help="只打印计划")
    parser.add_argument(
        "--jobs", default="1", help="cargo test 的 -j（默认 1，与文档记录的用例数一致）"
    )
    args = parser.parse_args()

    if args.all:
        packages, frontend, docs = None, True, True
        files = ["<--all>"]
    else:
        files = changed_files(args)
        if not files:
            print("没有改动 → 什么也不用跑。")
            return 0
        dirs, _deps, reverse = workspace_map()
        packages, frontend, docs = affected(files, dirs, reverse)

    print(f"改动文件 {len(files)} 个：")
    for path in files[:20]:
        print(f"  - {path}")
    if len(files) > 20:
        print(f"  …… 另有 {len(files) - 20} 个")

    steps: list[tuple[str, list[str]]] = []

    if packages:
        names = sorted(packages) if packages is not None else []
        if names:
            print(f"\n受影响的 Rust 包 {len(names)} 个：{', '.join(names)}")
            cmd = ["cargo", "test", "-j", str(args.jobs)]
            for name in names:
                cmd += ["-p", name]
            steps.append(("Rust 测试", cmd))
        else:
            print("\n没有 Rust 包受影响 → 跳过 cargo test。")

    if frontend:
        steps.append(("前端类型检查", ["npx", "tsc", "--noEmit"]))
        steps.append(("前端用例", ["npx", "vitest", "run"]))
        steps.append(("前端构建", ["npx", "vite", "build"]))
    else:
        print("前端没有改动 → 跳过 tsc / vitest / build。")

    if docs:
        steps.append(("文档链接", [sys.executable, "scripts/check_doc_links.py"]))
    else:
        print("文档没有改动 → 跳过链接检查。")

    if not steps:
        print("\n没有要跑的步骤。")
        return 0

    print("\n计划：")
    for label, cmd in steps:
        print(f"  [ ] {label}: {' '.join(cmd)}")

    if args.dry_run:
        print("\n--dry-run：没有执行。")
        return 0

    for label, cmd in steps:
        print(f"\n=== {label} ===")
        # 前端命令要在 apps/shell 下跑。
        cwd = REPO / "apps/shell" if cmd[0] in {"npx"} else REPO
        result = subprocess.run(cmd, cwd=cwd)
        if result.returncode != 0:
            print(f"\n✗ {label} 失败（退出码 {result.returncode}）")
            return result.returncode
        print(f"✓ {label} 通过")

    print("\n全部通过。")
    print("提示：发版/推 PR 前跑一次 `python scripts/test_changed.py --all`。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
