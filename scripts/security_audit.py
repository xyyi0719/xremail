#!/usr/bin/env python3
"""跑一遍 `cargo audit`，并按**可达性**给人话结论。

## 为什么需要包装它

`cargo audit` 直接跑有两个问题：

1. **它默认要联网克隆 RustSec 漏洞库**。这台机器上 `git` 到 GitHub 经常
   连不上（GnuTLS 中断），而 `gh api` 是通的——所以这里在失败时自动
   用 `gh api` 拉 tarball 解出漏洞库，再交给 `--db` 用。
2. **它只报清单，不报「这个漏洞在本项目里能不能被触发」**。一个只出现在
   诊断工具里的 crate 与一个在密码学路径上的 crate，处理优先级完全不同。
   这里会顺带打出每个受影响的 crate 处在依赖树的什么位置。

## 用法

```bash
python3 scripts/security_audit.py            # 概要
python3 scripts/security_audit.py --verbose  # 连未维护警告一起列
```

退出码：有**有补丁的漏洞**时返回 1（该修）；只剩无补丁的时返回 0 并说明。

## 它不做什么

* 不会自动改 `Cargo.toml` / `Cargo.lock`——升级要人看着做，
  尤其是密码学相关的 crate
* 不判断「无补丁」的那些到底能不能触发，只给出位置让人去判断
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".devcache"
ADVISORY_DIR = CACHE / "advisory-db-src"


def which(program: str) -> str | None:
    found = shutil.which(program)
    if found:
        return found
    candidate = Path.home() / ".local" / "bin" / program
    return str(candidate) if candidate.is_file() else None


def cargo_env() -> dict:
    """跑 `cargo` 的环境。

    **两个变量要分开看**：
    * `CARGO_HOME` 决定拉 crates 的缓存位置——用工作区里的那份，避免写 `$HOME`
    * `PATH` 决定去哪找 `cargo-audit` 这个**子命令**——它装在 `~/.cargo/bin`，
      所以 `PATH` 必须保留真实的那一份

    踩过的坑：只设 `CARGO_HOME` 而没管 `PATH` 时，`cargo audit` 报
    「no such command」，因为 `PATH` 里根本没有装插件的那个目录。
    """
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(CACHE / "cargo-home"))
    cargo_bin = str(Path.home() / ".cargo" / "bin")
    if cargo_bin not in env.get("PATH", ""):
        env["PATH"] = cargo_bin + os.pathsep + env.get("PATH", "")
    return env


def fetch_advisory_db() -> Path:
    """拉一份漏洞库。先试 git，失败再走 `gh api` 的 tarball。"""
    if ADVISORY_DIR.is_dir() and (ADVISORY_DIR / "crates").is_dir():
        return ADVISORY_DIR

    ADVISORY_DIR.parent.mkdir(parents=True, exist_ok=True)
    tarball = CACHE / "advisory-db.tar.gz"

    gh = which("gh")
    if not gh:
        print("找不到 gh，也没法 git clone——跳过自动下载。", file=sys.stderr)
        return ADVISORY_DIR

    print("下载 RustSec 漏洞库（走 gh api，本机 git 到 GitHub 不稳）…")
    with urllib.request.urlopen(  # noqa: S310 - 固定地址
        "https://api.github.com/repos/RustSec/advisory-db/tarball/main"
    ) as response, tarball.open("wb") as out:
        shutil.copyfileobj(response, out)

    # `cargo audit --db` 对目录有两条硬要求，踩了两遍才搞清楚：
    #   1. 目录必须**不存在或为空**——否则报「Refusing to initialize the
    #      non-empty directory」（它以为你要把 clone 落到那里）；
    #   2. 落地之后它还要一个 **`origin` remote**——没有就报
    #      「The remote named "origin" did not exist」。
    # 所以这里的做法是：解压到一个空目录 → `git init` → 加一个 origin（**不需要
    # 真的能 fetch**，只是让它认为这是 RustSec 的镜像）。
    if ADVISORY_DIR.is_dir():
        shutil.rmtree(ADVISORY_DIR)
    ADVISORY_DIR.mkdir(parents=True)
    with tarfile.open(tarball) as archive:
        # tarball 顶层是 `RustSec-advisory-db-<sha>/`，剥掉。
        for member in archive.getmembers():
            parts = Path(member.name).parts
            if len(parts) <= 1:
                continue
            member.name = str(Path(*parts[1:]))
            archive.extract(member, ADVISORY_DIR, filter="data")

    def run_git(*args: str) -> None:
        subprocess.run(
            ["git", *args], cwd=ADVISORY_DIR, check=False,
            capture_output=True, text=True,
            env={**os.environ, "GIT_AUTHOR_NAME": "xremail",
                 "GIT_AUTHOR_EMAIL": "xremail@local",
                 "GIT_COMMITTER_NAME": "xremail",
                 "GIT_COMMITTER_EMAIL": "xremail@local"},
        )

    run_git("init", "-q", ".")
    run_git("remote", "add", "origin", "https://github.com/RustSec/advisory-db.git")
    run_git("add", "-A")
    run_git("commit", "-q", "-m", "local snapshot")
    return ADVISORY_DIR

    ADVISORY_DIR.parent.mkdir(parents=True, exist_ok=True)
    tarball = CACHE / "advisory-db.tar.gz"

    gh = which("gh")
    if not gh:
        print("找不到 gh，也没法 git clone——跳过自动下载。", file=sys.stderr)
        return ADVISORY_DIR

    print("下载 RustSec 漏洞库（走 gh api，本机 git 到 GitHub 不稳）…")
    with urllib.request.urlopen(  # noqa: S310 - 固定地址
        "https://api.github.com/repos/RustSec/advisory-db/tarball/main"
    ) as response, tarball.open("wb") as out:
        shutil.copyfileobj(response, out)

    # **必须解到一个干净目录**：`cargo audit --db` 在目录非空且不是 git 仓库时
    # 会拒绝使用（它以为你要 clone 到那里），报「Refusing to initialize the
    # non-empty directory」。先删再建，不要复用。
    if ADVISORY_DIR.is_dir():
        shutil.rmtree(ADVISORY_DIR)
    ADVISORY_DIR.mkdir(parents=True)
    with tarfile.open(tarball) as archive:
        # tarball 顶层是 `RustSec-advisory-db-<sha>/`，剥掉。
        for member in archive.getmembers():
            parts = Path(member.name).parts
            if len(parts) <= 1:
                continue
            member.name = str(Path(*parts[1:]))
            archive.extract(member, ADVISORY_DIR, filter="data")
    return ADVISORY_DIR


def audit(db: Path) -> dict:
    cargo = which("cargo")
    if not cargo:
        print("找不到 cargo。", file=sys.stderr)
        sys.exit(1)
    result = subprocess.run(
        [cargo, "audit", "--json", "--db", str(db)],
        capture_output=True, text=True, env=cargo_env(), check=False,
    )
    # `cargo audit` 在有漏洞时退出码非零，但 JSON 仍然是完整的。
    text = result.stdout.strip()
    if not text.startswith("{"):
        print("cargo audit 没有输出 JSON：\n" + result.stderr[-2000:], file=sys.stderr)
        sys.exit(1)
    return json.loads(text)


def where(crate: str) -> str:
    """这个 crate 处在依赖树的什么位置（只取前几层，够看就行）。"""
    cargo = which("cargo")
    result = subprocess.run(
        [cargo, "tree", "-i", crate, "--depth", "2"],
        capture_output=True, text=True, env=cargo_env(), check=False,
    )
    lines = [
        line for line in result.stdout.splitlines()
        if line.strip() and not line.startswith("warning")
    ]
    return "\n".join(lines[:6])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verbose", action="store_true", help="连未维护警告一起列")
    args = parser.parse_args()

    db = fetch_advisory_db()
    data = audit(db)

    vulns = data.get("vulnerabilities", {}).get("list", [])
    warnings = data.get("warnings", {})
    # `warnings.unmaintained` / `warnings.unsound` 都是**列表**（不是字典），
    # 每项形如 `{"kind": …, "package": "…", "advisory": "RUSTSEC-…"}`。
    unmaintained = list(warnings.get("unmaintained", [])) + list(warnings.get("unsound", []))

    fixable = 0
    unfixable = 0

    if not vulns:
        print("✓ 没有已知漏洞。")
    else:
        print(f"发现 {len(vulns)} 个已知漏洞：\n")
        for item in vulns:
            advisory = item["advisory"]
            package = item["package"]
            versions = advisory.get("versions", {})
            patched = versions.get("patched") or []
            fixable_here = bool(patched)
            fixable += fixable_here
            unfixable += not fixable_here

            print(f"  {'✗ 有补丁' if fixable_here else '⚠ 无补丁'}  "
                  f"{package['name']} {package['version']}  {advisory['id']}")
            print(f"       {advisory.get('title', '')}")
            if fixable_here:
                print(f"       修法：升级到 {' / '.join(patched)}")
            else:
                print("       没有可用的修复版本——要么等上游，要么接受并记录")
            tree = where(package["name"])
            if tree:
                print("       依赖位置：")
                for line in tree.splitlines():
                    print(f"         {line}")
            print()

    if args.verbose and unmaintained:
        print(f"另有 {len(unmaintained)} 个未维护/不健全的 crate：")
        for info in unmaintained:
            print(f"  · {info.get('package', '?')}  {info.get('advisory', '')}".rstrip())

    print()
    if fixable:
        print(f"有 {fixable} 个**有补丁**的漏洞该修；{unfixable} 个无补丁需要判断可达性。")
        return 1
    if unfixable:
        print(f"{unfixable} 个漏洞目前没有可用补丁——"
              "先看上面的「依赖位置」判断它在你们这里能不能被触发，再决定要不要接受。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
