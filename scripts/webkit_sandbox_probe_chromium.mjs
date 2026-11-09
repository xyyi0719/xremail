/**
 * `scripts/webkit_sandbox_probe.py --engine=chromium|both` 的 Chromium 侧驱动。
 *
 * 为什么不把对照写成 Playwright 用例（`apps/shell/e2e/*.spec.ts`）：
 * 那样要在 `apps/shell` 里加文件、还要起 Vite、还要套 @playwright/test 的断言风格；
 * 而这里**必须和 WebKitGTK 侧吃同一份用例、同一份 CSP、同一份断言**，
 * 才能回答「两个引擎的行为是否一致」。所以 Python 侧出页面、做判定，
 * 这个文件只负责：把页面放进 Chromium、按给定 CSP 覆盖文档策略、跑一段脚本、把结果与心跳带回。
 *
 * 协议（stdin/stdout，一行一个 JSON）：
 *   {"op":"init"} → {"ok":true,"version":"…"}
 *   {"op":"run","html":…,"script":…,"settle":s,"csp":…|null}
 *     → {"value":"<JSON 字符串>","beats":[…],"requests":[…],"cspOverride":…}
 *   {"op":"close"} → {"ok":true}
 *
 * ## 为什么要覆盖文档的 CSP（而不是让页面自己带 meta）
 *
 * Playwright 的 `page.setContent()` 在 `about:blank` 上写文档，而 `about:blank` **继承**
 * 创建者（Playwright 的默认上下文/空页面）的策略；Playwright 1.65 的 Chromium 在页面导航时
 * 还会**传一条 CSP HTTP 头**。多条策略是取交集的，于是「页面里写的那条 CSP」与
 * 「引擎实际执行的那条」不是一回事，拿它跟 WebKitGTK 比就不公平。
 * 这里用 `page.route` 把文档 mock 成 `http://127.0.0.1:1/__probe_document__`（保留端口，
 * 不会有真的请求打出去），响应头里**只放一条** CSP：Python 侧算好的
 * 「原始策略 + script-src 加 nonce」。WebKitGTK 侧同样只执行一条（`<meta>` 里那条），两边可比。
 *
 * 代价（写进报告）：这一幕的源是 `http://127.0.0.1:1`，WebKitGTK 侧是 `about:blank`；
 * 两者都不是 `tauri://localhost`，`'self'` 指向的源两边不同。
 *
 * 心跳：页面里 `__probe()` → `console.log('__PROBE__:' + msg)`，这里用 `page.on('console')`
 * 收回来；`__probe` 的具体实现在被测页面里（Python 侧拼的），本驱动只认 `__PROBE__:` 前缀。
 */
import { createInterface } from "node:readline";
import { chromium } from "playwright";

let browser = null;
let context = null;
let page = null;
let beats = [];
let requests = [];

async function init() {
  browser = await chromium.launch();
  context = await browser.newContext();
  page = await context.newPage();
  page.on("console", (message) => {
    const text = message.text();
    if (text.startsWith("__PROBE__:")) {
      beats.push(text.slice("__PROBE__:".length));
    } else if (process.env.XREMAIL_PROBE_CONSOLE) {
      // 控制台里的 CSP 拒绝信息（「Refused to load …」）是引擎给出的理由，
      // 打开 XREMAIL_PROBE_CONSOLE=1 时会打到 stderr，供人工核对。
      process.stderr.write(`[chromium] ${message.type()}: ${text}\n`);
    }
  });
  page.on("request", (request) => {
    requests.push(request.url());
  });
  // 「Security」面板里的 CSP 违规（DevTools 的 Log 域），只在打开 XREMAIL_PROBE_CONSOLE 时打印。
  if (process.env.XREMAIL_PROBE_CONSOLE) {
    const client = await context.newCDPSession(page);
    await client.send("Log.enable");
    client.on("Log.entryAdded", ({ entry }) => {
      const text = entry.text || "";
      if (/Content Security Policy|CSP|Refused|violates/i.test(text)) {
        process.stderr.write(`[chromium] security: ${text}\n`);
      }
    });
  }
  return { ok: true, version: browser.version() };
}

async function run({ html, script, settle, csp }) {
  beats = [];
  requests = [];
  let cspOverride = "n/a";

  await page.route("**/__probe_document__", async (route) => {
    const headers = { "content-type": "text/html; charset=utf-8" };
    if (csp) headers["content-security-policy"] = csp;
    await route.fulfill({ status: 200, headers, body: html });
    cspOverride = csp ? "文档响应头只带探针指定的那一条 CSP" : "文档响应头不带 CSP";
  });
  await page.goto("http://127.0.0.1:1/__probe_document__", { waitUntil: "load" });
  if (settle) {
    await page.waitForTimeout(Math.round(settle * 1000));
  }
  let value = null;
  let outcome = "script";
  try {
    value = await page.evaluate(script);
  } catch (error) {
    // 导航把执行上下文冲掉了 —— 这本身就是观测结果（主文档被换掉了）。
    if (/Execution context was destroyed|navigat/i.test(String(error))) {
      outcome = "navigated";
      value = JSON.stringify({ outcome, url: page.url() });
    } else {
      throw error;
    }
  }
  if (outcome === "script") value = JSON.stringify(Object.assign({ outcome }, JSON.parse(value)));
  return { value, beats: beats.slice(), requests: requests.slice(), cspOverride };
}

async function main() {
  const rl = createInterface({ input: process.stdin });
  for await (const line of rl) {
    if (!line.trim()) continue;
    let request;
    try {
      request = JSON.parse(line);
    } catch (error) {
      process.stdout.write(JSON.stringify({ error: `坏请求：${error}` }) + "\n");
      continue;
    }
    try {
      let reply;
      if (request.op === "init") reply = await init();
      else if (request.op === "run") reply = await run(request);
      else if (request.op === "close") {
        await browser?.close();
        process.stdout.write(JSON.stringify({ ok: true }) + "\n");
        process.exit(0);
      } else reply = { error: `未知 op：${request.op}` };
      process.stdout.write(JSON.stringify(reply) + "\n");
    } catch (error) {
      process.stdout.write(JSON.stringify({ error: String(error?.stack || error) }) + "\n");
    }
  }
  await browser?.close().catch(() => {});
}

main().catch((error) => {
  process.stdout.write(JSON.stringify({ error: String(error?.stack || error) }) + "\n");
  process.exit(1);
});
