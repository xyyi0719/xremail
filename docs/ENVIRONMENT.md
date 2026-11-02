# 环境特性与平台差异

> 用途：记录**本机与各目标平台**的实测特性，避免在脚本、CI、命令和文档中重复踩坑。
> 维护者：xyyi0719 · 最后实测：2026-10-21
> 原则：**只写实测过的**。未验证的写「未验证」，不写推测。
> **本机手动设置的值（代理地址、凭据、密钥路径等）不写进本文档**，只写读取方法。

---

## 1. 本机开发环境（Windows）

### 1.1 工具链实测版本

| 工具 | 版本 | 备注 |
|---|---|---|
| rustc / cargo | 1.99.1 | 满足 `rust-version = 1.94` |
| Node.js | 22.21.0 | |
| npm | 随 Node | `package-lock.json` 已提交 |
| Python | 3.11.0（可执行文件是 **`python`**，在 `C:\Python311`） | ⚠️ 本机的 **`python3` 是 Microsoft Store 的空壳**（`…\WindowsApps\python3`）：直接退出 49、什么都不输出。**本地跑 `scripts/check_doc_links.py` 要用 `python`**；CI（ubuntu）上的 `python3` 是真的 |
| OpenJDK | **17.0.20.1**（`~/.jdks/jdk-17.0.20.1+1`） | 2026-10-21 装的 Microsoft OpenJDK 17，**免安装 zip**，装在用户目录无需管理员。机器级另有 Corretto **1.8.0_502**（`JAVA_HOME` 用户级已指向 17） |
| Android SDK | cmdline-tools 13.0，`%LOCALAPPDATA%\Android\Sdk` | 含 platform-tools 37.0.1、platforms android-36 / 37.0、build-tools 36.0.0 / 37.0.0 |
| Android NDK | **27.2.12479018**（side by side） | Tauri v2 + cargo-ndk 的稳妥组合 |
| cargo-ndk | 4.1.2 | `cargo install cargo-ndk --locked` |
| cargo-audit | 0.22.2 | `cargo install cargo-audit --locked`；[SECURITY §1 T12](SECURITY.md) 的供应链防护。**拉 advisory-db 要走代理**（`https_proxy=... cargo audit`），否则卡在 `github.com` DNS 劫持 |
| cargo-deny | 0.20.2 | `cargo install cargo-deny --locked`；依赖许可与来源白名单 |
| git-filter-repo | a40bce548d2c | `python -m pip install --user git-filter-repo`（免管理员）；换历史、改目录名时用 |
| gh CLI | 2.102.0 | 已登录 `xyyi0719` |
| Git | 内置 | `filter-repo` **已装**（pip --user），但 `~/.cargo/bin` 与 `AppData\Roaming\Python\Python311\Scripts` 需在 PATH 上 |

> **Android 构建链已打通（2026-10-21）**，只剩一个需要重登录的权限项，见 §1.8。
> 本机**非管理员**，所有组件都装在用户目录，未动 `Program Files`。

### 1.2 Shell 差异（最容易出错的地方）

**Windows 默认 shell 是 PowerShell，不是 bash。**

| 语法 | PowerShell | bash | 备注 |
|---|---|---|---|
| `$(cmd)` | ❌ 不支持（是数组字面量） | ✅ | **CI Windows job 必须显式 `shell: bash`** |
| `grep` | ❌ 需装 Git Bash 或用 `Select-String` | ✅ | |
| `2>/dev/null` | ❌ | ✅ | |
| `\|\|` 兜底 | ✅ | ✅ | |
| `cmd1 && cmd2` | ✅（新版） | ✅ | |

> **实例（真实踩坑）**：CI 的用例计数步骤最初写成 bash 语法，Ubuntu job 通过、
> **Windows job 失败**：
> ```
> The term 'count=$(cargo test ...)' is not recognized
> ```
> 修法：`shell: bash`，并在本机 Git Bash 下先验证命令输出。

**在本机执行 bash 语法命令**：用 `bash -c '...'` 显式调用，不要假设默认 shell。

### 1.3 命令行的多层引号陷阱

`ctx_execute` / `bash` 工具会把命令再交给外层 shell（cmd 或 sh），
命令**自身**的引号会被再解释一层。

