#!/usr/bin/env python3
"""从 CI 历史里统计某个 job 的**偶发失败率**。

## 为什么需要它

风险库里有一类问题长这样：「某条用例偶发失败，重跑就好」（R49），
或者「换了个写法之后应该好了」。这两件事都**只能靠频率来回答**，
而频率不在任何文档里——它散在几百次 CI 运行里。

人肉翻 `gh run list` 翻到第十几次就会开始糊弄。这个脚本把这件事做成可重复的：

```bash
python3 scripts/ci_failure_rate.py --job "Rust (windows" --limit 30
python3 scripts/ci_failure_rate.py --job "Rust (ubuntu" --limit 50 --since 2026-11-10
```

## 它输出什么

* 这个 job 在最近 N 次运行里成功 / 失败 / 取消各几次
* 失败那几次的**运行 id**（可以直接 `gh run view <id> --log-failed` 去看）
* 一条**样本量提醒**：N 次全成功，在已知基线概率 p 下，「其实没修好却没复现」
  的概率是 `(1-p)^N`。这个数经常大得让人意外——6 次全绿在 6% 基线下的
  漏判概率是 69%

## 依赖

需要 `gh` 已登录（`gh auth status`），且对目标仓库有读权限。
**它不写任何东西**，只读。

## 一个诚实的限制

job 失败 ≠ 你关心的那条用例失败。同一个 job 里可能有好几条用例，
脚本只能告诉你「这个 job 失败了」，判断是哪条要自己看日志（脚本会打印
可以直接执行的 `gh` 命令）。
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


def find_gh() -> str:
    """找 `gh` 可执行文件。

    不一定在 `PATH` 里：本机的 `gh` 装在 `~/.local/bin`，而脚本可能从
    cron / 别的 shell 里调起来，PATH 不一样。先按 `PATH` 找，再试几个常见位置。
    """
    found = shutil.which("gh")
    if found:
        return found
    for candidate in (Path.home() / ".local" / "bin" / "gh", Path("/usr/local/bin/gh")):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    print(
        "找不到 `gh`。这个脚本靠它读 CI 历史。\n"
        "  装法见 https://cli.github.com/ ，装完 `gh auth login` 一次。",
        file=sys.stderr,
    )
    sys.exit(1)


GH = find_gh()


def gh(*args: str) -> str:
    """跑一条 `gh` 命令，返回 stdout。失败时直接退出并带上 stderr。"""
    result = subprocess.run(
        [GH, *args], capture_output=True, text=True
    )
    if result.returncode != 0:
        print(f"gh {' '.join(args)} 失败：{result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)
    return result.stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--job", required=True, help="job 名的前缀，如 `Rust (windows`")
    parser.add_argument("--limit", type=int, default=30, help="翻最近多少次运行（默认 30）")
    parser.add_argument("--repo", default=None, help="仓库（默认取当前目录）")
    parser.add_argument(
        "--baseline",
        type=float,
        default=None,
        help="已知的历史失败率（0–1），给了就额外算「漏判概率」",
    )
    args = parser.parse_args()

    repo_args = ["--repo", args.repo] if args.repo else []
    runs = json.loads(
        gh("run", "list", *repo_args, "--limit", str(args.limit),
           "--json", "databaseId,conclusion,headSha,createdAt")
    )
    if not runs:
        print("没有找到运行记录。")
        return 1

    success = failure = other = 0
    failed_runs = []
    for run in runs:
        jobs = json.loads(
            gh("api", f"repos/{repo_of(repo_args)}/actions/runs/{run['databaseId']}/jobs")
        ).get("jobs", [])
        matched = [j for j in jobs if j["name"].startswith(args.job)]
        if not matched:
            # 这次运行里没有这个 job（矩阵没展开、或者被跳过）——不算样本。
            continue
        conclusion = matched[0]["conclusion"]
        if conclusion == "success":
            success += 1
        elif conclusion == "failure":
            failure += 1
            failed_runs.append((run["databaseId"], run["headSha"][:8], run["createdAt"]))
        else:
            other += 1

    total = success + failure + other
    print(f"job 前缀：{args.job}")
    print(f"样本：{total} 次（成功 {success} / 失败 {failure} / 其它 {other}）")
    if total == 0:
        print("没有匹配的样本——job 名写错了？")
        return 1
    rate = failure / total
    print(f"失败率：{rate:.1%}")

    if failed_runs:
        print("\n失败的运行（可以直接去看日志）：")
        for run_id, sha, created in failed_runs:
            print(f"  gh run view {run_id} --log-failed    # {sha} {created}")

    # 样本量提醒：这是本脚本存在的理由。
    baseline = args.baseline if args.baseline is not None else rate
    if baseline > 0 and success > 0:
        miss = (1 - baseline) ** success
        print(
            f"\n样本量提醒：若真实失败率是 {baseline:.1%}，"
            f"**{success} 次全成功**在「其实没修好」的前提下也会发生的概率是 "
            f"{miss:.0%}。"
        )
        if miss > 0.05:
            print(
                "  → 这个样本量**不足以**说「已修好/已缓解」。"
                "要让漏判概率降到 5% 以下，需要大约 "
                f"{needed(baseline)} 次连续成功。"
            )
        else:
            print("  → 可以初步认为已缓解（漏判概率 < 5%）。")

    return 0


def repo_of(repo_args: list) -> str:
    """`gh` 的 `--repo` 给了就用它，否则问当前目录对应的仓库。"""
    if repo_args:
        return repo_args[1]
    return gh("repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner").strip()


def needed(baseline: float) -> int:
    """要让 (1-p)^n < 0.05，n 至少是多少。"""
    import math

    if baseline <= 0 or baseline >= 1:
        return 0
    return math.ceil(math.log(0.05) / math.log(1 - baseline))


if __name__ == "__main__":
    sys.exit(main())
