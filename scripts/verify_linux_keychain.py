#!/usr/bin/env python3
"""在一个临时 D-Bus 会话里跑 Linux 钥匙串的探针用例。

## 解决什么问题

`apps/shell/src-tauri/tests/linux_secret_service.rs` 里那两条用例默认**空跑**：
不设 `XREMAIL_SECRET_SERVICE_PROBE=1` 就直接 `return`——这是有意的，
免得在日常 `cargo test` 里往用户的真实钥匙串里写条目。

麻烦在于**怎么把它们真跑起来**：

* 光设环境变量不够——还得有一个 **D-Bus 会话**（`DBUS_SESSION_BUS_ADDRESS`），
  否则 `keyring` 连不上 `org.freedesktop.secrets`；
* `dbus-run-session` 能起会话，但它要跟一条**具体命令**；
* 测试二进制的路径由 cargo 决定，不能写死。

`tests/linux_secret_service.rs` 的模块文档写着「由 `scripts/` 里的包装脚本设」——
**那个脚本此前并不存在**（2026-11-10 补上），所以只能靠人照着手写那串命令。

## 用法

```bash
python3 scripts/verify_linux_keychain.py
```

它会：

1. 查这台机器有没有 **Secret Service 守护进程**（`gnome-keyring-daemon` /
   `kwalletd` 之类）。没有就**明确告诉你「验不了」**，而不是假装跑过；
2. 有的话，在一个临时的 `dbus-run-session` 里设好环境变量跑那两条用例；
3. 把结果原样报出来——包括「两种结局都合法」这件事：
   有守护进程就该写→读→删走通，没有就该明确报 `Unavailable`。

## 它**不**做什么

* 不启动守护进程。起 `gnome-keyring` 需要图形会话与解锁口令，
  不是脚本能替人做的决定。
* 不碰你真实的账户条目——用例用的是 `xremail:probe*` 这种独立名字，
  且写完立刻删。
"""

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".devcache"


def cargo_env() -> dict:
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(CACHE / "cargo-home"))
    env.setdefault("CARGO_TARGET_DIR", str(CACHE / "cargo-target"))
    env.setdefault("SQLX_OFFLINE", "true")
    cargo_bin = str(Path.home() / ".cargo" / "bin")
    if cargo_bin not in env.get("PATH", ""):
        env["PATH"] = cargo_bin + os.pathsep + env.get("PATH", "")
    return env


def has_secret_service() -> tuple[bool, str]:
    """这台机器上有没有能响应 `org.freedesktop.secrets` 的东西。

    判据是**守护进程在不在**，而不是「D-Bus 通不通」：
    后者在容器/CI 里通常是通的，但没人提供服务。
    """
    for name in ("gnome-keyring-daemon", "kwalletd5", "kwalletd6", "keepassxc"):
        if shutil.which(name):
            return True, name
    return False, ""


def main() -> int:
    env = cargo_env()

    print("1. 查 Secret Service 守护进程")
    present, daemon = has_secret_service()
    if present:
        print(f"  ✓ 找到 {daemon}")
    else:
        print("  ✗ 没找到（gnome-keyring-daemon / kwalletd / keepassxc 都不在 PATH 里）")
        print()
        print("  这意味着**这一项在本机验不了**：没有守护进程就只可能得到")
        print("  「如实报 Unavailable」那一种结局，写→读→删的往返走不通。")
        print()
        print("  要验它，先装一个：")
        print("    sudo apt install gnome-keyring        # Debian / Ubuntu")
        print("    sudo dnf install gnome-keyring        # Fedora")
        print("  然后在**图形会话**里跑（或先 `dbus-run-session -- gnome-keyring-daemon --start`）。")
        print()
        print("  —— 仍然可以跑一遍，看「没有守护进程时会不会如实报错」这一半：")
        print("     （下面就会跑；若你想跳过，Ctrl-C 即可）")
        print()
        # 没有守护进程也照跑：那一半本身就是有价值的断言。

    print("2. 在临时 D-Bus 会话里跑探针用例")
    if not shutil.which("dbus-run-session"):
        print("  ✗ 找不到 dbus-run-session（`apt install dbus-x11` / `dnf install dbus-tools`）")
        return 1

    probe_env = {**env, "XREMAIL_SECRET_SERVICE_PROBE": "1"}
    result = subprocess.run(
        [
            "dbus-run-session", "--",
            "cargo", "test", "-p", "xremail-shell",
            "--test", "linux_secret_service",
            "--", "--test-threads=1",
        ],
        env=probe_env, cwd=ROOT, check=False,
        capture_output=True, text=True,
    )
    output = result.stdout + result.stderr

    # 只把有用的那几行打出来，不要把整份 cargo 输出倒给用户。
    for line in output.splitlines():
        if any(marker in line for marker in ("running ", "test ", "test result:", "panicked")):
            print("  " + line.strip())

    if result.returncode == 0:
        print()
        print("通过。")
        if not present:
            print("注意：这次验的是「没有守护进程时会如实报 Unavailable」那一半——")
            print("      写→读→删的往返**仍然没验过**，需要先装守护进程。")
        return 0

    print()
    print("失败。把完整输出贴出来再判断——上面只筛了几行。")
    return 1


if __name__ == "__main__":
    sys.exit(main())
