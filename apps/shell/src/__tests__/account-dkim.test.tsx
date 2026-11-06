/**
 * 账户的 DKIM 配置面板。
 *
 * 这是整个前端**唯一**碰私钥的地方，所以断言的重点不是「能保存」，
 * 而是「私钥不留下痕迹」：
 *
 * - 私钥输入框是 `type="password"`；
 * - 保存成功后输入框立刻清空（不让明文躺在 React state 里）；
 * - 面板只显示「已配置 selector@domain」和 DNS 记录，**不显示任何私钥片段**；
 * - 后端返回的配置里没有 `-----BEGIN` 字样——私钥只进系统钥匙串，不回传。
 */
import { screen } from "@testing-library/react";
import { createBackend } from "../test/backend";
import { makeAccount } from "../test/fixtures";
import { ACCOUNT_EMAIL, renderApp, settle } from "../test/harness";

vi.mock("@tauri-apps/api/core", async () => {
  const { invokeMock } = await import("../test/tauriState");
  return { invoke: invokeMock };
});

vi.mock("@tauri-apps/api/event", async () => {
  const { listenMock } = await import("../test/tauriState");
  return { listen: listenMock };
});

/** 一段看起来像私钥的文本（测试用，不是真的钥）。 */
const PRIVATE_KEY_PEM = [
  "-----BEGIN PRIVATE KEY-----",
  "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7TESTSECRET",
  "-----END PRIVATE KEY-----",
].join("\n");

function backendWithAccount() {
  return createBackend({
    accounts: [makeAccount({ id: "acc-1", email: ACCOUNT_EMAIL })],
  });
}

describe("DKIM 配置面板", () => {
  it("未配置时显示「未配置」，输入框里没有私钥", async () => {
    const backend = backendWithAccount();
    await renderApp(backend);

    const status = await screen.findByTestId("dkim-status");
    expect(status).toHaveTextContent("DKIM：未配置");

    const keyInput = screen.getByLabelText(/私钥 PEM/);
    expect(keyInput).toHaveValue("");
    // 私钥框必须是 password：明文不该在屏幕上（也不该被 devtools 顺手截屏带走）。
    expect(keyInput).toHaveAttribute("type", "password");
    expect(keyInput).toHaveAttribute("autocomplete", "off");
  });

  it("保存后显示「已配置 selector@domain」，私钥立刻清空且不出现在页面里", async () => {
    const backend = backendWithAccount();
    const { user } = await renderApp(backend);

    await user.type(await screen.findByLabelText(/选择器/), "default");
    await user.type(screen.getByLabelText(/签名域名/), "Example.COM");
    await user.type(screen.getByLabelText(/私钥 PEM/), PRIVATE_KEY_PEM);
    await user.click(screen.getByRole("button", { name: "保存 DKIM 配置" }));
    await settle();

    const calls = backend.callsTo("set_dkim_config");
    expect(calls).toHaveLength(1);
    expect(calls[0].args).toEqual({
      accountId: "acc-1",
      selector: "default",
      domain: "Example.COM",
      // user-event 往密码框里逐字敲会吞掉换行；PEM 的关键是「原样送到」。
      privateKeyPem: PRIVATE_KEY_PEM.replace(/\n/g, ""),
    });

    const status = await screen.findByTestId("dkim-status");
    expect(status).toHaveTextContent("DKIM：已配置 default@example.com");

    // 私钥已经交给后端（→ 系统钥匙串），组件里不能还留着。
    expect(screen.getByLabelText(/私钥 PEM/)).toHaveValue("");
    // 页面上任何地方都不能出现私钥正文或 PEM 头。
    expect(document.body.textContent ?? "").not.toContain("TESTSECRET");
    expect(document.body.textContent ?? "").not.toContain("-----BEGIN");

    // DNS 记录要给出来，用户才贴得进域名解析。
    const dns = await screen.findByTestId("dkim-dns");
    expect(dns).toHaveTextContent("default._domainkey.example.com");

    // 后端回来的 JSON 里也没有私钥（假后端与真后端同一个口径）。
    const returned = await backend.impl("get_dkim_config", { accountId: "acc-1" });
    expect(JSON.stringify(returned)).not.toContain("-----BEGIN");
    expect(JSON.stringify(returned)).not.toContain("TESTSECRET");
  });

  it("已配置时只换选择器可以留空私钥", async () => {
    const backend = backendWithAccount();
    const { user } = await renderApp(backend);

    await user.type(await screen.findByLabelText(/选择器/), "default");
    await user.type(screen.getByLabelText(/签名域名/), "example.com");
    await user.type(screen.getByLabelText(/私钥 PEM/), PRIVATE_KEY_PEM);
    await user.click(screen.getByRole("button", { name: "保存 DKIM 配置" }));
    await settle();

    const selector = screen.getByLabelText(/选择器/);
    await user.clear(selector);
    await user.type(selector, "second");
    await user.click(screen.getByRole("button", { name: "更新 DKIM 配置" }));
    await settle();

    const calls = backend.callsTo("set_dkim_config");
    expect(calls).toHaveLength(2);
    expect(calls[1].args).toMatchObject({
      selector: "second",
      privateKeyPem: "",
    });
    expect(await screen.findByTestId("dkim-status")).toHaveTextContent(
      "DKIM：已配置 second@example.com",
    );
  });

  it("清除后回到「未配置」，并调用 clear_dkim_config", async () => {
    const backend = backendWithAccount();
    const { user } = await renderApp(backend);

    await user.type(await screen.findByLabelText(/选择器/), "default");
    await user.type(screen.getByLabelText(/签名域名/), "example.com");
    await user.type(screen.getByLabelText(/私钥 PEM/), PRIVATE_KEY_PEM);
    await user.click(screen.getByRole("button", { name: "保存 DKIM 配置" }));
    await settle();

    await user.click(screen.getByRole("button", { name: "清除 DKIM 配置" }));
    await settle();

    expect(backend.callsTo("clear_dkim_config")).toHaveLength(1);
    expect(await screen.findByTestId("dkim-status")).toHaveTextContent(
      "DKIM：未配置",
    );
  });

  it("保存失败时把原因说出来", async () => {
    const backend = backendWithAccount();
    backend.dkimSaveError = new Error("私钥太短：只接受 2048 位以上的 RSA 私钥");
    const { user } = await renderApp(backend);

    await user.type(await screen.findByLabelText(/选择器/), "default");
    await user.type(screen.getByLabelText(/签名域名/), "example.com");
    await user.type(screen.getByLabelText(/私钥 PEM/), PRIVATE_KEY_PEM);
    await user.click(screen.getByRole("button", { name: "保存 DKIM 配置" }));
    await settle();

    const notice = screen.getByTestId("app-notice");
    expect(notice).toHaveTextContent("保存 DKIM 配置失败");
    expect(notice).toHaveTextContent("2048");
    expect(notice).toHaveClass("app-notice-error");
    // 失败时也不能把私钥打进提示里。
    expect(notice.textContent ?? "").not.toContain("TESTSECRET");
  });
});