| 坑 | 现象 | 正确做法 |
|---|---|---|
| `curl -w "%{http_code}"` | `%{` 被外层展开 → 输出 `%{http_code}` 字面量 | 用 `\n__CODE__%{http_code}` 之类**无歧义标记**，再正则提取 |
| `grep -c ": test$"` | `$` 被外层吞掉 | 单引号包裹整个 pattern |
| 命令里含 `'`（如 `don't`） | 提前闭合引号 | 改用双引号或转义 |
| `cmd` 中的 `&` `\|` `>` | 被当作管道/重定向 | 引号包裹整条命令 |

> **原则**：需要精确控制时，**用脚本文件**（写入临时 `.py`/`.js` 再执行），
> 不要把复杂逻辑塞进单行命令字符串。

### 1.4 不可用的命令

| 命令 | 原因 |
|---|---|
| `timeout`（GNU coreutils） | 本机无此命令；`timeout /t 10` 在非交互 shell 下报 exit 125 |
| `grep`（纯 PowerShell 环境） | 见 §1.2 |
| `git filter-repo` | 未安装 |
| `sed -i` | Windows 上行为与 GNU 有差异，改用 Node 脚本改文件 |

### 1.5 网络与代理（重要）

**系统代理由维护者手动设置在 Windows 系统设置里，地址随网络环境变化，本文不写死具体地址。**
用之前先读**当前值**：

```bash
# Git Bash / MSYS 下 /v 会被当成路径改写，用 //v 或 MSYS_NO_PATHCONV=1
reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings' //v ProxyServer
reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings' //v ProxyEnable
reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings' //v ProxyOverride
```

| 注册表值 | 含义 |
|---|---|
| `ProxyEnable` | `1` = 代理开启，`0` = 关闭 |
| `ProxyServer` | 当前代理 `host:port`（HTTP 代理） |
| `ProxyOverride` | **绕过代理**的主机列表；本机实测含 `localhost`、`127.*`、`10.*`、`172.16.*`–`172.31.*`、`192.168.*`、`*.cn`、`*.local`、`*.cloud`、`<local>` → 内网地址与 `*.cn` 走直连 |

**仓库级 `http.proxy` / `https.proxy` 同样是手动写的**：落在 `.git/config`，
而 `.git/` 不被 git 跟踪，因此**不会随仓库分发**，换机器必须重设。
查看当前值：`git config --local --get http.proxy`。

**关键差异**：

| 工具 | 是否自动使用系统代理 | 说明 |
|---|---|---|
| `gh` | ✅ 会 | Go 运行时读取 Windows 代理设置 |
| `git` | ❌ 不会 | 必须显式 `git config http.proxy` |
| `curl` | ❌ 不会 | 必须显式 `curl -x` |
| Node `fetch` | ❌ 不会 | 需要显式代理 agent |

**DNS 劫持**：`github.com` 在本机解析到 **127.0.0.1**（企业网络策略），而 `api.github.com` 正常。

| 目标 | 直连 | 走代理 | 绕行方式 |
|---|---|---|---|
| `api.github.com` | ✅ 200 | ✅ 200 | — |
| `github.com` | ❌ 解析到 127.0.0.1 | ✅ 200 | 走代理，或 `--resolve` 配实时 IP |
| `github.com:22`（SSH） | ❌ 连接被拒 | — | 改用 **SSH over 443**：`ssh://git@ssh.github.com:443/...` |
| `developers.google.com` | ❌ 连接超时 | — | 无法核实 Google 官方文档，相关结论须标注未核实 |
| `crates.io` / `npm registry` | ✅ 可达 | — | 依赖版本可实时核对 |

**`--resolve` 用的 GitHub 公网 IP 会轮换，不要写死**。需要时经代理走 DoH 实时取：

```bash
PXY=$(reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings' //v ProxyServer \
      | tr -d '\r' | sed -n 's/^.*REG_SZ[ ]\{1,\}\(.*\)$/\1/p')
curl -sS -x "http://$PXY" -H 'accept: application/dns-json' \
     'https://dns.google/resolve?name=github.com&type=A'
```

**SSH 密钥、`.ssh/config` 主机别名、`.git/config` 的 `core.sshCommand` 与 remote URL 都是本机手动配置，不入库。**
本仓库 remote 走 **SSH over 443**（`ssh://git@ssh.github.com:443/...`），因为 22 端口在本机被拒。

**已知限制**：`gh auth refresh` 需访问 `github.com`（被劫持），**无法自助扩权**。
需要新 scope（如 `delete_repo`）时只能：
1. 手动改 hosts（**本机无写权限**，EPERM）
2. 用 `curl --resolve` 走设备流（已验证可行，但需人工在浏览器输入设备码）

