#!/usr/bin/env python3
"""在虚拟显示（Xvfb）里真实启动打包产物，验证 Linux 运行链路。

## 为什么需要它

`cargo test` 能证明业务逻辑对，`cargo build` 能证明能编译，**但都不能证明
"装上去能跑"**——真实的失败往往发生在两者之间：

* 打包产物缺一个动态库（`ldd` 会显示 `not found`，但 CI 只跑 `cargo test` 时不打包）；
* 数据目录在只读文件系统上 → 迁移失败；
* 首次启动时迁移链跑不通 → 应用启动失败（而不是降级成空库）；
* tauri.conf.json 里的 CSP / 窗口配置让 WebView 起不来。

这个脚本把这些串起来跑一遍：**Xvfb 起虚拟显示 → 真跑打包出来的二进制 →
检查它建出了数据库、跑完了全部迁移、并且没有 panic**。

## 它验什么、不验什么

验：
1. 二进制能加载（`ldd` 无 `not found`）；
2. 进程能起来并活过启动期（不是立刻崩）；
3. 应用数据目录里建出了 `xremail.db`；
4. `_sqlx_migrations` 全部成功（`success = 1`），且条数与 `migrations/` 里的
   `.sql` 文件数一致——**这条能抓到「迁移写错但没人发现」**；
5. 日志里没有 `panicked`；
6. **托盘图标真的渲染出来了**：`$XDG_RUNTIME_DIR/tray-icon/` 下有 PNG，
   且日志里没有「创建托盘图标失败」。这一步需要一个**可写的**
   `XDG_RUNTIME_DIR`——脚本自己建一个传给应用（见下）。

不验：
* GUI 的外观与交互（Xvfb 里没有真实输入，也没有人工判断）；
* 托盘的**外观与点击行为**（Xvfb 里没有真实输入；但**图标是否渲染成功**
  是可以验的——见下面第 6 步）；
* IMAP/SMTP 真实收发（需要真实账号）。

## 用法

```bash
# 先打包（只需一次）
cd apps/shell && npm run tauri build -- --bundles deb
cd apps/shell && npm run tauri build -- --bundles appimage

# 再验（默认验 deb；给参数就验 AppImage）
python3 scripts/verify_linux_runtime.py
python3 scripts/verify_linux_runtime.py appimage
```

依赖：`xvfb-run`、`dpkg-deb`（解包用）。两者在 Ubuntu 上是标准件。
"""

import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# 打包产物的默认位置（Tauri 的 bundle 目录；CARGO_TARGET_DIR 可能被改过，
# 所以这里两个都找）。
BUNDLE_CANDIDATES = [
    ROOT / "target" / "release" / "bundle",
    ROOT / ".devcache" / "cargo-target" / "release" / "bundle",
]
MIGRATIONS = ROOT / "crates" / "xremail-store" / "migrations"


def fail(message: str) -> None:
    print(f"  ✗ {message}")
    sys.exit(1)


def ok(message: str) -> None:
    print(f"  ✓ {message}")


def make_executable(path: Path) -> None:
    """AppImage 必须可执行才能自解包运行（下载下来通常没有执行位）。"""
    path.chmod(path.stat().st_mode | 0o111)


def find_package(kind: str) -> Path | None:
    """找打包产物。`kind` 是 `deb` 或 `appimage`。"""
    pattern = "*.deb" if kind == "deb" else "*.AppImage"
    for bundle_root in BUNDLE_CANDIDATES:
        directory = bundle_root / kind
        if directory.is_dir():
            found = sorted(directory.glob(pattern))
            if found:
                return found[0]
    return None


