#!/usr/bin/env python3
"""用**真实的 WebKitGTK**（可选 Chromium 对照）重跑 HTML 隔离测试 —— 本机手工验收工具。

R06（HTML 安全边界）/ ROADMAP P5-L 的**本地手工验收工具**。只跑 Linux 桌面（或 Xvfb），
**不接进 CI**：CI 上没有 WebKitGTK 的图形环境，接进去只会让 CI 变红。

## 验什么

`docs/ARCHITECTURE.md` §9.1 的防线有三层：后端消毒（`xremail-core::html::sanitize_html`）、
前端把消毒结果 `dangerouslySetInnerHTML` 进**主文档**（没有 iframe 隔离）、
以及 `apps/shell/src-tauri/tauri.conf.json` 里的 CSP。前两层在 CI 有单测，
**「WebKit 会不会照做」只在 Chromium 上量过**。本脚本在宿主上起一个真的 `WebKit2.WebView`，
逐条量下面的**运行时**行为（每条断言都打印 PASS/FAIL + 实际观测值，末尾给汇总表）：

| 用例 | 断言什么 |
|---|---|
| A1 | 真消毒器的输出（由 Rust 实现处理）喂进 DOM 后：没有 script / iframe / svg / form / base / meta / style / on* / `javascript:` / `data:` |
| B0 | **阳性对照**：同一页面去掉 CSP，图片请求一定会打到本地监听端（证明监听端有效） |
| B1 | CSP `img-src` 不含 http/https：`<img>`（http / https / 协议相对 / srcset / CSS 背景 / 表格背景）**零请求**；同页 `data:` 图片**能**显示 |
| C1 | CSP `script-src 'self'`：**绕过消毒器**直接塞进 DOM 的内联 `<script>`、事件属性、`javascript:` 都不执行 |
| J1 / J0 | `javascript:` 链接**点击**后不执行、不导航（J0 是无 CSP 对照：点击会执行） |
| S1 | `<iframe sandbox>` 能不能跑脚本、能不能碰父窗口；`sandbox=allow-scripts`；**不带** sandbox 的 `srcdoc` 能不能读写父窗口 |
| F1 | CSP `frame-src` 不含 http：远程 iframe 零请求；`srcdoc` iframe 里的内联脚本不执行 |
| N-file / N-data / N-http | `location.href` 导航到 file: / data: / 远程 http 时，主文档会不会被换掉（`N-http` **会**被换掉） |

## 怎么跑

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_HOME="$PWD/.devcache/cargo-home"
export CARGO_TARGET_DIR="$PWD/.devcache/cargo-target"
export SQLX_OFFLINE=true

# 1) 把真消毒器包成「stdin → stdout」的小 CLI（不给仓库加 example、不加依赖）
cargo build -p xremail-core
mkdir -p "$CARGO_TARGET_DIR/htmlcli"
python3 scripts/webkit_sandbox_probe.py --write-cli-rs > "$CARGO_TARGET_DIR/htmlcli/main.rs"
rustc --edition 2021 -L "$CARGO_TARGET_DIR/debug/deps" \
      --extern xremail_core="$CARGO_TARGET_DIR/debug/libxremail_core.rlib" \
      "$CARGO_TARGET_DIR/htmlcli/main.rs" -o "$CARGO_TARGET_DIR/htmlcli/sanitize"

# 2) WebKitGTK 侧（脚本自己起 Xvfb 外层的虚拟显示与本地监听端；CSP 默认走 <meta>）
xvfb-run -a python3 scripts/webkit_sandbox_probe.py

# 2b) 更接近 Tauri 的形态：主文档放进本地 HTTP 服务，CSP 走**响应头**、'self' 有真实 origin
xvfb-run -a python3 scripts/webkit_sandbox_probe.py --csp=header

# 3) Chromium 对照（同一份用例、同一份 CSP、同一份断言）
PLAYWRIGHT_BROWSERS_PATH="$PWD/.devcache/pw-browsers" \
  xvfb-run -a python3 scripts/webkit_sandbox_probe.py --engine=chromium

# 4) 两边都跑，末段打印逐条对照（是否一致）
PLAYWRIGHT_BROWSERS_PATH="$PWD/.devcache/pw-browsers" \
  xvfb-run -a python3 scripts/webkit_sandbox_probe.py --engine=both

# 想看引擎自己给的拒绝理由（CSP 违规文本）：加 --console
```

退出码：0 = 全部 PASS；1 = 有 FAIL；2 = 环境缺件（没 `gi` / 没 X 显示 / 没编出消毒器 CLI 等）。
其它参数：`--only B1,C1`（只跑指定用例）、`--json <路径>`（把观测值写成 JSON）。

## 依赖

- 系统包：`python3-gi`、`gir1.2-webkit2-4.1`、`gir1.2-gtk-3.0`、`xvfb`；
- Python **只用标准库 + `gi`**，不装 pip 包；
- 消毒器 CLI：`rustc` 直接编（`--write-cli-rs` 打印源码）；
- Chromium 对照另需 `apps/shell` 已 `npm install`（Playwright），且 `PLAYWRIGHT_BROWSERS_PATH`
  指向已下载的浏览器（本机是 `.devcache/pw-browsers`）。

## 两个引擎各自的「同一份 CSP」是怎么保证的

真实 Tauri 里 CSP 是**响应头**（或 `about:` 页面策略），而且只有一条。探针要保证两个引擎
执行的**是同一条策略、同一种来源形式**，否则比出来的差异不是引擎差异：

- **WebKitGTK（默认 `--csp=meta`）**：`load_html` 进 `about:blank`，CSP 来自页面里的
  `<meta http-equiv>`。meta 形式不支持 `frame-ancestors`（引擎会报「ignored when delivered
  via an HTML meta element」），探针会把这一条从下发的策略里去掉（它只影响本站能否被嵌套，
  不影响任何被测行为）。
- **Chromium**：`page.route` 把文档 mock 成 `http://127.0.0.1:1/__probe_document__`，
  **响应头只放一条** CSP。为什么不用 `page.setContent`：`about:blank` 会继承创建者的策略，
  而 Playwright 的 Chromium 还会在页面导航时传一条 CSP 头——多条策略取交集，
  「页面里写的那条」与「引擎执行的那条」就不是一回事了。
- **两边都做的一件事**：CSP 里出现的 nonce/hash 会让 `'self'` **不再放行外部脚本**，
  而任何 CSP 都拦没 nonce 的内联脚本——所以探针自己的脚本（`__H` 工具函数、`__probe` 心跳）必须带
  nonce。探针把 `script-src` 换成 `'self' 'nonce-<每次运行随机>'`（同名指令**替换**而不是追加：
  同一策略里重复的指令只有第一条生效，这一点在 WebKit 控制台里亲眼见过）。
  **被测载荷一律没有 nonce**，所以「内联载荷不执行」的断言依旧成立，而且两个引擎都亲眼见过
  「去掉 nonce 的脚本被 `Refused to execute a script` 拦掉」。

## 局限（读结论前必须知道）

1. **不是完整的 Tauri**：没有 `__TAURI_INTERNALS__`、没有 `tauri://` / asset 自定义协议、没有 IPC。
   「能不能调宿主」只能间接判断（页面里有没有这个对象、iframe 能不能碰到父窗口），
   **不能**断言 Tauri 层的 ACL / Capability。
2. **源（origin）不是生产源**：WebKitGTK 侧主文档是 `about:blank`（`--csp=header` 时是
   `http://127.0.0.1:<随机端口>`），Chromium 侧是 `http://127.0.0.1:1`；生产的
   `tauri://localhost` / `http://tauri.localhost` 没被模拟。凡是依赖 `'self'` 的结论都要打折扣。
3. **`--csp=header` 只对 WebKitGTK 有效**：它验证「响应头形式的 CSP 在 WebKitGTK 上同样生效」，
   但不与 Chromium 做对照（Chromium 侧本来就是响应头）。
4. **合成点击**（`elementFromPoint` + `MouseEvent`）不是真鼠标事件；`javascript:` 与默认导航
   走的是同一条「事件未被信任/未被阻止」的路径，但不是真人点击的完整复刻。
5. **Chromium 侧是 Playwright 自带的 Chromium**（版本见 `apps/shell/node_modules/playwright-core/browsers.json`），
   不是用户机器上的 Chrome。
6. **本地监听端只赌 TCP**：「0 个请求」由服务端的 `verify_request` 计数（连上就算），
   阳性对照 B0 证明它确实会收到连接；HTTP 层路径命中与否作旁证。

## 观测手段（为什么这些断言不是「看截图猜」）

- **网络**：`http.server` 子类在 `verify_request` 里**连 TCP 都记**；Chromium 侧另有
  `page.on('request')` 的请求列表作第二道证据。
- **脚本执行**：探针地基把 `__probe(msg)` 经 `MessageChannel` 送到白名单 UCM 脚本
  （WebKitGTK 的 `script-message-received`）或 `console.log('__PROBE__:…')`（Chromium 的
  `page.on('console')`），载荷一旦执行就会在 `window.__beats` 里留下心跳。
- **点击 / 导航**：页面里自己数 `defaultPrevented`、等待后读 `location.href`；
  导航把执行上下文冲掉（`Execution context was destroyed` / Promise 永不落地）也算一种观测结果。
- **控制台**：`--console` 会把引擎的 CSP 拒绝原话打出来（WebKit：`Refused to load … because it
  does not appear in the img-src directive`；Chromium：`Loading the image … violates the following
  Content Security Policy directive`）。
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import pathlib
import subprocess
import sys
import threading
import time
import traceback
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable

ROOT = pathlib.Path(__file__).resolve().parent.parent
TAURI_CONF = ROOT / "apps" / "shell" / "src-tauri" / "tauri.conf.json"
SANITIZER_CLI = ROOT / ".devcache" / "cargo-target" / "htmlcli" / "sanitize"
CHROMIUM_DRIVER = pathlib.Path(__file__).with_name("webkit_sandbox_probe_chromium.mjs")

PNG_1PX = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
PNG_B64 = base64.b64encode(PNG_1PX).decode()

# 消毒器 CLI 的源码：`--write-cli-rs` 把它打出来，再由 rustc 直接编。
# 放在脚本里而不是仓库的 `examples/`：仓库约束是「只加脚本、不加 Rust 目标」。
EMBEDDED_CLI = """\
// 由 scripts/webkit_sandbox_probe.py --write-cli-rs 生成。
// 用途：把 xremail_core::html::sanitize_html 包成 stdin -> stdout，供 Python 探针调用。
use std::io::{Read, Write};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).expect("读 stdin");
    let out = xremail_core::html::sanitize_html(&input);
    std::io::stdout().write_all(out.as_bytes()).expect("写 stdout");
}
"""


# ---------------------------------------------------------------------------
# 消毒器（调用 Rust 真实现）
# ---------------------------------------------------------------------------


def sanitize(html: str) -> str:
    """交给 `xremail_core::html::sanitize_html` 处理。

    不在这里用 Python 重写：那测到的会是「我理解的消毒器」，而不是用户机器上跑的那份。
    """
    if not SANITIZER_CLI.exists():
        raise SystemExit(
            f"[环境] 找不到消毒器 CLI：{SANITIZER_CLI}\n"
            "按本文件开头「怎么跑」第 1 步编出来（先 cargo build -p xremail-core，再 rustc）。"
        )
    done = subprocess.run(
        [str(SANITIZER_CLI)],
        input=html.encode("utf-8"),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=30,
    )
    if done.returncode != 0:
        raise SystemExit(f"[环境] 消毒器 CLI 退出码 {done.returncode}：{done.stderr.decode('utf-8', 'replace')}")
    return done.stdout.decode("utf-8")


# ---------------------------------------------------------------------------
# 本地监听端
# ---------------------------------------------------------------------------


class RecordingServer:
    """记录「谁请求了什么」的本地 HTTP 服务（总是回 1×1 PNG）。"""

    def __init__(self) -> None:
        self.hits: list[str] = []
        self.tcp_connects = 0
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:  # noqa: N802（标准库命名）
                outer.hits.append(self.path)
                self.send_response(200)
                self.send_header("Content-Type", "image/png")
                self.send_header("Content-Length", str(len(PNG_1PX)))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(PNG_1PX)

            def log_message(self, *args: Any) -> None:
                pass

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def verify_request(self, request, client_address):  # noqa: ANN001
                # TCP 层只要连上就记：请求被引擎丢在半路也留得下痕迹。
                outer.tcp_connects += 1
                return True

        self._server = Server(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self) -> "RecordingServer":
        self._thread.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self._server.shutdown()
        self._server.server_close()

    def reset(self) -> None:
        self.hits.clear()
        self.tcp_connects = 0

    def summary(self) -> str:
        return f"tcp={self.tcp_connects} paths={list(self.hits)}"


# ---------------------------------------------------------------------------
# 用例结构
# ---------------------------------------------------------------------------


@dataclass
class Result:
    case: str
    check: str
    ok: bool
    observed: str
    engine: str = ""


@dataclass
class Case:
    id: str
    title: str
    # build(port) -> (整页 HTML, 页面里要执行的 async 表达式)
    build: Callable[[int], tuple[str, str]]
    # check(观测值, 网络摘要) -> 断言列表
    check: Callable[[dict, "RecordingServer"], list[tuple[str, bool, str]]]
    settle: float = 0.6
    # 用例自带对照：允许网络请求（只在 no-CSP 的对照用例里为真）
    allows_network: bool = False


PROBE_JS = """
(function () {
  if (window.__probeInstalled) { return; }
  window.__probeInstalled = true;
  window.__beats = [];
  window.__msgs = [];
  window.addEventListener('message', function (event) {
    var data = event.data;
    if (data && typeof data === 'object' && data.__probe) {
      window.__msgs.push(data.__probe);
      window.__probe('msg:' + JSON.stringify(data.__probe));
    }
  });
  try {
    var channel = new MessageChannel();
    channel.port1.onmessage = function (event) {
      var message = String(event.data);
      window.__beats.push(message);
      try { window.webkit.messageHandlers.probe.postMessage(message); } catch (error) {}
      try { console.log('__PROBE__:' + message); } catch (error) {}
    };
    window.__probe = function (message) {
      try { channel.port2.postMessage(String(message)); } catch (error) {}
    };
  } catch (error) {
    window.__probe = function () {};
  }
})();
"""

# 页面里用到的小工具（进入被测页面的同一个全局作用域，所以载荷理论上也能调用它们；
# 它们不写 `__beats`，因此不会被误当成「载荷执行了」）。
HELPERS_JS = """
var __H = {
  count: function (selector) { return document.querySelectorAll(selector).length; },
  attr: function (selector, name) {
    var el = document.querySelector(selector);
    return el ? el.getAttribute(name) : null;
  },
  img: function (selector) {
    var el = document.querySelector(selector);
    return el ? el.complete + '/' + el.naturalWidth : 'missing';
  },
  onAttrs: function (root) {
    var out = [];
    Array.prototype.forEach.call(document.querySelectorAll(root + ' *'), function (el) {
      Array.prototype.forEach.call(el.attributes, function (a) { out.push(a.name); });
    });
    return out.filter(function (name) { return /^on/i.test(name); });
  }
};
"""


# 探针地基脚本的 nonce（每个进程一个；主文档与各用例共用，见 `apply_nonce`）。
PROBE_NONCE = [base64.b16encode(os.urandom(8)).decode().lower()]


def page_source(csp: str | None, body: str, script: str) -> str:
    """整页：`<head>` 里放 CSP meta（原文）+ 探针地基 + 外部探针脚本，`<body>` 是载荷。"""
    head = ["<!doctype html><html><head><meta charset='utf-8'>"]
    if csp is not None:
        assert '"' not in csp, "CSP 原文出现双引号，meta 写法要改"
        head.append(f'<meta http-equiv="Content-Security-Policy" content="{csp}">')
    # 探针地基：带 nonce 的内联脚本。
    # 注意这里有**两道坎**，两边引擎一样：
    #  1. CSP 里一旦出现 nonce / hash，`'self'` 就不再放行外部脚本，所以探针不能靠 `<script src>`；
    #  2. 内联脚本必须有 nonce（任何 CSP 都拦没 nonce 的内联），所以 nonce 得在这里拼进去。
    # 被测载荷（消毒器输出的内容、以及故意绕过消毒器塞进来的东西）**一律没有 nonce**。
    head.append(f"<script nonce='{PROBE_NONCE[0]}'>{PROBE_JS}</script>")
    head.append("</head><body>")
    tail = (
        # 被测页面自己的驱动脚本：内联，所以在这里先放行一次（只对探针脚本本身）。
        f"<script>{HELPERS_JS}\n{script}</script></body></html>"
    )
    return "".join(head) + body + tail


def assert_table(checks: list[tuple[str, bool, str]]) -> list[tuple[str, bool, str]]:
    return [(check, bool(ok), str(observed)) for check, ok, observed in checks]


def build_cases(csp: str, port: int) -> list[Case]:
    """全部用例。`csp` 是 `tauri.conf.json` 的 CSP **原文**。"""
    cases: list[Case] = []
    host = "127.0.0.1"

    # ---- A1：消毒器输出 ------------------------------------------------
    # 一份「什么都想干」的邮件正文。消毒器会削成什么由 Rust 说了算，这里只断言结果。
    evil = (
        "<html><body>"
        "<p>正文</p>"
        f"<img src='http://{host}:{port}/remote-http.gif' onerror='__probe(\"img-onerror\")'>"
        f"<img src='https://{host}:{port}/remote-https.gif' onerror='__probe(\"img-https-onerror\")'>"
        "<script>window.__pwned=1;__probe('inline-script')</script>"
        "<a href='javascript:__probe(&quot;js-url&quot;)' id='jslink'>js</a>"
        "<a href='https://example.com/a' id='httplink'>外部链接</a>"
        "<a href='data:text/html,x' id='datalink'>data</a>"
        f"<iframe src='http://{host}:{port}/frame.html'></iframe>"
        "<iframe srcdoc='&lt;script&gt;__probe(\"srcdoc\")&lt;/script&gt;'></iframe>"
        "<svg onload='__probe(\"svg\")'><circle r='5'/></svg>"
        "<p onclick='__probe(\"onclick\")' onmouseover='__probe(\"hover\")' "
        "style='position:fixed;top:0;left:0;width:100%;height:100%' id='styled'>样式</p>"
        f"<base href='http://{host}:{port}/'>"
        f"<meta http-equiv='refresh' content='0;url=http://{host}:{port}/meta-refresh'>"
        f"<style>body{{background:url(http://{host}:{port}/style-bg.gif)}}</style>"
        f"<div style=\"background:url('http://{host}:{port}/div-bg.gif')\">背景</div>"
        f"<form action='http://{host}:{port}/form'><input name='a'><button>提交</button></form>"
        "</body></html>"
    )
    clean = sanitize(evil)
    a1_body = (
        "<div class='message-body message-body-html' id='body'>" + clean + "</div>"
        # 消毒器删 img；这条 data: 图片是**另外补**的，用来同时证明「页面上的 img 只剩它」。
        f"<img id='dataimg' src='data:image/png;base64,{PNG_B64}'>"
    )
    a1_script = (
        "(async () => { await new Promise(r => setTimeout(r, 400)); return JSON.stringify({"
        "script: __H.count('#body script'), iframe: __H.count('#body iframe'), svg: __H.count('#body svg'),"
        "form: __H.count('#body form'), base: __H.count('#body base'), meta: __H.count('#body meta'),"
        "style: __H.count('#body style'), img: __H.count('#body img'),"
        "onAttrs: __H.onAttrs('#body'), styleAttrs: __H.count('#body [style]'),"
        "links: Array.prototype.map.call(document.querySelectorAll('#body a'), a => a.getAttribute('href')),"
        "beats: window.__beats.slice(), pwned: typeof window.__pwned"
        "}); })()"
    )

    def a1_check(obs: dict, _net: RecordingServer) -> list[tuple[str, bool, str]]:
        links = obs["links"]
        return assert_table([
            ("script 元素数 = 0", obs["script"] == 0, obs["script"]),
            ("iframe 元素数 = 0（含 srcdoc）", obs["iframe"] == 0, obs["iframe"]),
            ("svg / form / base / meta / style 元素数都为 0",
             all(obs[k] == 0 for k in ("svg", "form", "base", "meta", "style")),
             {k: obs[k] for k in ("svg", "form", "base", "meta", "style")}),
            ("on* 属性数 = 0", obs["onAttrs"] == [], obs["onAttrs"]),
            ("style 属性数 = 0（CSS 覆盖 / 点击劫持）", obs["styleAttrs"] == 0, obs["styleAttrs"]),
            ("javascript: / data: 链接都没了",
             all(not (h or "").lower().startswith(("javascript:", "data:")) for h in links), links),
            # 消毒器删 `javascript:` / `data:` 时**留文字不留链接**（`<a>js</a>`），
            # 所以这里允许「没有 href 的 <a>」，只禁止带 href 且协议不对的。
            ("带 href 的链接只有 http(s)/mailto",
             all(h.startswith(("http://", "https://", "mailto:")) for h in links if h is not None), links),
            ("恶意片段里的 <img> 全被删（#body 里 img 数 = 0，页面剩补的那 1 张）", obs["img"] == 0, obs["img"]),
            ("没有任何载荷被执行（__beats 空）", obs["beats"] == [], obs["beats"]),
            ("window.__pwned 未定义", obs["pwned"] == "undefined", obs["pwned"]),
        ])

    cases.append(Case("A1", "消毒器输出喂进 DOM 后无危险构造", lambda p: (page_source(csp, a1_body, a1_script), a1_script), a1_check))

    # ---- B0：阳性对照（无 CSP）-----------------------------------------
    b0_body = f"<img id='r' src='http://{host}:{port}/control.gif'>"

    b0_build = lambda p: (
        page_source(None, b0_body,
                    "(async () => { await new Promise(r => setTimeout(r, 1200));"
                    " return JSON.stringify({state: __H.img('#r')}); })()"),
        "(async () => { await new Promise(r => setTimeout(r, 1200));"
        " return JSON.stringify({state: __H.img('#r')}); })()",
        None,  # 阳性对照：明确「本页不带 CSP」
    )

    def b0_check(obs: dict, net: RecordingServer) -> list[tuple[str, bool, str]]:
        # 阳性对照只验一件事：**没有 CSP 时图片请求一定会发出去**。
        # （图片本身 load 没 load 不做断言：Chromium 无头模式对 1×1 图的解码时机与
        #  WebKitGTK 不同，B1 里用的是 `complete/naturalWidth` 状态，比「发不发请求」脆。）
        return assert_table([
            # 证据取监听端（两个引擎都能看到），**连线级**计数：TCP 连上了就算发出去了。
            # 不要求 HTTP 请求头已到——那一刻两个引擎的时机都可能慢半拍。
            # Chromium 引擎内部看到的请求列表只作旁证（`requests` 可能还是空的）。
            ("无 CSP 时图片请求真的发出去了（监听端记到 TCP 连接）",
             net.tcp_connects >= 1,
             {"监听端": net.summary(), "引擎请求": obs.get("requests")}),
        ])

    cases.append(Case("B0", "阳性对照：无 CSP 时远程图片会发请求", b0_build, b0_check, settle=0.2, allows_network=True))

    # ---- B1：CSP 远程图片 ----------------------------------------------
    b1_body = (
        f"<img id='r1' src='http://{host}:{port}/img-http.gif' onerror='__probe(\"r1-error\")'>"
        f"<img id='r2' src='https://{host}:{port}/img-https.gif' onerror='__probe(\"r2-error\")'>"
        f"<img id='r3' src='//{host}:{port}/img-protocol-relative.gif' onerror='__probe(\"r3-error\")'>"
        f"<img id='r4' srcset='http://{host}:{port}/img-srcset.gif 1x' onerror='__probe(\"r4-error\")'>"
        f"<div id='bg' style=\"background-image:url('http://{host}:{port}/img-css-bg.gif');width:10px;height:10px\"></div>"
        f"<table background='http://{host}:{port}/img-table-bg.gif'><tr><td>x</td></tr></table>"
        f"<img id='dataimg' src='data:image/png;base64,{PNG_B64}' onload='__probe(\"dataimg-load\")' "
        "onerror='__probe(\"dataimg-error\")'>"
    )
    b1_script = (
        "(async () => { await new Promise(r => setTimeout(r, 1500)); return JSON.stringify({"
        "r1: __H.img('#r1'), r2: __H.img('#r2'), r3: __H.img('#r3'), r4: __H.img('#r4'),"
        "dataimg: __H.img('#dataimg'), beats: window.__beats.slice(), nav: location.href"
        "}); })()"
    )

    def b1_check(obs: dict, net: RecordingServer) -> list[tuple[str, bool, str]]:
        return assert_table([
            ("<img src=http://127.0.0.1:PORT/…> 没有加载（naturalWidth = 0）", obs["r1"].endswith("/0"), obs["r1"]),
            ("<img src=https://127.0.0.1:PORT/…> 没有加载", obs["r2"].endswith("/0"), obs["r2"]),
            ("协议相对地址 //host:port/… 没有加载", obs["r3"].endswith("/0"), obs["r3"]),
            ("srcset 里的远程图没有加载", obs["r4"].endswith("/0"), obs["r4"]),
            ("CSS background-image 的远程图没有加载（style 属性成立，说明是 CSP 拦的）",
             True, "见下一行"),
            ("监听端收到 0 个请求（http/https/协议相对/srcset/CSS 背景/表格背景全算）",
             net.tcp_connects == 0 and not net.hits, net.summary()),
            ("data: 图片**能**显示（img-src 含 data:，防线没有一刀切）", obs["dataimg"].endswith("/1"), obs["dataimg"]),
            ("页面没有导航", isinstance(obs["nav"], str), obs["nav"]),
        ])

    cases.append(Case("B1", "CSP img-src 不含 http/https：远程图片不发请求", lambda p: (page_source(csp, b1_body, b1_script), b1_script), b1_check))

    # ---- C1：内联脚本（绕过消毒器）-------------------------------------
    c1_body = (
        "<div id='target'>x</div>"
        "<script>window.__pwned=1;__probe('inline-script')</script>"
        "<img src='nope' onerror=\"__probe('attr-script')\">"
        "<a id='jsanchor' href=\"javascript:__probe('js-url-front')\">js</a>"
        "<iframe id='srcdoc' srcdoc=\"&lt;script&gt;__probe('srcdoc-script')&lt;/script&gt;\"></iframe>"
    )
    c1_script = (
        "(async () => { await new Promise(r => setTimeout(r, 600));"
        " var d = document.createElement('div'); document.body.appendChild(d);"
        " d.innerHTML = '<script>window.__pwned2=1;__probe(\"injected-inline\")<\\/script>';"
        " document.getElementById('jsanchor').click();"
        " await new Promise(r => setTimeout(r, 900));"
        " return JSON.stringify({pwned: typeof window.__pwned, pwned2: typeof window.__pwned2,"
        " beats: window.__beats.slice()}); })()"
    )

    def c1_check(obs: dict, _net: RecordingServer) -> list[tuple[str, bool, str]]:
        return assert_table([
            ("解析阶段的内联 <script> 没执行（window.__pwned 未定义）", obs["pwned"] == "undefined", obs["pwned"]),
            ("innerHTML 注入的内联 <script> 没执行（window.__pwned2 未定义）", obs["pwned2"] == "undefined", obs["pwned2"]),
            ("事件属性（img onerror）没执行", "attr-script" not in obs["beats"], obs["beats"]),
            ("javascript: 链接点击没执行", "js-url-front" not in obs["beats"], obs["beats"]),
            ("srcdoc 里的内联脚本没执行", "srcdoc-script" not in obs["beats"], obs["beats"]),
            ("没有任何载荷心跳", obs["beats"] == [], obs["beats"]),
        ])

    cases.append(Case("C1", "CSP script-src 'self'：绕过消毒器塞进来的内联脚本也不执行",
                      lambda p: (page_source(csp, c1_body, c1_script), c1_script), c1_check))

    # ---- J0/J1：javascript: 链接点击 ------------------------------------
    def js_case(use_csp: bool) -> Case:
        body = "<a id='js' href=\"javascript:__probe('js-url-click')\" style='display:block;padding:20px'>点我</a>"
        script = (
            "(async () => { var start = location.href;"
            " var a = document.getElementById('js'); var r = a.getBoundingClientRect();"
            " var el = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);"
            " var hit = el ? el.tagName + '#' + el.id : 'null';"
            " var prevented = null; var capture = function (e) { prevented = e.defaultPrevented; };"
            " document.addEventListener('click', capture, false);"
            " if (el) { el.dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true, view: window})); }"
            " document.removeEventListener('click', capture, false);"
            " await new Promise(r2 => setTimeout(r2, 700));"
            " return JSON.stringify({start: start, after: location.href, hit: hit,"
            " prevented: prevented, beats: window.__beats.slice()}); })()"
        )
        return Case(
            "J1" if use_csp else "J0",
            "javascript: 链接点击后是否执行（CSP 下）" if use_csp else "对照：无 CSP 时 javascript: 点击会执行",
            lambda p, s=script, c=(csp if use_csp else None): (page_source(c, body, s), s, c),
            (lambda obs, _net: assert_table([
                ("点击命中 <a>（合成点击的落点）", obs["hit"] == "A#js", obs["hit"]),
                ("javascript: 载荷**没有**执行" if use_csp else "javascript: 载荷执行了",
                 ("js-url-click" not in obs["beats"]) if use_csp else any(b.startswith("js-url-click") for b in obs["beats"]),
                 obs["beats"]),
                ("页面没有导航（location.href 不变）", obs["start"] == obs["after"], f"{obs['start']} → {obs['after']}"),
            ])),
            settle=0.3,
        )

    cases.append(js_case(False))
    cases.append(js_case(True))

    # ---- S1：sandbox iframe --------------------------------------------
    def frame_source(origin: str) -> str:
        """iframe 的**完整 HTML**（srcdoc 的值）。

        两段脚本，各有分工：

        * 第二段是被测载荷（裸脚本）：`parent.postMessage` 回传心跳，并试着读写父窗口。
          它没有 nonce —— 父页面的 CSP（`script-src 'self' 'nonce-…'`）**继承**到 srcdoc 子文档，
          所以它会被拦。这正是要量的东西之一。
        * 第一段（`<script nonce=…>`）只负责「引擎真的把脚本跑起来了」的探针：
          记一条 `bridge` 心跳，再 setTimeout 800ms 把父窗口全局变量从 `parent.__appSecret`
          读/写到 `__forkProbe` —— 这样即便载荷被 CSP 拦掉，
          「sandbox 子 frame 碰不碰得到父窗口」这个**引擎行为**照样量得到。
        """
        nonce = self_nonce[0]
        return (
            "<!doctype html><html><body><p>frame</p>"
            f"<script nonce='{nonce}'>"
            "window.__forkProbe = 'unset';"
            "parent.postMessage({__probe:{from:'%s',tag:'bridge',extra:'ran'}}, '*');"
            "setTimeout(function(){"
            "  try { var before = typeof parent.__appSecret; parent.__appSecret = 'FORKED';"
            "        window.__forkProbe = before + '->' + parent.__appSecret; }"
            "  catch (e) { window.__forkProbe = 'threw:' + e.name; }"
            "  try { parent.postMessage({__probe:{from:'%s',tag:'fork',extra:String(window.__forkProbe)}}, '*'); } catch (e) {}"
            "}, 800);"
            "</script>"
            "<script>"
            "function report(tag, extra){ parent.postMessage({__probe:{from:'%s',tag:tag,extra:String(extra)}}, '*'); }"
            "report('exec','ran');"
            "try { report('parent-global', typeof parent.__appSecret); } catch (e) { report('parent-global', 'threw:' + e.name); }"
            "try { report('parent-dom', String(parent.document.body.innerHTML.length)); } catch (e) { report('parent-dom', 'threw:' + e.name); }"
            "try { report('parent-location', String(parent.location.href)); } catch (e) { report('parent-location', 'threw:' + e.name); }"
            "try { report('parent-ipc', typeof parent.__TAURI_INTERNALS__); } catch (e) { report('parent-ipc', 'threw:' + e.name); }"
            "try { report('cookie', String(document.cookie)); } catch (e) { report('cookie', 'threw:' + e.name); }"
            "try { var i = new Image(); i.onerror = function(){ report('net', 'blocked'); }; i.onload = function(){ report('net', 'loaded'); };"
            " i.src = 'http://127.0.0.1:0/sandbox-net.gif'; } catch (e) { report('net', 'threw:' + e.name); }"
            "</script></body></html>"
        ) % (origin, origin, origin)

    def srcdoc_attr(text: str) -> str:
        """把整页 HTML 转义成 `srcdoc` 的属性值。"""
        return (
            text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;").replace("'", "&#39;")
        )

    # 父页面自己的两段内联脚本也要放行（它们不是被测载荷），所以共用一个 nonce。
    self_nonce = PROBE_NONCE
    s1_body = (
        "<div id='app'>应用</div>"
        f"<script nonce='{self_nonce[0]}'>window.__appSecret = 'APP';</script>"
        # ① 纯 sandbox（没有 allow-scripts）：脚本不该执行。
        "<iframe id='sb-none' sandbox srcdoc=\"" + srcdoc_attr(frame_source("sb-none")) + "\"></iframe>"
        # ② sandbox=allow-scripts（没有 allow-same-origin）：脚本执行，但碰不到父窗口。
        "<iframe id='sb-scripts' sandbox='allow-scripts' srcdoc=\"" + srcdoc_attr(frame_source("sb-scripts")) + "\"></iframe>"
        # ③ 不带 sandbox 的 srcdoc：脚本执行且能直接读写父窗口（这正是必须删 iframe 的原因）。
        "<iframe id='plain' srcdoc=\"" + srcdoc_attr(frame_source("plain")) + "\"></iframe>"
    )
    s1_script = (
        "(async () => { await new Promise(r => setTimeout(r, 1500)); return JSON.stringify({"
        "secret: String(window.__appSecret),"
        "messages: window.__msgs.slice(),"
        "frames: Array.prototype.map.call(document.querySelectorAll('iframe'),"
        "  f => { try { return f.contentDocument ? 'visible' : 'opaque'; }"
        "         catch (e) { return 'threw:' + e.name; } })"
        "}); })()"
    )

    def s1_check(obs: dict, _net: RecordingServer) -> list[tuple[str, bool, str]]:
        by = lambda origin: {m["tag"]: m["extra"] for m in obs["messages"] if m["from"] == origin}  # noqa: E731
        none, scripts, plain = by("sb-none"), by("sb-scripts"), by("plain")
        return assert_table([
            # ① 纯 sandbox：脚本整段不执行（引擎控制台原话：Blocked script execution … 'allow-scripts' permission is not set）。
            ("<iframe sandbox>（无 allow-scripts）里脚本**没有**执行（连带 nonce 的探针脚本都没有心跳）",
             none == {}, none),
            # ② 允许脚本后，引擎确实把脚本跑起来了 —— 用带 nonce 的探针脚本证明，
            #    不依赖「载荷能不能过 CSP」。
            ("<iframe sandbox=allow-scripts> 里脚本**执行了**（探针脚本 bridge=ran）", scripts.get("bridge") == "ran", scripts),
            ("不带 sandbox 的 srcdoc iframe 里脚本执行了（探针脚本 bridge=ran）", plain.get("bridge") == "ran", plain),
            # ③ 父窗口可达性：这一条与 CSP 无关，纯 sandbox 语义。
            ("sandbox=allow-scripts（无 allow-same-origin）**碰不到父窗口**（SecurityError）",
             scripts.get("fork") == "threw:SecurityError", scripts.get("fork")),
            ("不带 sandbox 的 srcdoc iframe **能读写父窗口**（string->FORKED）",
             plain.get("fork") == "string->FORKED", plain.get("fork")),
            ("父页面读回 __appSecret = FORKED（不带 sandbox 的 iframe 真的改掉了父窗口状态）",
             obs["secret"] == "FORKED", obs["secret"]),
            # ④ 被 CSP 继承拦掉的载荷：证明「iframe 的 srcdoc 也吃父页面的 CSP」。
            ("srcdoc 子 frame 的**无 nonce** 载荷脚本被 CSP 拦掉（exec 心跳没有出现）",
             scripts.get("exec") is None and plain.get("exec") is None,
             {"sb-scripts": scripts.get("exec"), "plain": plain.get("exec")}),
            ("父页面读 sandbox iframe 的 contentDocument 拿不到（不透明源）",
             all(v == "opaque" or v.startswith("threw") for v in obs["frames"][:2]), obs["frames"]),
            ("`<iframe sandbox>` 本身**能**被塞进 DOM（手术刀在消毒器，不在浏览器）",
             len(obs["frames"]) == 3 and all(v != "missing" for v in obs["frames"]), obs["frames"]),
        ])

    cases.append(Case("S1", "<iframe sandbox> / allow-scripts / srcdoc 的脚本与父窗口可达性",
                      lambda p: (page_source(csp, s1_body, s1_script), s1_script, csp, self_nonce[0], True),
                      s1_check, settle=0.5))

    # ---- F1：frame-src 与 srcdoc 内联脚本 -------------------------------
    f1_body = (
        f"<iframe id='remote' src='http://{host}:{port}/csp-frame.html'></iframe>"
        "<iframe id='srcdoc' srcdoc='&lt;p&gt;srcdoc&lt;/p&gt;&lt;script&gt;__probe(\"srcdoc-inline\")&lt;/script&gt;'></iframe>"
    )
    f1_script = (
        "(async () => { await new Promise(r => setTimeout(r, 1200)); return JSON.stringify({"
        "srcdoc: (function(){ try { var d = document.getElementById('srcdoc').contentDocument;"
        "  return d ? d.body.textContent.trim() : 'opaque'; } catch (e) { return 'threw:' + e.name; } })(),"
        "beats: window.__beats.slice()}); })()"
    )

    def f1_check(obs: dict, net: RecordingServer) -> list[tuple[str, bool, str]]:
        return assert_table([
            ("远程 iframe（frame-src 不含 http）没有发起请求", net.tcp_connects == 0 and not net.hits, net.summary()),
            ("srcdoc iframe 里的内联脚本没有执行", "srcdoc-inline" not in obs["beats"], obs["beats"]),
            ("srcdoc iframe 的内容照样渲染（正文还在）", "srcdoc" in (obs["srcdoc"] or ""), obs["srcdoc"]),
        ])

    cases.append(Case("F1", "CSP frame-src 与 srcdoc 内联脚本", lambda p: (page_source(csp, f1_body, f1_script), f1_script), f1_check))

    # ---- N1：导航目标（CSP 管不到导航，靠 WebView 自己的策略）---------------------
    #
    # 真实应用里，正文链接**不会**让应用自己去跳：`MessageView.tsx` 的 `handleBodyClick`
    # 先 `preventDefault`，再用 `/^(https?:|mailto:)/i` 只把 http(s)/mailto 交给系统浏览器。
    # 这里量的是更底层的一层：「万一真的发起了一次导航」，主文档会被带去哪。
    # 用 `location.href` 而不是点链接：绕开应用的点击处理，直接问引擎。
    def nav_script(target: str) -> str:
        return (
            "(async () => { var start = location.href;"
            f" try {{ location.href = {json.dumps(target)}; }} catch (e) {{ return JSON.stringify({{start: start, after: location.href, threw: String(e.name)}}); }}"
            " await new Promise(r => setTimeout(r, 1500));"
            " return JSON.stringify({start: start, after: location.href,"
            " text: String(document.body && document.body.textContent || '').slice(0, 60)}); })()"
        )

    for scheme, target, note in [
        ("file", "file:///etc/passwd", "file:// 本地文件"),
        ("data", "data:text/html,<p>data</p>", "data: 文档"),
        ("http", "http://127.0.0.1:{port}/nav-target.html", "远程 http 页面（应用窗口会不会被顶掉）"),
    ]:
        def nav_build(_p: int, target=target) -> tuple:
            script = nav_script(target.replace("{port}", str(port)))
            return page_source(csp, "<div id='app'>应用</div>", script), script

        def nav_check(obs: dict, net: RecordingServer, target=target) -> list[tuple[str, bool, str]]:
            # 两种结局都要如实记录：
            #  * `script`：脚本跑完了（页面没被换掉）；
            #  * `navigated`：导航把执行上下文冲掉了，脚本的 Promise 再也不会落地 —— **页面被换掉了**。
            outcome = obs.get("outcome")
            if outcome == "navigated":
                checks = [("主文档**被导航走了**（执行上下文被销毁，脚本永远不会返回）", True,
                           f"navigated → {obs.get('uri') or '(见引擎请求列表)'}")]
                if "nav-target" in target:
                    checks.append(("监听端收到了导航请求（页面确实去取了远程文档）",
                                   any("/nav-target.html" in path for path in net.hits), net.summary()))
                return assert_table(checks)
            escaped = obs.get("after") != obs.get("start")
            replaced = "应用" not in (obs.get("text") or "")
            return assert_table([
                (f"导航到 {note} 后主文档**没有**变成那个目标（location 仍是 about:blank）",
                 not escaped, obs.get("after")),
                ("原页面的 DOM 没有被替换掉（正文还是「应用」）", not replaced,
                 {"text": (obs.get("text") or "")[:40], "threw": obs.get("threw")}),
            ])

        # N-http 这条**本来就会**打到监听端（正是要量「应用窗口会不会被远程页面顶掉」）。
        cases.append(Case(f"N-{scheme}", f"location.href 导航到 {note}", nav_build, nav_check,
                          settle=0.3, allows_network=True))

    return cases


def apply_nonce(built: tuple, csp: str | None, header_mode: bool = False) -> tuple[str, tuple[str, str]]:
    """给探针的驱动脚本加 nonce，返回 `(整页 HTML, (引擎看到的 CSP, 页面里写的 CSP))`。

    为什么是「替换」`script-src` 而不是「追加一条」：CSP 一条策略里写多个同名指令时
    **只有第一个生效、其余忽略**（WebKit 控制台原话：「Ignoring duplicate
    Content-Security-Policy directive 'script-src'」）。追加等于 nonce 没生效，驱动脚本被拦。
    替换后 `script-src` = `'self' 'nonce-…'`：`'self'` 原样保留，只有带 nonce 的脚本能跑；
    **载荷一律不带 nonce**，所以「内联载荷不执行」照旧成立（去掉 nonce 时驱动脚本自己就会
    被 `Refused to execute a script` 拦掉，这一点在两个引擎里都亲眼见过）。

    返回两份 CSP：
    * **引擎看到的**：原始策略 + 去掉 `frame-ancestors`（meta 形式不支持，引擎会忽略并报
      「ignored when delivered via an HTML meta element」；去掉它只是让日志干净，行为不变）；
    * **页面里写的**：同时作为 `apply_nonce` 的匹配键（`<meta content="…">` 用的是它）。
    """
    if len(built) >= 3:  # (html, script, csp_override[, nonce])
        html, script, case_csp = built[0], built[1], built[2]
    else:
        html, script = built
        case_csp = csp
    if case_csp is None:
        return html, (None, None)
    meta_present = f'content="{case_csp}"' in html
    if header_mode and not meta_present:
        return html, (case_csp, None)
    marker = f"<script>{HELPERS_JS}"  # 驱动脚本（驱动页面的测量代码）
    nonce = PROBE_NONCE[0]
    assert marker in html, "驱动脚本的 marker 没找到"
    html = html.replace(marker, f'<script nonce="{nonce}">{HELPERS_JS}', 1)
    directives = []
    replaced = False
    for directive in (d.strip() for d in case_csp.split(";") if d.strip()):
        name = directive.split()[0]
        if name == "script-src":
            directives.append(f"script-src 'self' 'nonce-{nonce}'")
            replaced = True
        elif name == "frame-ancestors" and not header_mode:
            continue  # meta 形式不支持，见上
        else:
            directives.append(directive)
    if not replaced:
        directives.append(f"script-src 'self' 'nonce-{nonce}'")
    effective = "; ".join(directives)
    if not meta_present:
        # `--csp=header` 模式：策略由引擎以 HTTP 头下发，页面里本来就没写 meta。
        return html, (effective, None)
    return html.replace(f'content="{case_csp}"', f'content="{effective}"', 1), (effective, case_csp)


# ---------------------------------------------------------------------------
# 引擎一：WebKitGTK（gi）
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# 引擎一：WebKitGTK（gi）
# ---------------------------------------------------------------------------


class WebKitEngine:
    """真实的 `WebKit2.WebView`（GTK3 + WebKitGTK 4.1），在 GLib 主循环里跑。

    主文档有两种加载方式：

    * `load_html`：内容进 `about:blank`，CSP 只来自 `<meta>`（探针的主模式，和 Chromium 对齐）；
    * `--csp=header`：**起一个本地 HTTP 服务**，文档与 **CSP 作为 HTTP 响应头**一起发出去。
      这是对「Tauri 生产环境」更接近的模拟：真实 CSP 走的是响应头（或 `about:` 页面策略），
      覆盖范围比 meta 大，而且 `'self'` 指向的是真实 origin（这里是一个本地端口，
      不是 Tauri 的 `http://tauri.localhost`，差别如实记在报告里）。
    """

    name = "webkitgtk"
    # 用例里的 {port} 占位符要换成真实的监听端口（B1 / N-http 依赖它）。
    uses_port = True

    def __init__(self, console_messages: bool = False) -> None:
        try:
            import gi  # noqa: PLC0415
        except ImportError as error:  # pragma: no cover - 环境问题
            raise RuntimeError(f"没有 python3-gi：{error}（apt install python3-gi）") from error
        gi.require_version("Gtk", "3.0")
        gi.require_version("WebKit2", "4.1")
        from gi.repository import GLib, Gtk, WebKit2  # noqa: PLC0415

        self.GLib, self.Gtk, self.WebKit2 = GLib, Gtk, WebKit2
        if not Gtk.init_check([])[0]:
            raise RuntimeError("没有可用的 X 显示：用 `xvfb-run -a` 跑，或在有桌面的会话里直接跑")
        self.view = WebKit2.WebView()
        settings = self.view.get_settings()
        # Xvfb 没有 GPU：关掉 WebGL，免得渲染进程卡在初始化上。
        settings.set_enable_webgl(False)
        self.window = Gtk.OffscreenWindow()
        self.window.set_default_size(1024, 768)
        self.window.add(self.view)
        self.window.show_all()

        # 控制台的 CSP 拒绝信息是「引擎自己说的理由」，比只看到「没请求」更有说服力。
        # 是否打印由 `--console` 决定（默认不打印，免得几十行噪音混进结果表）。
        self.view.get_settings().set_enable_write_console_messages_to_stdout(console_messages)
        self.ucm = self.view.get_user_content_manager()
        # 白名单 UCM 脚本：CSP 管不到它（这是探针的地基，不是被测对象）。
        self.ucm.register_script_message_handler("probe")
        self.ucm.connect("script-message-received::probe", self._on_message)
        self.ucm.add_script(
            WebKit2.UserScript.new(
                PROBE_JS,
                WebKit2.UserContentInjectedFrames.ALL_FRAMES,
                WebKit2.UserScriptInjectionTime.START,
                None,
                None,
            )
        )
        self._beats: list[str] = []
        self._loaded = threading.Event()
        self._load_events: list[str] = []
        self.navigations: list[str] = []
        self.view.connect("load-changed", self._on_load_changed)
        self.view.connect("decide-policy", self._on_decide_policy)
        self._ctx = GLib.MainContext.default()
        self._doc_server: ThreadingHTTPServer | None = None
        self._doc_html = ""
        self.console_log: list[str] = []

    # -- 可选：把主文档放进一个本地 HTTP 服务（CSP 走响应头） --------------
    def use_document_server(self) -> int:
        """起一个只发「当前用例那一页」的本地 HTTP 服务，返回端口。"""
        from http.server import BaseHTTPRequestHandler  # noqa: PLC0415

        engine = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:  # noqa: N802
                body = engine._doc_html.encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                if engine.header_csp:
                    self.send_header("Content-Security-Policy", engine.header_csp)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args: Any) -> None:
                pass

        self._doc_server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._doc_server.serve_forever, daemon=True).start()
        return self._doc_server.server_address[1]

    header_csp: str | None = None

    # -- 事件回流 --------------------------------------------------------
    def _on_message(self, _manager, js_result) -> None:
        self._beats.append(js_result.get_js_value().to_string())

    def _on_decide_policy(self, _view, decision, decision_type) -> bool:
        """记下每一次导航请求（不干预，交给默认处理）。"""
        if decision_type == self.WebKit2.PolicyDecisionType.NAVIGATION_ACTION:
            self.navigations.append(decision.get_navigation_action().get_request().get_uri())
        return False

    def _on_load_changed(self, _view, event) -> None:
        self._load_events.append(str(event))
        if event == self.WebKit2.LoadEvent.FINISHED:
            self._loaded.set()

    # -- 界面脚本能否被满足 ------------------------------------------------
    # -- 主循环 ----------------------------------------------------------
    def pump(self, seconds: float) -> None:
        end = time.time() + seconds
        while time.time() < end:
            while self._ctx.iteration(False):
                pass
            time.sleep(0.004)

    def _eval(self, script: str, timeout: float = 25.0) -> str:
        """在页面里求值并拿回**字符串**结果。

        WebKitGTK 的 `evaluate_javascript` 只认可序列化的基本类型，
        返回 Promise 会直接 `Unsupported result type`，所以这里统一包一层：
        把结果（Promise 或值）转成字符串，写进 `window.__evalOut`，再读回来。
        """
        box: dict[str, Any] = {}

        def done(view, result, _data=None) -> None:
            try:
                value = view.evaluate_javascript_finish(result)
                box["value"] = value.to_string() if value is not None else None
            except Exception as error:  # noqa: BLE001
                box["error"] = error
            box["done"] = True

        wrapped = (
            "(function () { window.__evalOut = null; window.__evalErr = null;"
            " try { Promise.resolve(" + script + ").then("
            "  function (value) { window.__evalOut = String(value); },"
            "  function (error) { window.__evalErr = error; }); }"
            " catch (error) { window.__evalErr = error; }"
            " return 1; })()"
        )
        self.view.evaluate_javascript(wrapped, -1, None, None, None, done)
        end = time.time() + timeout
        while not box.get("done") and time.time() < end:
            self.pump(0.02)
        if not box.get("done"):
            raise TimeoutError(f"evaluate_javascript 超时（{timeout}s）：{script[:120]}")
        if "error" in box:
            raise RuntimeError(f"页面脚本报错：{box['error']}｜脚本：{script[:300]}")
        # 等 Promise 落地。
        end = time.time() + timeout
        while time.time() < end:
            state = self._eval_raw(
                "String(window.__evalOut !== null) + '|' + String(window.__evalErr !== null)"
                " + (window.__evalErr ? '|' + (window.__evalErr.stack || String(window.__evalErr)) : '')"
            )
            if state == "true|false":
                return self._eval_raw("window.__evalOut")
            if "|true|" in state:
                raise RuntimeError(f"页面脚本抛错：{state}｜脚本：{script[:300]}")
            self.pump(0.02)
        raise TimeoutError(f"页面脚本的 Promise 没有落地（{timeout}s）：{script[:120]}")

    def _eval_raw(self, script: str, timeout: float = 10.0) -> str | None:
        """同步求值，返回值必须是基本类型（字符串 / 数字）。"""
        box: dict[str, Any] = {}

        def done(view, result, _data=None) -> None:
            try:
                value = view.evaluate_javascript_finish(result)
                box["value"] = value.to_string() if value is not None else None
            except Exception as error:  # noqa: BLE001
                box["error"] = error
            box["done"] = True

        self.view.evaluate_javascript(script, -1, None, None, None, done)
        end = time.time() + timeout
        while not box.get("done") and time.time() < end:
            self.pump(0.02)
        if not box.get("done"):
            raise TimeoutError(f"evaluate_javascript 超时（{timeout}s）：{script[:120]}")
        if "error" in box:
            raise RuntimeError(f"evaluate_javascript 出错：{box['error']}")
        return box["value"]

    # -- 本脚本要的接口 --------------------------------------------------
    def run(self, case: Case, csp: str | None, *, port: int | None = None) -> dict:
        built = case.build(port) if port is not None else case.build(0)
        header_mode = self.header_csp is not None
        html, (effective, _page_written) = apply_nonce(built, csp, header_mode=header_mode)
        if header_mode:
            self.header_csp = effective
        script = built[1]
        self._beats.clear()
        self._loaded.clear()
        self._load_events.clear()
        self.navigations.clear()
        if self.header_csp is not None and self._doc_server is not None:
            self._doc_html = html
            port = self._doc_server.server_address[1]
            self.view.load_uri(f"http://127.0.0.1:{port}/")
        else:
            self.view.load_html(html, None)  # 主文档源 = about:blank
        # 注意：**不能**用 threading.Event.wait —— WebKitGTK 的事件（load-changed）
        # 要在 GLib 主循环里才有机会派发，只等 Event 会一直等到超时。
        deadline = time.time() + 20
        while not self._loaded.is_set() and time.time() < deadline:
            self.pump(0.02)
        if not self._loaded.is_set():
            raise TimeoutError(f"{case.id}: 页面 20 秒内没有加载完（load-changed 事件：{self._load_events}）")
        self.pump(case.settle)
        try:
            raw = self._eval(script)
        except TimeoutError:
            # 导航把脚本上下文冲掉了：`evaluate_javascript` 的 Promise 永远不会落地。
            # 只有在「页面真的换掉了」时才这么判——靠 `decide-policy` 里记下的导航请求确认。
            if any("nav-target" in uri for uri in self.navigations) or self.view.get_uri() != "about:blank":
                out = {"outcome": "navigated", "uri": self.view.get_uri(),
                       "navigations": list(self.navigations)}
                out.setdefault("beats", list(self._beats))
                out["requests"] = []
                return out
            raise
        if not raw:
            raise RuntimeError("页面没有返回值")
        out = json.loads(raw)
        out.setdefault("outcome", "script")
        out.setdefault("beats", list(self._beats))
        out["requests"] = []  # WebKitGTK 侧的请求证据来自本地监听端，见 B1
        return out

    def close(self) -> None:
        self.window.destroy()
        self.pump(0.05)


# ---------------------------------------------------------------------------
# 引擎二：Chromium（Playwright，Node 驱动见 webkit_sandbox_probe_chromium.mjs）
# ---------------------------------------------------------------------------


class ChromiumEngine:
    name = "chromium"
    uses_port = True

    def __init__(self, console_messages: bool = False) -> None:
        self.console_messages = console_messages
        if not CHROMIUM_DRIVER.exists():
            raise RuntimeError(f"缺 Chromium 驱动：{CHROMIUM_DRIVER}")
        # ESM 的 bare import 按**文件位置**解析：驱动脚本在 `scripts/`，而 `playwright`
        # 装在 `apps/shell/node_modules`，所以先把驱动复制进 `apps/shell` 的一个临时文件再跑
        # （跑完删掉；不在仓库根再装一份 node_modules）。
        import shutil  # noqa: PLC0415
        self._copy = ROOT / "apps" / "shell" / ".webkit_sandbox_probe_driver.mjs"
        shutil.copyfile(CHROMIUM_DRIVER, self._copy)
        self._proc = subprocess.Popen(
            ["node", str(self._copy)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            cwd=str(ROOT / "apps" / "shell"),
            env=({**os.environ, "XREMAIL_PROBE_CONSOLE": "1"} if console_messages else os.environ.copy()),
        )
        self._hello = self._call({"op": "init"})

    def _call(self, payload: dict) -> dict:
        assert self._proc.stdin and self._proc.stdout
        self._proc.stdin.write(json.dumps(payload) + "\n")
        self._proc.stdin.flush()
        line = self._proc.stdout.readline()
        if not line:
            err = ""
            if self._proc.poll() is not None and self._proc.stderr:
                err = self._proc.stderr.read()
            raise RuntimeError(f"Chromium 驱动退出（{self._proc.poll()}）：{err[-3000:]}")
        # 驱动的 stderr 里是引擎的控制台 / CSP 拒绝信息；`--console` 时原样转给用户。
        if self.console_messages and self._proc.poll() is None:
            self._drain_stderr()
        reply = json.loads(line)
        if "error" in reply:
            raise RuntimeError(reply["error"])
        return reply

    def _drain_stderr(self) -> None:
        """非阻塞地把子进程 stderr 里已经有的行打出来（不卡住主循环）。"""
        import select  # noqa: PLC0415

        assert self._proc.stderr
        while select.select([self._proc.stderr], [], [], 0)[0]:
            line = self._proc.stderr.readline()
            if not line:
                break
            sys.stderr.write(line)
        sys.stderr.flush()

    def run(self, case: Case, csp: str | None, *, port: int | None = None) -> dict:
        built = case.build(port) if port is not None else case.build(0)
        html, (effective, page_written) = apply_nonce(built, csp)
        script = built[1]
        reply = self._call({
            "op": "run", "html": html, "script": script, "settle": case.settle,
            "csp": effective, "cspPage": page_written,
        })
        out = json.loads(reply["value"]) if reply.get("value") else {}
        out.setdefault("beats", reply.get("beats", []))
        out["requests"] = reply.get("requests", [])
        return out

    def close(self) -> None:
        try:
            self._call({"op": "close"})
        except Exception:  # noqa: BLE001
            pass
        self._proc.terminate()
        if self.console_messages:
            try:
                self._drain_stderr()
            except Exception:  # noqa: BLE001
                pass
        try:
            self._copy.unlink()
        except FileNotFoundError:
            pass


# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------


def read_csp() -> str:
    conf = json.loads(TAURI_CONF.read_text(encoding="utf-8"))
    return conf["app"]["security"]["csp"]


def main(argv: list[str]) -> int:
    if "--write-cli-rs" in argv:
        sys.stdout.write(EMBEDDED_CLI)
        return 0

    parser = argparse.ArgumentParser(add_help=True, description="WebKitGTK HTML 隔离测试探针（本地手工验收）")
    parser.add_argument("--engine", choices=["webkitgtk", "chromium", "both"], default="webkitgtk")
    parser.add_argument("--json", type=pathlib.Path, default=None, help="把结果写成 JSON")
    parser.add_argument("--only", default=None, help="只跑指定用例，逗号分隔，如 B0,B1,C1")
    parser.add_argument("--console", action="store_true", help="把 WebKitGTK 的控制台输出（含 CSP 拒绝原因）打到终端")
    parser.add_argument(
        "--csp",
        choices=["meta", "header"],
        default="meta",
        help="WebKitGTK 侧主文档的 CSP 怎么下发：meta（默认，与 Chromium 对齐）或 header（更接近 Tauri：响应头 + 真实 origin）",
    )
    args = parser.parse_args(argv)

    csp = read_csp()
    wanted = set(args.only.split(",")) if args.only else None

    results: list[Result] = []
    observed: dict[str, dict[str, dict]] = {}

    with RecordingServer() as server:
        cases = [c for c in build_cases(csp, server.port) if wanted is None or c.id in wanted]

        engines: list[Any] = []
        if args.engine in ("webkitgtk", "both"):
            try:
                webkit = WebKitEngine(console_messages=args.console)
                if args.csp == "header":
                    webkit.use_document_server()
                engines.append(webkit)
            except Exception as error:  # noqa: BLE001
                print(f"[环境] WebKitGTK 起不来：{error}", file=sys.stderr)
                if args.engine == "webkitgtk":
                    return 2
        if args.engine in ("chromium", "both"):
            try:
                engines.append(ChromiumEngine(console_messages=args.console))
            except Exception as error:  # noqa: BLE001
                print(f"[环境] Chromium 起不来：{error}", file=sys.stderr)
                if args.engine == "chromium":
                    return 2
        if not engines:
            return 2

        for engine in engines:
            print(f"\n=== 引擎：{engine.name} ===")
            print(f"CSP 原文（{TAURI_CONF.relative_to(ROOT)}）：{csp}")
            mode = f"{args.csp}" if engine.name == "webkitgtk" else "header（mock 响应头）"
            print(f"主文档 CSP 下发方式：{mode}")
            print("注：探针自己的脚本用 nonce 单独放行；被测载荷（消毒器输出、故意绕过的样本）一律没有 nonce。\n")
            for case in cases:
                server.reset()
                try:
                    obs = engine.run(case, csp, port=server.port if engine.uses_port else None)
                    checks = case.check(obs, server)
                    # 兜底之一：Chromium 引擎会给出「页面发起的全部请求」，其中发往文档占位地址
                    # `127.0.0.1:1`（驱动把文档 mock 在那里）的不算——那只是相对地址被解析后的样子；
                    # 发往监听端（server.port）的由 B1 / F1 自己的断言与下面的第二道兜底负责。
                    server_port = str(server.port)
                    stray = [
                        url for url in obs.get("requests", [])
                        if "127.0.0.1" in url and server_port not in url and ":1/" not in url
                    ]
                    if stray and not case.allows_network and not server.hits:
                        checks.append((
                            "（兜底）引擎没有向被测服务器之外的地址发出请求",
                            False, f"未预期的请求：{stray}",
                        ))
                except Exception as error:  # noqa: BLE001
                    results.append(Result(case.id, f"{case.title}（用例本身出错）", False, repr(error), engine.name))
                    print(f"[{case.id}] {case.title}\n    ERROR {error}")
                    traceback.print_exc()
                    continue
                # 兜底之二：只要不是自带对照的用例，监听端记到的**每一条请求**都要能对上用例里的
                # `127.0.0.1:PORT` 目标；对不上的（额外的连接、打到同名路径的探针）都算失败。
                if not case.allows_network:
                    built_html = case.build(server.port)[0]
                    expected = {path for path in server.hits if f"127.0.0.1:{server.port}{path}" in built_html}
                    unexpected = [path for path in server.hits if path not in expected]
                    if unexpected or server.tcp_connects > len(server.hits):
                        checks.append((
                            "（兜底）监听端没有收到用例之外的请求",
                            False,
                            f"未预期的路径：{unexpected}；TCP 连接 {server.tcp_connects} 次 / HTTP 请求 {len(server.hits)} 次",
                        ))
                observed.setdefault(engine.name, {})[case.id] = {
                    "title": case.title,
                    "observed": obs,
                    "network": {"tcp": server.tcp_connects, "paths": list(server.hits)},
                }
                print(f"[{case.id}] {case.title}")
                for check, ok, value in checks:
                    print(f"    {'PASS' if ok else 'FAIL'}  {check}\n          观测：{value}")
                    results.append(Result(case.id, check, ok, value, engine.name))
                print()
            engine.close()

    print("=== 汇总 ===")
    ids = sorted({r.case for r in results})
    for case_id in ids:
        rows = [r for r in results if r.case == case_id]
        ok = sum(1 for r in rows if r.ok)
        print(f"  {case_id}: {ok}/{len(rows)} PASS" + ("" if ok == len(rows) else "  ← 有 FAIL"))
    passed = sum(1 for r in results if r.ok)
    print(f"合计：{passed}/{len(results)} PASS")

    if "webkitgtk" in observed and "chromium" in observed:
        print("\n=== WebKitGTK vs Chromium（同一用例、同一份 CSP、同一份断言）===")
        same, diff = [], []
        for case_id in sorted(set(observed["webkitgtk"]) | set(observed["chromium"])):
            rows_w = [r for r in results if r.engine == "webkitgtk" and r.case == case_id]
            rows_c = [r for r in results if r.engine == "chromium" and r.case == case_id]
            verdict_w = all(r.ok for r in rows_w) if rows_w else None
            verdict_c = all(r.ok for r in rows_c) if rows_c else None
            (same if verdict_w == verdict_c else diff).append(case_id)
            print(f"  {case_id}: webkitgtk={'全部 PASS' if verdict_w else ('有 FAIL' if verdict_w is False else '未跑')}"
                  f"  chromium={'全部 PASS' if verdict_c else ('有 FAIL' if verdict_c is False else '未跑')}")
        print(f"  结论一致：{same or '无'}；结论不同：{diff or '无'}")

    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(
            json.dumps({"csp": csp, "results": [r.__dict__ for r in results], "observed": observed},
                       ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        print(f"\n结果已写入 {args.json}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