#### 1.5.1 连 GitHub 前先探明

**每次要访问 GitHub（推送、拉取、调 API、开 PR）之前，先探明能不能连通，再执行操作。**
本机 `github.com` 被 DNS 劫持，直连必失败，很容易误判成「没网」或「仓库不存在」。

```bash
# 1) 推送走的真实通道（SSH over 443）——最贴近 push 本身
git ls-remote origin

# 2) API 通道（gh 自动读系统代理）
gh auth status
gh api rate_limit --jq '.rate.remaining'

# 3) HTTPS 通道（需显式 -x 走系统代理）
PXY=$(reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings' //v ProxyServer \
      | tr -d '\r' | sed -n 's/^.*REG_SZ[ ]\{1,\}\(.*\)$/\1/p')
curl -sS -o /dev/null -w 'code=%{http_code}\n' -x "http://$PXY" https://api.github.com
```

判读：

| 探针结果 | 结论 | 下一步 |
|---|---|---|
| `git ls-remote` 成功 | 推送通道可用 | 可以 `git push` |
| `git ls-remote` 失败但 `gh api` 成功 | 仅 HTTPS / API 通道可用 | 检查 remote 是否为 SSH over 443 |
| 全部失败 | 代理未开或网络不通 | 读注册表 `ProxyEnable` / `ProxyServer` 当前值 |
| `curl` 报 `Unsupported proxy syntax` | 提取代理的管道写错了 | 核对 `PXY` 提取结果是否为空 |

**本机到 GitHub 的连接是间歇性的（2026-10-21 实测）**：

连续 5 次采样，同一批命令的结果在**成功与失败之间跳变**：

| 探针 | 5 次中的成功次数 | 失败时的现象 |
|---|---|---|
| `git ls-remote origin` | 3/5 | `Connection reset by <ip> port 443` |
| `gh api rate_limit` | **5/5** | — |
| `curl -x <代理> https://api.github.com` | 1/5 | `schannel: failed to receive handshake` |

→ **单次探针失败不等于断网**。先重试再下结论；`gh` 走 Go 运行时代理、最稳，
`curl`（Windows schannel）过企业代理最容易 TLS 握手失败，**不要把它当唯一判据**。

**三种失败分属不同通道，别混判（2026-10-21 逐条定位）**：

| 通道 | 失败信息 | 含义 |
|---|---|---|
| SSH over 443（推送走这条） | `Connection reset by <ip> port 443` + `fatal: Could not read from remote repository.` | **TCP 443 能连上，SSH 会话被中途 reset**；与证书、密钥、仓库权限都无关 |
| `gh api`（HTTPS / 代理） | `net/http: TLS handshake timeout` | Go 运行时与企业代理之间的 TLS 握手超时 |
| `curl`（HTTPS / 代理） | `schannel: failed to receive handshake, SSL/TLS connection failed` | Windows schannel 握手失败，**最不稳，不可作唯一判据** |

**遇到 SSH 的 `Connection reset` 不要换 GitHub IP**：2026-10-21 实测 9 个候选
（`20.205.243.160`、`20.205.243.166`、`20.27.177.113`、`20.200.245.247`、
`20.207.73.82`、`20.248.137.48`、`140.82.112.3`、`140.82.113.3`、`140.82.114.3`）：

- TCP 443 对**所有**候选都通 → 问题不在路由，在会话层；
- 但**只有本地 DNS 解析到的那个**接受 `ssh.github.com` 的 SSH 服务，其余全部
  `Connection closed by <ip> port 443`。

→ **换 IP 无效，只能重试。** 候选 IP 也会轮换，不要写进脚本。

**探不通就不要 push**：连接失败时反复推送只会产生半截状态或让 CI 空跑。
先按上表定位到具体通道，修好再推，并把实际探针输出当作证据记录。

**探针与 push 必须放在同一个重试循环里**：探针通过之后，`push` 自身仍可能在传输中被 reset。
2026-10-21 的一次真实推送，探针在前 3 次全失败、**第 4 次**才通过 —— `TRIES=3` 不够用。

**判成功只看 `git push` 自己的退出码**（2026-10-21 实测踩坑）：