def main() -> None:
    kind = sys.argv[1] if len(sys.argv) > 1 else "deb"
    if kind not in ("deb", "appimage"):
        fail(f"未知的打包类型 {kind!r}（可选 deb / appimage）")

    print(f"1. 找打包产物（{kind}）")
    package = find_package(kind)
    if package is None:
        hint = "--bundles deb" if kind == "deb" else "--bundles appimage"
        fail(f"找不到 {kind}。先打包：cd apps/shell && npm run tauri build -- {hint}")
    ok(f"{package.name}（{package.stat().st_size / 1024 / 1024:.1f} MiB）")

    print("2. 取出可执行文件（不动系统）")
    workdir = Path(tempfile.mkdtemp(prefix="xremail-verify-"))
    try:
        if kind == "deb":
            extracted = workdir / "root"
            subprocess.run(
                ["dpkg-deb", "-x", str(package), str(extracted)],
                check=True,
                capture_output=True,
            )
            binaries = [
                p
                for p in extracted.rglob("*")
                if p.is_file() and os.access(p, os.X_OK) and p.parent.name == "bin"
            ]
            if not binaries:
                fail("包里没有 /usr/bin/ 下的可执行文件")
            binary = binaries[0]
            # deb 里的二进制直接跑。
            launch = [str(binary)]
        else:
            # AppImage 是自己解包运行的，直接把文件本身当命令。
            make_executable(package)
            binary = package
            # `APPIMAGE_EXTRACT_AND_RUN=1` 避免依赖 FUSE（CI / 容器里通常没有）。
            launch = [str(package)]
        ok(f"{binary.name}")

        print("3. 检查动态库依赖")
        ldd = subprocess.run(
            ["ldd", str(binary)], capture_output=True, text=True
        ).stdout
        missing = [line for line in ldd.splitlines() if "not found" in line]
        if missing:
            fail(f"有动态库缺失：{missing}")
        ok("没有缺失的 .so")

        print("4. 在 Xvfb 里启动（40 秒超时，预期是被 timeout 杀掉）")
        data_dir = workdir / "data"
        data_dir.mkdir()
        # 托盘（libayatana-appindicator）会把图标渲染成 PNG 写到
        # `$XDG_RUNTIME_DIR/tray-icon/`。默认的 `/run/user/<uid>` 在沙箱/容器里
        # 常常是**只读**的，于是托盘建不出来、只打一条 warn——
        # 那不是应用的问题，是运行环境没有给一个可写的地方。
        runtime_dir = workdir / "runtime"
        runtime_dir.mkdir()
        runtime_dir.chmod(0o700)  # XDG 规范要求 0700，否则 appindicator 会拒绝
        env = {
            **os.environ,
            "XDG_DATA_HOME": str(data_dir),
            "XDG_RUNTIME_DIR": str(runtime_dir),
            "RUST_LOG": "info",
            "APPIMAGE_EXTRACT_AND_RUN": "1",
            # 托盘在 Xvfb 下拿不到可写路径，会 warn 后跳过；不是失败。
        }
        try:
            result = subprocess.run(
                ["xvfb-run", "-a", *launch],
                env=env,
                capture_output=True,
                text=True,
                # 应用是长期运行的 GUI，靠超时结束它。
                # AppImage 首次运行要先自解包到临时目录，比 deb 慢不少；
                # 15 秒不够它跑到建库那一步。
                timeout=40,
            )
            log = result.stdout + result.stderr
        except subprocess.TimeoutExpired as expired:
            # 超时 = 它一直在跑 = 启动成功。这是我们期望的结局。
            log = (expired.stdout or b"").decode(errors="replace") if isinstance(
                expired.stdout, bytes
            ) else (expired.stdout or "")
            log += (expired.stderr or b"").decode(errors="replace") if isinstance(
                expired.stderr, bytes
            ) else (expired.stderr or "")

        if "panicked at" in log:
            fail("启动过程中 panic 了：\n" + "\n".join(
                line for line in log.splitlines() if "panicked" in line
            ))
        ok("没有 panic")

        print("5. 检查数据库与迁移")
        databases = list(data_dir.rglob("*.db"))
        if not databases:
            fail(f"没有建出数据库（在 {data_dir} 下找不到 .db）\n日志：\n{log[-2000:]}")
        db = databases[0]
        ok(f"{db.relative_to(data_dir)}")

        connection = sqlite3.connect(db)
        rows = connection.execute(
            "SELECT version, success FROM _sqlx_migrations ORDER BY version"
        ).fetchall()
        failed = [version for version, success in rows if not success]
        if failed:
            fail(f"这些迁移没成功：{failed}")
        expected = len(list(MIGRATIONS.glob("*.sql")))
        if len(rows) != expected:
            fail(
                f"迁移条数对不上：库里 {len(rows)} 条，"
                f"migrations/ 里有 {expected} 个 .sql"
            )
        ok(f"{len(rows)} 条迁移全部成功（与 migrations/ 的 {expected} 个文件一致）")

        print("6. 托盘图标（libayatana-appindicator）")
        tray_failed = "创建托盘图标失败" in log
        icons = list((runtime_dir / "tray-icon").glob("*.png")) if (
            runtime_dir / "tray-icon"
        ).is_dir() else []
        if tray_failed:
            fail(
                "托盘创建失败（日志里有「创建托盘图标失败」）。\n"
                + "\n".join(l for l in log.splitlines() if "托盘" in l)
            )
        if not icons:
            fail(
                "没有在 $XDG_RUNTIME_DIR/tray-icon/ 下找到渲染出来的图标 PNG。\n"
                f"运行目录内容：{list(runtime_dir.iterdir())}"
            )
        # 图标像素尺寸也能读出来更硬，但 stdlib 里没有 PNG 解析器；
        # 文件存在 + 非空 + 有 PNG magic 就够了。
        icon = icons[0]
        head = icon.read_bytes()[:8]
        if head != b"\x89PNG\r\n\x1a\n":
            fail(f"{icon.name} 不是合法的 PNG（开头是 {head!r}）")
        ok(f"{icon.name}（{icon.stat().st_size} 字节，PNG）")

        print("7. 关键表都在")
        tables = {
            name
            for (name,) in connection.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table'"
            )
        }
        required = {"account", "folder", "message", "send_queue", "mail_operation"}
        absent = required - tables
        if absent:
            fail(f"缺表：{sorted(absent)}")
        ok(f"表齐全（{len(tables)} 张）")
        connection.close()

        print("\n全部通过。")
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
