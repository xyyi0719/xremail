import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import AccountForm from "../components/AccountForm";
import { PROVIDER_PRESETS } from "../types";

/**
 * 服务商预设的预填行为。
 *
 * ## 为什么这组测试重要
 *
 * `PROVIDER_PRESETS` 是 `xremail-core::model::ProviderKind::presets()` 的**镜像**
 * （权威在 Rust 侧，那边有 `provider_preset_tests`）。两份值不一致的表现是
 * 「界面上填的和实际连的不一样」——最难查的一类 bug。这里逐家比对。
 */

/** 从界面上读当前的表单值。 */
function readForm() {
  const value = (label: RegExp) =>
    (screen.getByLabelText(label) as HTMLInputElement | HTMLSelectElement).value;
  return {
    imapHost: value(/IMAP 服务器/),
    imapPort: value(/IMAP 端口/),
    smtpHost: value(/SMTP 服务器/),
    smtpPort: value(/SMTP 端口/),
  };
}

describe("服务商预设", () => {
  it("选服务商后自动填上实测过的服务器参数", async () => {
    const user = userEvent.setup();
    render(<AccountForm />);

    // 默认是 163，切到 QQ 再切回来，验证参数跟着走。
    await user.selectOptions(screen.getByLabelText(/服务商/), "tencent");
    expect(readForm()).toEqual({
      imapHost: "imap.qq.com",
      imapPort: "993",
      smtpHost: "smtp.qq.com",
      smtpPort: "465",
    });

    await user.selectOptions(screen.getByLabelText(/服务商/), "netease_163");
    expect(readForm()).toEqual({
      imapHost: "imap.163.com",
      imapPort: "993",
      smtpHost: "smtp.163.com",
      smtpPort: "465",
    });
  });

  it("前端镜像与 Rust 侧的预设置一致", () => {
    // 这些值两边必须一模一样。Rust 侧见 crates/xremail-core/src/model.rs
    // 的 provider_preset_tests；任何一边改了没同步，这两组断言会有一个红。
    expect(PROVIDER_PRESETS.netease_163).toMatchObject({
      imapHost: "imap.163.com",
      imapPort: 993,
      smtpHost: "smtp.163.com",
      smtpPort: 465,
    });
    expect(PROVIDER_PRESETS.tencent).toMatchObject({
      imapHost: "imap.qq.com",
      smtpHost: "smtp.qq.com",
    });
    expect(PROVIDER_PRESETS.wo).toMatchObject({
      imapHost: "imap.wo.cn",
      smtpHost: "smtp.wo.cn",
    });
    expect(PROVIDER_PRESETS.netease_enterprise).toMatchObject({
      imapHost: "imap.qiye.163.com",
      smtpHost: "smtp.qiye.163.com",
    });
  });

  it("没实测过的服务商不预填，也不清空用户已填的内容", async () => {
    const user = userEvent.setup();
    render(<AccountForm />);

    // 先切到 163 拿到一组值。
    await user.selectOptions(screen.getByLabelText(/服务商/), "netease_163");
    // 再切到「126」——它没有实测预设。
    await user.selectOptions(screen.getByLabelText(/服务商/), "netease_126");

    // 值保持在上一个预设，没有被清空（用户可能只是改了标签）。
    expect(readForm().imapHost).toBe("imap.163.com");
    // 而且 126 确实不在预设表里。
    expect(PROVIDER_PRESETS.netease_126).toBeUndefined();
  });

  it("用户手改过的字段不会被切换服务商覆盖", async () => {
    const user = userEvent.setup();
    render(<AccountForm />);

    // 用户手工把 IMAP 端口改成 143。
    const port = screen.getByLabelText(/IMAP 端口/);
    await user.clear(port);
    await user.type(port, "143");

    // 切到 QQ：端口不该被抢回 993。
    await user.selectOptions(screen.getByLabelText(/服务商/), "tencent");
    expect(readForm().imapPort).toBe("143");
    // 但没动过的 host 应该被填上。
    expect(readForm().imapHost).toBe("imap.qq.com");
  });
});