- **不要给 `git push` 接管道**。`git push origin main 2>&1 | tail -3` 的退出码是 `tail` 的，
  永远是 0 —— 循环会把失败读成成功，把成功读成失败（实测一次已成功推送被误报为失败并重试了 40 次）。
  要保留输出就**重定向到文件**，别用管道。
- **推送后用 `ls-remote` 核验，本身也会间歇性失败**。核验失败 **不等于** 没推上去；
  以 `git push` 的退出码为准，核验要带重试。
- 核验要**比对 SHA**，不要只看命令是否报错：

```bash
# 正确的核验：比对远端 main 与本地 HEAD
for i in $(seq 1 8); do
  RSHA=$(timeout 30 git ls-remote origin refs/heads/main 2>/dev/null | cut -f1)
  [ -n "$RSHA" ] && break
  sleep 3
done
[ "$RSHA" = "$(git rev-parse HEAD)" ] && echo "远端已同步" || echo "远端未同步"
```

建议用带重试的探针 + 推送脚本（推送前跑一次，通过即推）：

```bash
#!/usr/bin/env bash
# GitHub 连通性探针 + 推送：本机连接间歇性失败，必须重试。
# 探针和 push 放在同一循环：探针过了 push 也可能被 reset。
# TRIES=6 是刻意收敛的上限：探针单次成功率约 25%，整轮全失败的概率约 18%。
# 6 次仍不通就停下报给维护者，不要继续刷。
TRIES=6
for i in $(seq 1 $TRIES); do
  if git ls-remote origin >/dev/null 2>&1; then
    echo "[$i/$TRIES] 探针 OK -> 执行 push"
    if git push origin main; then
      echo "PUSH_OK"; exit 0
    fi
    echo "push 失败（可能传输中被 reset），继续重试…"
  else
    echo "[$i/$TRIES] 探针失败（连接被 reset），3 秒后重试…"
  fi
  sleep 3
done
echo "PUSH_FAIL: $TRIES 次尝试内未完成推送；停下报给维护者，不要继续刷"; exit 1
```

> 2026-10-21 实测：该脚本把一次真实推送送到了远端（第 4 次尝试通过），
> 随后 CI run `39559605711` 的 4 个 job 全绿。

### 1.6 文件系统

| 项 | 情况 |
|---|---|
| `C:\Windows\System32\drivers\etc\hosts` | **只读**（EPERM），无法加 hosts 条目 |
| 行尾 | `.gitattributes` 统一 `text=auto eol=lf`；Windows 上 `git add` 会提示 CRLF→LF 转换（正常） |
| 路径分隔符 | Rust 用 `std::path::Path`；文档链接用 `/`；**代码注释里的相对路径按 crate 位置算层级**（见下） |

### 1.7 代码注释里的文档相对路径

路径是**相对于源文件所在目录**的，不是相对于仓库根。

| 文件 | 正确层级 |
|---|---|
| `crates/xremail-core/src/*.rs` | `../../../docs/X.md`（src → crate → crates → 根） |
| `apps/shell/src-tauri/src/*.rs` | `../../../../docs/X.md`（src → src-tauri → shell → apps → 根） |

> 曾在 4 个文件里全部写成少一层，被 CI 的 doc-comment 检查抓出。
> **CI 已加自动检查**（见 `.github/workflows/ci.yml` 的 docs job）。

### 1.8 Android 工具链（2026-10-21 装）

非管理员环境下用**免安装 zip** 装在用户目录，未动 `Program Files`：

| 组件 | 位置 | 装法 |
|---|---|---|
| JDK 17 | `C:\Users\UNI-LX\.jdks\jdk-17.0.20.1+1` | `https://aka.ms/download-jdk/microsoft-jdk-17-windows-x64.zip` 解压即用 |
| Android SDK | `C:\Users\UNI-LX\AppData\Local\Android\Sdk` | `commandlinetools-win-11479570_latest.zip` 解压到 `cmdline-tools/latest` |
| NDK | `%ANDROID_HOME%\ndk\27.2.12479018` | `sdkmanager "ndk;27.2.12479018"` |

**用户级环境变量**（机器级 `JAVA_HOME` 仍指向 Corretto 8，用户级优先级更高）：
`JAVA_HOME`、`ANDROID_HOME`、`ANDROID_SDK_ROOT`、`NDK_HOME`。

Rust 侧：`rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android` + `cargo install cargo-ndk --locked`。

**踩过的坑**：

