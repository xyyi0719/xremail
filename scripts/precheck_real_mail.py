#!/usr/bin/env python3
"""真实邮箱验收的**预检**：不需要真凭据也能跑，只检查环境是否就绪。

## 为什么需要它

`apps/shell/src-tauri/tests/real_mail_roundtrip.rs` 会**真的发一封邮件**出去，
所以它有两道开关（`#[ignore]` + `XREMAIL_REAL_ACCOUNTS=1`），
在普通 CI 里永远不跑。

代价是：**没有任何东西能在"真的跑之前"发现事情已经坏了**——
凭据文件路径变了、段名写错了、两个账户配成了同一家……
这些都要等到真正发信那一步、甚至要等到对端没收到，才知道。

这个脚本把那些**可以离线检查**的部分单独拿出来：

1. 凭据文件在不在、**权限是不是过松**（口令明文落盘的地方不该 0644）
2. 里面有没有 `163` 与 `qq` 两段（**用例目前写死要这两段**，见下）
3. 每段的 `user` / `password` 是否非空、`user` 像不像一个地址
4. QQ 的数字用户名有没有被正确补成 `@qq.com`
5. 有没有把两个账户配成同一家（那样"互发"就变成给自己发，验不出东西）
6. **不会**打印口令，也不会打印完整地址（只留首字母 + 域名）

## 它不验什么

* **不发信**、不连服务器——那是真验收的事
* 不验口令是否还有效（那要真的登录一次）

## 用法

```bash
python3 scripts/precheck_real_mail.py
```

退出码：0 = 就绪，1 = 有问题（每条都会说清是什么、怎么修）。

## 一个已知限制

`real_mail_roundtrip.rs` 目前**写死**从 `163` 与 `qq` 两段取账户
（`require_mailbox(&accounts, "163")` / `"qq"`）。想用别的组合要改代码。
这个脚本会把这件事**明确报出来**，而不是让人在跑起来之后才发现。
"""

import os
import stat
import sys
from pathlib import Path

# 用例里写死要这两段（`real_mail_roundtrip.rs` 的 `require_mailbox` 调用）。
REQUIRED_SECTIONS = ["163", "qq"]

# 各服务商的默认域名（用户名字段常常只写号码）。
PROVIDER_DOMAINS = {
    "163": "163.com",
    "qq": "qq.com",
    "wo": "wo.cn",
}


def problems(message: str) -> None:
    print(f"  ✗ {message}")


def ok(message: str) -> None:
    print(f"  ✓ {message}")


def accounts_path() -> Path:
    """与 `real_mail_roundtrip.rs` 的 `accounts_file_path()` 保持一致。"""
    home = os.environ.get("USERPROFILE") or os.environ.get("HOME") or "."
    return Path(home) / ".xremail" / "accounts.txt"


def mask(text: str) -> str:
    """把地址打成 `a***@163.com` 的形状——够核对，不够拿来登录。"""
    if "@" not in text:
        # 没有 `@` 的是「只有号码」的写法，只露最后两位。
        return text[:1] + "***" + (text[-2:] if len(text) > 3 else "")
    local, _, domain = text.partition("@")
    return f"{local[:1]}***@{domain}"


def load(text: str) -> dict:
    """照 `load_accounts()` 的规则解析：INI 风格，`#` 注释，`key = value`。"""
    out: dict = {}
    section = ""
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1].strip()
            out.setdefault(section, {})
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        out.setdefault(section, {})[key.strip()] = value.strip()
    return out


def main() -> int:
    failed = 0

    print("1. 凭据文件")
    path = accounts_path()
    if not path.is_file():
        problems(
            f"找不到 {path}。真验收会用到它。\n"
            "      它**必须放在仓库之外**：本仓库的 .gitignore **没有**覆盖这个名字，"
            "安全性完全靠「它在 HOME 下、不在版本库里」这一点。"
            "别为了图方便把它挪进仓库目录。"
        )
        return 1
    ok(f"{path}")

    print("2. 文件权限（口令是明文，不该让别人读得到）")
    mode = path.stat().st_mode
    if mode & (stat.S_IRGRP | stat.S_IROTH):
        problems(
            f"权限是 {stat.S_IMODE(mode):04o}，组/其他人也能读。"
            f"建议 `chmod 600 {path}`"
        )
        failed += 1
    else:
        ok(f"{stat.S_IMODE(mode):04o}")

    print("3. 解析")
    accounts = load(path.read_text(encoding="utf-8", errors="replace"))
    if not accounts:
        problems("文件里一段配置都没有（格式是 `[163]` 换行 `user = …` / `password = …`）")
        return 1
    ok(f"共 {len(accounts)} 段：{', '.join(sorted(accounts))}")

    print("4. 用例写死要的两段")
    for name in REQUIRED_SECTIONS:
        if name not in accounts:
            problems(
                f"缺 [{name}] 段。`real_mail_roundtrip.rs` 目前**写死**从 `163` 与 `qq` "
                "取账户——想用别的组合要先改那个文件"
            )
            failed += 1
            continue
        entry = accounts[name]
        user = entry.get("user", "")
        password = entry.get("password", "")
        if not user or not password:
            problems(f"[{name}] 的 user 或 password 是空的")
            failed += 1
            continue

        # 用户名的形状：163/沃通常是完整地址，QQ 常常只有号码。
        address = user if "@" in user else f"{user}@{PROVIDER_DOMAINS.get(name, '')}"
        ok(f"[{name}] {mask(address)}，口令 {len(password)} 个字符")
        if "@" not in user:
            print(
                f"      提示：用户名没写 `@`，用例会按 `{PROVIDER_DOMAINS.get(name, '?')}` "
                f"补成 {mask(address)}（与 `real_mail_roundtrip.rs` 的规则一致）"
            )

    print("5. 两个账户不能是同一家")
    if len(REQUIRED_SECTIONS) == 2 and all(s in accounts for s in REQUIRED_SECTIONS):
        a, b = (accounts[s].get("user", "") for s in REQUIRED_SECTIONS)
        if a and a == b:
            problems("两个段的 user 完全相同——「互发」会变成给自己发，验不出东西")
            failed += 1
        else:
            ok("两段是不同的账户")

    print()
    if failed:
        print(f"有 {failed} 个问题，先修掉再跑真验收。")
        return 1
    print("预检通过。真验收的命令：")
    print("  XREMAIL_REAL_ACCOUNTS=1 cargo test -p xremail-shell \\")
    print("      --test real_mail_roundtrip -- --ignored --nocapture")
    print("（它会**真的发一封邮件**出去，两个账户各收一次。）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
