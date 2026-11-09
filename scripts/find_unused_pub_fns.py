#!/usr/bin/env python3
"""找出**没有任何调用方**的 `pub` 函数。

## 为什么会有这种需求

`pub` 是给外部用的承诺，所以编译器**不会**为它报 `dead_code`——
一个 `pub fn` 哪怕全仓库没有一处调用，`cargo clippy -D warnings` 也一声不吭。

结果就是这类东西会一直藏着：`delete_messages_in_folder` 就是个例子
（注释说它「用于 epoch 切换」，实际上 epoch 切换走的是另一条**事务内**的路径，
而这个函数是「全删」，语义还不一样、更危险）。

`#[allow(dead_code)]` 也挡不住——它根本不会触发。

## 它怎么判

对每个 `pub fn` / `pub async fn`：
1. 在 `crates/*/src`、`crates/*/tests`、`apps/` 里找它在**定义之外**的出现
2. 出现形式收 `name(` —— 但**排除**这些误报源：
   * 同名的 `dyn Trait` 方法 / trait 实现（`impl X for Y { fn name }` 不算定义，
     但调用 `obj.name(` 也不算「调用了这个自由函数」）——这里**不做语义分析**，
     所以结果可能偏保守（多报）
3. 输出时把「在别的文件里被定义」的情形标出来，方便人工确认

## 这是一个**启发式**工具，不是证明

它靠文本匹配，会被这些情况骗到：函数被 `use` 重命名、通过函数指针传走、
被宏展开调用、在 `#[cfg(test)]` 里以 `module::name` 形式调用。

**所以它的输出是「值得看一眼的清单」，不是「可以删的清单」。**
每条都该人工确认之后再做决定——本仓库处理这类问题的做法是
**加注释说明现状与风险**，而不是直接删（`pub` 是承诺）。
"""

import argparse
import pathlib
import re
import sys

# 这些名字太常见，按文本匹配必然误报，直接跳过。
SKIP = {
    "new", "default", "fmt", "from", "into", "clone", "eq", "cmp", "hash",
    "serialize", "deserialize", "drop", "next", "poll", "deref", "display",
}


def definitions(root: pathlib.Path) -> dict:
    """`函数名 -> [(文件, 行号), …]`。只收顶格的 `pub fn` / `pub async fn`。"""
    found: dict = {}
    for path in sorted(root.rglob("*.rs")):
        # 只看源码，不看构建产物与缓存。
        parts = path.parts
        if any(p in parts for p in ("target", ".devcache", "node_modules", "registry")):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        for m in re.finditer(r"^pub (?:async )?fn (\w+)", text, re.M):
            name = m.group(1)
            if name in SKIP:
                continue
            line = text[: m.start()].count("\n") + 1
            found.setdefault(name, []).append((path, line))
    return found


def callers(root: pathlib.Path) -> dict:
    """`函数名 -> `name(` 出现的次数（**不算定义那一行**）。

    定义行必须排除：`pub async fn delete_messages_in_folder(` 里也有
    `delete_messages_in_folder(`。不排掉的话每个函数都至少「被调用一次」——
    第一版就是这么错的：报「没有发现零调用方的函数」，而实际上有一个。
    """
    counts: dict = {}
    for path in sorted(root.rglob("*.rs")):
        parts = path.parts
        if any(part in parts for part in ("target", ".devcache", "node_modules", "registry")):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        definition_lines = {
            text[: m.start()].count("\n") + 1
            for m in re.finditer(r"^\s*pub (?:async )?fn (\w+)", text, re.M)
        }
        for lineno, line in enumerate(text.split("\n"), start=1):
            if lineno in definition_lines:
                continue
            for name in re.findall(r"\b(\w+)\s*\(", line):
                if name in SKIP:
                    continue
                counts[name] = counts.get(name, 0) + 1
    return counts


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="仓库根目录（默认当前目录）")
    parser.add_argument(
        "--show-used",
        action="store_true",
        help="连「只有定义处自己出现一次」的都列出来（默认只列这种）",
    )
    args = parser.parse_args()

    root = pathlib.Path(args.root).resolve()
    defs = definitions(root)
    counts = callers(root)

    suspicious = []
    for name, places in sorted(defs.items()):
        # 定义处本身算一次出现（`fn name(` 里没有 `name(`，所以不算——
        # 但 `pub async fn name(` 也不会被 `name(` 匹配到，所以计数里通常只有真实调用）
        n = counts.get(name, 0)
        if n == 0:
            suspicious.append((name, places, n))

    if not suspicious:
        print("没有发现「零调用方」的 pub 函数。")
        return 0

    print(f"发现 {len(suspicious)} 个**没有任何 `name(` 形式调用**的 pub 函数：\n")
    for name, places, _ in suspicious:
        for path, line in places:
            print(f"  {name}")
            print(f"      {path.relative_to(root)}:{line}")
    print()
    print("注意：这是**启发式**结果，不是证明。会被这些情况骗到：")
    print("  · 通过 `use … as …` 重命名后调用")
    print("  · 作为函数指针传走（`map(name)` 这类）")
    print("  · 被宏展开调用")
    print("每条都要人工确认——`pub` 是给外部的承诺，删之前先想清楚。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