1. **JDK 下载源**：Adoptium API 会 307 跳到 `github.com`，而本机 `github.com` 被 DNS 劫持到
   127.0.0.1 → 下载必失败。改用 `aka.ms/download-jdk/microsoft-jdk-17-windows-x64.zip`
   （跳 `download.visualstudio.microsoft.com`，直连可用）。
2. **包名不是 `platforms;android-37`**：实际是 **`platforms;android-37.0`**（还有 37.1 / 37.2），
   装到 `platforms\android-37.0` 目录。Tauri 生成的 Gradle 里是 `compileSdk = 37` / `targetSdk = 37`。
3. **`sdkmanager` 需要 JDK 17**：本机默认 `java` 是 JDK 8，直接跑 `sdkmanager` 会抛
   `ClassNotFoundException`。必须先把 `JAVA_HOME` 指向 JDK 17 再跑。
4. **Git Bash 下 `reg add` / `reg query` 的 `/v` `/t` `/d` `/f` 会被 MSYS 当路径改写**，
   报「无效语法」。用 `MSYS_NO_PATHCONV=1` 或 `//v` 写法（与 §1.5 同一个坑）。
5. **桌面端专属插件在 Android 上没有 API**：`tauri-plugin-single-instance` 与
   `tauri-plugin-autostart` 的 `init` / `MacosLauncher` 只在桌面端存在，
   编 Android 目标时直接编译失败 → 需要 `#[cfg(desktop)]` 隔离。

**当前唯一卡点：Windows 符号链接权限**。

`tauri android build` 要把编好的 `.so` **符号链接**进 `app/src/main/jniLibs/<abi>/`。
非管理员 + 未开开发者模式时失败：

```text
failed to build Android app: Failed to create a symbolic link ...
Creation symbolic link is not allowed for this system.
```

已把开发者模式写进注册表（用户级，无需管理员）：

```bash
MSYS_NO_PATHCONV=1 reg add 'HKCU\Software\Microsoft\Windows\CurrentVersion\AppModelUnlock' \
  /v AllowDevelopmentWithoutDevLicense /t REG_DWORD /d 1 /f
```

但**该权限在登录时随 token 发放，写入注册表后当前会话仍无权限**
（`whoami /priv` 看不到 `SeCreateSymbolicLinkPrivilege`，`mklink` 仍报「没有足够的权限」）。
→ **需要注销后重新登录（或重启）**，权限才会生效。预置同名普通文件绕不过去
（Tauri 每次都会重建符号链接）。以管理员身份跑一次也可以。

**已验证可用的部分（2026-10-21）**：
`tauri android init` 成功（自动识别 NDK 27.2.12479018）→
`cargo build --target aarch64-linux-android` 成功，
产物经 `file` 确认为 `ELF 64-bit LSB shared object, ARM aarch64`。
即 **Rust × NDK × cargo-ndk × Gradle 整条链已通**，只差上面那个链接权限。

### 1.9 Tauri 桌面端在 Windows 上的两个坑（2026-10-22 踩到）

#### 1.9.1 强杀应用会留下 WebView2 残留进程，导致下次启动失败

用 `kill` / `taskkill` 结束 Tauri 应用时，**子进程 `msedgewebview2.exe` 不会被一起收掉**。
它继续占着 WebView2 的用户数据目录
（`%LOCALAPPDATA%/<identifier>/EBWebView`），下次启动时创建 webview 失败：

```
panicked at tauri-2.12.4/src/app.rs:1444: Failed to setup app:
  runtime error: failed to create webview:
  WebView2 error: WindowsError(Error { code: HRESULT(0x800700AA),
  message: "请求的资源在使用中。" })
```

`0x800700AA` = `ERROR_BUSY`。

**迷惑之处**：Tauri 的 `setup()` 内部是**先按 `tauri.conf.json` 建窗口、再跑用户的 setup 钩子**
（见 `tauri-2.12.4/src/app.rs` 的 `fn setup`）。所以这个错误发生在**用户钩子之前**，
看起来像"我的 setup 代码没跑"——实际上数据目录根本没被创建。
排查时先确认应用逻辑是否执行，别被这个顺序误导。

清理方式：

```bash
taskkill //F //IM msedgewebview2.exe
```

验证时每次跑完应用都要清，否则第二次必然失败。

#### 1.9.2 测试二进制缺少 comctl32 v6 的 manifest

