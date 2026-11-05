/**
 * `uncertain`（服务器没确认）的二次确认流。
 *
 * 这是风险最高的一处交互：`uncertain` 的意思是「**不知道**对方收没收到」，
 * 所以
 *
 * - 不能自动重发（会让用户收到两封）；
 * - 「再发一次」与「撤回来改」都要用户**明确确认**；
 * - 确认文案要说明「对方可能已经收到了」。
 */
import { screen } from "@testing-library/react";
import { renderApp, settle } from "../test/harness";
import { backendWithAccount, openCompose } from "./compose-support";

vi.mock("@tauri-apps/api/core", async () => {
  const { invokeMock } = await import("../test/tauriState");
  return { invoke: invokeMock };
});

vi.mock("@tauri-apps/api/event", async () => {
  const { listenMock } = await import("../test/tauriState");
  return { listen: listenMock };
});

/** 填收件人并发一封（后端会回 uncertain）。 */
async function sendUncertain(user: Awaited<ReturnType<typeof renderApp>>["user"]) {
  await user.type(screen.getByLabelText("收件人"), "to@example.com");
  await user.click(screen.getByRole("button", { name: "发送" }));
  await settle();
}

describe("发送结果不确定时的二次确认", () => {
  it("不会自动重发，发送按钮停用，并给出两个动作", async () => {
    const backend = backendWithAccount();
    // 让假后端回「不确定」。
    backend.sendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    // 只发过一次，没有自动重试。
    expect(backend.callsTo("send_message")).toHaveLength(1);

    const result = screen.getByTestId("compose-send-result");
    expect(result).toHaveTextContent("不确定");
    expect(result).not.toHaveTextContent("发送失败");
    // 发送按钮停用：要再发得走「再发一次」那条路。
    expect(screen.getByRole("button", { name: "发送" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "再发一次" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "撤回来改" })).toBeInTheDocument();
  });

  it("「再发一次」要先确认，确认之后走 resend_uncertain，而不是新写一封", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    await user.click(screen.getByRole("button", { name: "再发一次" }));
    await settle();

    // 确认框要说清楚风险。
    const confirm = screen.getByTestId("compose-confirm");
    expect(confirm).toHaveTextContent("两封");
    // 还没确认 → 没有再发。
    expect(backend.callsTo("resend_uncertain")).toHaveLength(0);
    expect(backend.callsTo("send_message")).toHaveLength(1);

    // 这次让后端回「已发送」。
    backend.resendState = "sent";
    await user.click(screen.getByRole("button", { name: "确认再发一次" }));
    await settle();

    // 走的是「把原来那封再提交一次」，带的就是原来那封的条目 id。
    const resends = backend.callsTo("resend_uncertain");
    expect(resends).toHaveLength(1);
    expect(resends[0].args.entryId).toBe("entry-1");
    // 没有新写一封信：send_message 仍然只有最初那一次。
    expect(backend.callsTo("send_message")).toHaveLength(1);
    expect(backend.sentDrafts).toHaveLength(1);

    // 结果改成「已发送」，且 Message-ID 就是原来那封的。
    const result = screen.getByTestId("compose-send-result");
    expect(result).toHaveTextContent("已发送");
    expect(result).toHaveTextContent("<sent-1@example.com>");
    // 名单里没有了，也没有「另外还有 N 封」的提示。
    expect(
      screen.queryByTestId("compose-uncertain-pending"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "撤回来改" }),
    ).not.toBeInTheDocument();
  });

  it("「再发一次」之后仍然不确定时，这封留在未确认名单里", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    backend.resendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);
    await user.click(screen.getByRole("button", { name: "再发一次" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认再发一次" }));
    await settle();

    expect(backend.callsTo("resend_uncertain")).toHaveLength(1);
    expect(backend.callsTo("send_message")).toHaveLength(1);

    // 还是不确定：结果区仍是「不确定」，两个动作都还在。
    const result = screen.getByTestId("compose-send-result");
    expect(result).toHaveTextContent("不确定");
    expect(screen.getByRole("button", { name: "再发一次" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "撤回来改" })).toBeInTheDocument();

    // 这封仍然能单独撤回（说明它还在名单里）。
    await user.click(screen.getByRole("button", { name: "撤回来改" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认撤回来" }));
    await settle();
    expect(backend.callsTo("withdraw_draft")[0].args.entryId).toBe("entry-1");
  });

  it("「再发一次」被后端拒绝时报失败，并保留「撤回来改」入口", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    backend.resendError = new Error("这封信正在被另一个发送器处理，请稍后再试");
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);
    await user.click(screen.getByRole("button", { name: "再发一次" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认再发一次" }));
    await settle();

    const result = screen.getByTestId("compose-send-result");
    expect(result).toHaveTextContent("再发一次失败");
    expect(result).toHaveTextContent("请稍后再试");
    expect(backend.callsTo("send_message")).toHaveLength(1);

    // 失败的结果区只有「改完再点发送」那个入口；
    // 撤回来改挂在下面的「未确认」名单里（这封其实还是没确认）。
    await user.click(screen.getByRole("button", { name: "撤回来改 entry-1" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认撤回来" }));
    await settle();
    expect(backend.callsTo("withdraw_draft")[0].args.entryId).toBe("entry-1");
  });

  it("「再发一次」取消后不会重发", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    await user.click(screen.getByRole("button", { name: "再发一次" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "取消" }));
    await settle();

    expect(backend.callsTo("send_message")).toHaveLength(1);
    expect(backend.callsTo("resend_uncertain")).toHaveLength(0);
    expect(screen.queryByTestId("compose-confirm")).not.toBeInTheDocument();
  });

  it("「撤回来改」确认后会把信撤回来，重新变成可编辑的草稿", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    await user.click(screen.getByRole("button", { name: "撤回来改" }));
    await settle();
    expect(screen.getByTestId("compose-confirm")).toHaveTextContent("两封");

    await user.click(screen.getByRole("button", { name: "确认撤回来" }));
    await settle();

    const calls = backend.callsTo("withdraw_draft");
    expect(calls).toHaveLength(1);
    expect(calls[0].args.entryId).toBe("entry-1");
    // 不确定的结果没了，编辑区又能改了。
    expect(screen.queryByTestId("compose-send-result")).not.toBeInTheDocument();
    expect(screen.getByLabelText("正文")).not.toBeDisabled();
  });

  it("「撤回来改」取消后不会撤回", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    await user.click(screen.getByRole("button", { name: "撤回来改" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "取消" }));
    await settle();

    expect(backend.callsTo("withdraw_draft")).toHaveLength(0);
    // 结果未确认的提示还在。
    expect(screen.getByTestId("compose-send-result")).toBeInTheDocument();
  });

  it("「再发一次」之后仍然不确定时，单独撤回会撤到原来那一封", async () => {
    const backend = backendWithAccount();
    backend.sendState = "uncertain";
    backend.resendState = "uncertain";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);
    await user.click(screen.getByRole("button", { name: "再发一次" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认再发一次" }));
    await settle();

    // 再提交一次没有产生新的一封（Message-ID 不变），
    // 所以名单里还是同一封，结果区仍然是不确定。
    expect(backend.callsTo("resend_uncertain")).toHaveLength(1);
    expect(screen.getByTestId("compose-send-result")).toHaveTextContent(
      "不确定",
    );

    await user.click(screen.getByRole("button", { name: "撤回来改" }));
    await settle();
    await user.click(screen.getByRole("button", { name: "确认撤回来" }));
    await settle();

    const calls = backend.callsTo("withdraw_draft");
    expect(calls).toHaveLength(1);
    expect(calls[0].args.entryId).toBe("entry-1");
    // 撤回之后它变回草稿，可以接着改。
    expect(screen.queryByTestId("compose-send-result")).not.toBeInTheDocument();
    expect(screen.getByLabelText("正文")).not.toBeDisabled();
  });

  it("明确失败与不确定的文案分得清", async () => {
    const backend = backendWithAccount();
    backend.sendState = "failed";
    const { user } = await renderApp(backend);
    await openCompose(user);

    await sendUncertain(user);

    const failed = screen.getByTestId("compose-send-result");
    expect(failed).toHaveTextContent("发送失败");
    expect(failed).not.toHaveTextContent("不确定");
    // 明确失败是可以直接改完再发的：发送按钮还能用。
    expect(screen.getByRole("button", { name: "发送" })).not.toBeDisabled();
    expect(screen.queryByRole("button", { name: "再发一次" })).not.toBeInTheDocument();
  });
});