`tauri::test`（`mock_builder` 等）会把 wry 整条链拉进测试二进制，
于是测试二进制**静态导入** `comctl32.dll!TaskDialogIndirect`。
该导出只有 comctl32 v6（SxS 版本）才有；`tauri-build` 只给**主程序**嵌入
v6 的 manifest，测试二进制没有 → 加载器把 `comctl32.dll` 解析到
System32 的 v5.82 → 找不到导出 → 进程以
`STATUS_ENTRYPOINT_NOT_FOUND`（`0xC0000139`）退出，**测试一个都跑不了**。

解法（见 `apps/shell/src-tauri/build.rs`）：把 comctl32 改成**延迟加载**，
加载期就不再要求那个导出：

```rust
println!("cargo:rustc-link-arg-tests=/DELAYLOAD:comctl32.dll");
println!("cargo:rustc-link-lib=delayimp");   // 提供 __delayLoadHelper2
```

`delayimp.lib` 不能少，否则链接期报 `LNK2001 无法解析的外部符号 __delayLoadHelper2`。

#### 1.9.3 sqlx 连接池与创建它的运行时绑定

sqlx 的连接池在**创建它的运行时**上工作。若在 `#[tokio::test]` 的 tokio 运行时里建池，
而 Command 在 Tauri 自己的运行时上取连接，就会一直等不到连接，最终
`pool timed out while waiting for an open connection`。

在 `tauri::test` 里测 Command 时，**连接池要在 Tauri 的运行时上建**：

```rust
let pool = tauri::async_runtime::block_on(async {
    let pool = open_pool_in_memory(&cfg).await?;
    migrate(&pool).await?;
    pool
});
```

（这条与 §1.9.1/1.9.2 无关，是独立的一条。）

### 1.10 Tauri IPC 测试的本地 origin 是**平台相关**的（2026-10-26 CI 首次推送时踩到）

在 `tauri::test` 里手工构造 `InvokeRequest` 时，**`url` 必须是当前平台的本地 origin**：

| 平台 | 本地 origin |
|---|---|
| Windows / Android | `http://tauri.localhost` |
| 其它（Linux / macOS） | `tauri://localhost` |

（出自 Tauri 的 `protocol::origin`：Windows 与 Android 用 `http(s)://{scheme}.localhost` 绕行，
其它平台用 `{scheme}://localhost`。）

**写错的后果**：请求被当成**远程来源**（`is_local = false`）→ 走 ACL 检查 →
而 mock 应用的 ACL 是**空的** → 命令被一律拒绝：

```text
create_account not allowed. Plugin not found
```

**这个坑在 Windows 上看不见**（那里恰好硬编码对了），
只有 Linux CI 才暴露。而本地 mock 应用的 ACL 为空是 Tauri 的设计
（`mock_builder` 用 `Resolved::default()`），不是我们的配置问题。

**验证手段**（不需要 Linux）：在 Windows 上把 URL 换成 `tauri://localhost`，
应当立刻复现出同样的错误——这能证明机制判断没错。

修法见 `apps/shell/src-tauri/tests/command_ipc.rs` 的 `local_origin()`。

---

## 2. 目标平台差异

### 2.1 平台支持状态（截至 2026-10-21）

| 平台 | 状态 | 证据 |
|---|---|---|
| **Windows** | ✅ 可编译可运行 | 本机构建通过；CI `windows-latest` 绿 |
| **Linux (Ubuntu)** | ✅ **可编译**（运行未验证） | CI `ubuntu-latest` 绿，且 ubuntu job 编译了 tauri 应用本体 |
| **Android** | 🟡 **Rust 侧可交叉编译**；完整 APK 打包待符号链接权限 | 工具链已装（见 §1.8）；`aarch64-linux-android` 产物已验证为 ARM aarch64 ELF；APK 打包卡在 Windows 符号链接权限（需重登录） |
| **macOS** | ❓ 未验证 | 本机无 macOS；CI 未覆盖 |
| **iOS** | ❓ 未验证 | 需要 Apple 开发者账号 |

> Linux 编译是 **CI 实际证据**，不是推断：workspace 含 `apps/shell/src-tauri`，
> ubuntu job 执行 `cargo clippy/test --workspace`，且预装了 `libwebkit2gtk-4.1-dev`。

### 2.2 Linux 构建依赖

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev librsvg2-dev patchelf
sudo apt-get install -y libayatana-appindicator3-dev \
  || sudo apt-get install -y libappindicator3-dev
```

| 包 | 用途 | 缺失后果 |
|---|---|---|
| `libwebkit2gtk-4.1-dev` | WebView（注意是 **4.1**，不是 4.0） | 编译失败 |
| `librsvg2-dev` | 图标 | 编译失败 |
| `patchelf` | 修补动态库路径 | 打包失败 |
| `libayatana-appindicator3-dev` | 托盘图标 | 托盘不可用（可退化） |

### 2.3 各平台的 WebView 差异（**最重要的未验证项**）

| 平台 | 引擎 | 版本 | 对本项目的影响 |
|---|---|---|---|
| Windows | **Chromium**（WebView2） | 跟随 Edge | 安全模型的设计基准 |
| Android | **Chromium**（WebView） | 随系统更新，碎片化 | 需真机矩阵 |
| Linux | **WebKitGTK** | 跟随发行版 | ⚠️ **与 Chromium 行为不同** |
| macOS | **WebKit** | 跟随系统 | ⚠️ 同上 |

**为什么这件事重要**：我们的 HTML 邮件安全模型（[SECURITY §3](SECURITY.md)）依赖
sandbox iframe + CSP + `srcdoc`。这些行为在 **WebKitGTK 上与 Chromium 存在差异**：

- `sandbox` 属性对 `allow-popups` / `allow-popups-to-escape-sandbox` 的处理
- `srcdoc` 内联文档的 CSP 继承与 `meta` CSP 生效顺序
- `default-src 'none'` 能否真正阻断远程图片请求
- `blob:` / `data:` 在 opaque origin 下的可用性
- 外部链接在无 `allow-scripts` 下的可点击性

→ **结论**：Windows/Android 上验证过的沙箱行为，**不能直接推断到 Linux/macOS**。
在 WebKit 系平台上线前，必须重跑 [TESTING_STRATEGY §2.4](TESTING_STRATEGY.md) 的
网络请求计数、脚本执行、导航与 IPC 隔离测试。

### 2.4 平台相关 API 差异

| 能力 | Windows | Linux | macOS | Android |
|---|---|---|---|---|
| 系统凭据库 | Credential Manager | **Secret Service**（gnome-keyring/kwallet） | Keychain | **Keystore**（需自写适配） |
| 托盘 | 原生 | libayatana-appindicator | 原生 | 不适用 |
| 通知 | 原生 | **D-Bus / libnotify** | 原生 | NotificationManager |
| 开机自启 | Registry / Startup | **XDG autostart .desktop** | LaunchAgent | Background 限制 |
| 应用数据目录 | `%APPDATA%` | `~/.local/share` | `~/Library/Application Support` | app 内部存储 |

> `keyring` crate 4.x **没有按平台划分的 feature**（平台后端已拆为独立 crate），
> 且**不能假定自动支持 Android**。见 [SECURITY §2.1](SECURITY.md)。

### 2.5 前端无平台分支

当前 `apps/shell/src/App.tsx` **没有任何 OS 判断**（无 `navigator.platform` /
`userAgent` 分支）。所有平台差异都落在 Rust 侧与 Tauri 插件上。

→ 这是一件好事：平台差异集中处理，不必在前端散落 `if (isWindows)`。

### 2.6 Ubuntu 26.04 开发机准备清单

> 面向“用 Ubuntu 26.04 机器开发”的场景。**包名与版本已对 Ubuntu 26.04 (resolute) 官方仓库核实**，
> 标「未验证」的部分需在该机器上实测后回填。

**发行版基线**：26.04 LTS "Resolute Raccoon"（2026-04-23 发布，支持到 2031-04）。

| 工具链 | 24.04 | 26.04 | 对本项目的影响 |
|---|---|---|---|
| glibc | 2.39 | **2.43** | ⚠️ 决定 .deb 的兼容下限（见下） |
| GCC | 13.2 | 15.2 | |
| binutils | 2.42 | 2.46 | |
| LLVM / Clang | 18 | **21.1** | `ring` / `bindgen` 需匹配 |
| Python | 3.12 | **3.14** | CI docs job 用 `python3` |
| Rust（发行版包） | 1.75 | **1.93.1** | **不满足** `rust-version = 1.94`，发行版包滞后 → 用 rustup |
| OpenJDK（默认） | 21 | **25** | Android 工具链要 **17** → 额外装 `openjdk-17-jdk` |
| Kernel / GNOME | | 7.0.0 / 50 | |

**系统依赖**（包名已在 26.04 仓库核实存在）：

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  libsoup-3.0-dev \
  build-essential \
  curl wget file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  patchelf \
  xdg-utils \
  openjdk-17-jdk
```

- `libwebkit2gtk-4.1-dev` 在 26.04 是 `2.52.6-0ubuntu0.26.04.1`，**Tauri v2 需要的正是 4.1 API** ✅
  （24.04 也是 4.1；22.04 只有 4.0，不支持 Tauri v2）。
- `libxdo-dev` 在 **universe** 仓库 → 先确认已启用：
  `sudo add-apt-repository universe && sudo apt update`
- `libayatana-appindicator3-dev` 26.04 是 `0.5.94-1build1`（main）✅
- `librsvg2-dev` 26.04 是 `2.61.3+dfsg-3`（main）✅

**Rust / Node**：

```bash
# Rust：用 rustup 固定版本，别用发行版包
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add clippy rustfmt
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
cargo install cargo-ndk --locked

# Node：用 nvm / NodeSource 装 Node 22 LTS（项目用 npm + 已提交的 package-lock.json）
```

**Android（与 Windows 同一套，Linux 反而更顺）**：§1.8 的组件列表不变。
Linux 上 Tauri 不需要 Windows 那个符号链接权限 → **不会遇到 §1.8 的卡点**。
`ANDROID_HOME` / `NDK_HOME` 写进 `~/.bashrc`；注意 GUI 应用不继承 shell dotfiles
（Tauri 官方用 `fix-path-env-rs` 处理，见其部署文档）。

**CI 联动（重要，会自动发生）**：

- `ubuntu-latest` **目前仍指向 24.04**；runner 镜像公告写的是
  “`ubuntu-latest` label will use Ubuntu 26.04 in **November 2026**”。
  → 迁移前把 CI 的 `runs-on: ubuntu-latest` **固定成 `ubuntu-24.04`**，
  或先加一个 `ubuntu-26.04` 的 job 试跑再切换。
- 26.04 runner 镜像已 GA（`ubuntu26/20261018.168`，OS 26.04.1 LTS），可直接用 `runs-on: ubuntu-26.04`。
- **glibc 兼容下限**：Tauri 官方要求“用你要支持的最老系统构建”。
  在 26.04（glibc 2.43）编出的 .deb 装到 22.04/24.04 会报
  `GLIBC_2.xx not found`。→ 要发 .deb 就用 24.04 或容器构建。
  （ROADMAP 把 Linux 列为可选 P5-L，**在用户明确提出需求前不投入**。）

**WSL2 预演（Windows 本机可用）**：本机 WSL 已就绪（WSL 3.0.1.0 / 内核 6.18.40.1 /
WSLg 1.0.79），`wsl --list --online` 里已有 `Ubuntu-26.04`：

```bash
wsl --install -d Ubuntu-26.04 --no-launch
```

但 **WSL ≠ 真机**：WebKitGTK 沙箱、通知、托盘、窗口行为的差异必须真机验证
（[SECURITY §3](SECURITY.md) 要求在目标 WebView 上实测网络请求计数与 IPC 隔离）。

---

## 3. 写脚本 / CI 前的检查清单

- [ ] 命令在 **PowerShell** 下能跑吗？还是要 `shell: bash`？
- [ ] 用到的命令（`grep` / `sed -i` / `timeout`）在目标 shell 存在吗？
- [ ] 引号能扛住外层 shell 再解释一层吗？复杂逻辑改用脚本文件
- [ ] 需要访问的网络目标在**目标环境**可达吗（代理 / DNS / 端口）？
- [ ] 要访问 GitHub 吗？**先探明连通性再操作**，别直接 push（见 §1.5.1）
- [ ] 依赖的本地工具装了吗（`filter-repo` / JDK 17 / SDK / `cargo-audit`）？
- [ ] 文档相对路径的层级对吗（见 §1.7）？
- [ ] 平台差异是否影响这条命令/这段代码（见 §2.3）？

## 4. 本文档的维护

- 每次遇到新的环境坑 → 补进 §1，并在 CHANGELOG 记录
- 每次实测新的平台能力 → 更新 §2 的状态表（区分「可编译」与「可运行」）
- 推断性的内容一律标注「未验证」，不写成结论

相关：[架构](ARCHITECTURE.md) · [安全](SECURITY.md) · [测试策略](TESTING_STRATEGY.md) · [AI 规范](AI_DEVELOPMENT_GUIDE.md)
