/**
 * 读信侧的附件：列表、下载、打开、另存为，以及内联图片（`cid:`）。
 *
 * 这一组用例固化三条约定：
 *
 * 1. **附件字节不走 IPC**——前端只拿元数据，下载交给后端落盘，
 *    「打开」传路径给系统；用例断言 `invoke` 的参数里从来没有内容。
 * 2. **下载完成后给可点击的提示**——用户要知道文件去哪了。
 * 3. **内联图片只显示后端替换好的那份**——`htmlSafe` 里的 `data:` 图直接渲染，
 *    `cid:` 或远程地址不许出现在 DOM 里。
 */
import { screen, within } from "@testing-library/react";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { createBackend } from "../test/backend";
import { makeAccount, makeFolder, makeMessage } from "../test/fixtures";
import { ACCOUNT_EMAIL, nextMacrotask, renderApp, settle } from "../test/harness";
import type { Attachment, MessageBody } from "../types";

vi.mock("@tauri-apps/api/core", async () => {
  const { invokeMock } = await import("../test/tauriState");
  return { invoke: invokeMock };
});

vi.mock("@tauri-apps/api/event", async () => {
  const { listenMock } = await import("../test/tauriState");
  return { listen: listenMock };
});

/** 一份普通附件。 */
function makeAttachment(overrides: Partial<Attachment> = {}): Attachment {
  return {
    id: "<m1@example.com>/2",
    filename: "销售报告.pdf",
    contentType: "application/pdf",
    size: 2 * 1024 * 1024,
    contentId: null,
    isInline: false,
    downloaded: false,
    downloadedAtMs: null,
    localPath: null,
    ...overrides,
  };
}

function backendWith(
  attachments: Attachment[],
  extra: { messageBody?: MessageBody; downloadError?: Error | null } = {},
) {
  return createBackend({
    accounts: [makeAccount({ id: "acc-1", email: ACCOUNT_EMAIL })],
    folders: [makeFolder({ id: "f1", name: "收件箱" })],
    messages: [
      makeMessage({ id: "m1", accountId: "acc-1", folderId: "f1", uid: 101 }),
    ],
    attachments: { m1: attachments },
    ...(extra.messageBody ? { messageBody: extra.messageBody } : {}),
    ...(extra.downloadError ? { downloadError: extra.downloadError } : {}),
  });
}

/** 打开文件夹并点开那封邮件。 */
async function openMail(backend: ReturnType<typeof createBackend>) {
  const { user, openFolder } = await renderApp(backend);
  await openFolder();
  await user.click(screen.getByRole("button", { name: /测试邮件/ }));
  await settle();
  return user;
}

/**
 * 点一下，然后把随之而来的异步更新等到落定。
 *
 * `user-event` 的指针事件里有几拍 `setTimeout`（点击间隔、延迟），
 * 所以点完要**先在 act 外面**让出一个宏任务，再进 `settle`——
 * 反过来的话，那些更新会落在 act 之外（见 `harness.tsx` 的说明）。
 */
async function click(user: UserLike, target: Element) {
  await user.click(target);
  await nextMacrotask();
  await settle();
}

/** 只用得到 `click` 那一点能力，这样调用处不必写长长的类型。 */
type UserLike = { click: (target: Element) => Promise<void> };

describe("邮件附件", () => {
  it("正文下面列出附件：文件名、大小、下载按钮", async () => {
    await openMail(backendWith([makeAttachment()]));

    const panel = await screen.findByTestId("message-attachments");
    expect(within(panel).getByText("销售报告.pdf")).toBeTruthy();
    expect(within(panel).getByText(/2\.0 MB/)).toBeTruthy();
    expect(within(panel).getByTestId("attachment-download")).toBeEnabled();
  });

  it("没有附件时整块不出现（不留空标题）", async () => {
    await openMail(backendWith([]));
    expect(screen.queryByTestId("message-attachments")).toBeNull();
  });

  it("点下载会带齐邮件与附件标识；完成后给出可点的保存位置", async () => {
    const backend = backendWith([makeAttachment()]);
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-download"));

    const call = backend.lastCallTo("download_attachment");
    expect(call?.args).toMatchObject({
      accountId: "acc-1",
      folderId: "f1",
      uid: 101,
      attachmentId: "<m1@example.com>/2",
    });
    // 参数里只有标识，**没有任何内容**（字节不走 IPC）。
    expect(JSON.stringify(call?.args)).not.toMatch(/base64|content|bytes/i);

    const saved = await screen.findByTestId("attachment-saved");
    expect(within(saved).getByText(/attachments\/m1/)).toBeTruthy();
    expect(await screen.findByTestId("attachment-download")).toHaveTextContent(
      "已下载",
    );
  });

  it("点「已保存到 …」的路径会交给文件管理器", async () => {
    const backend = backendWith([makeAttachment()]);
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-download"));
    await screen.findByTestId("attachment-saved");
    await click(user, screen.getByTestId("attachment-reveal"));

    expect(openPath).toHaveBeenCalledTimes(1);
    expect(vi.mocked(openPath).mock.calls[0][0]).toMatch(/attachments\/m1/);
  });

  it("下载失败时提示错误，按钮回到可重试", async () => {
    const backend = backendWith([makeAttachment()], {
      downloadError: new Error("网络断了"),
    });
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-download"));

    expect(await screen.findByRole("alert")).toHaveTextContent(/网络断了/);
    expect(await screen.findByTestId("attachment-download")).toBeEnabled();
    expect(screen.queryByTestId("attachment-saved")).toBeNull();
  });

  it("已经下载过的附件直接是「已下载」，不重复下载", async () => {
    const backend = backendWith([
      makeAttachment({
        downloaded: true,
        downloadedAtMs: 1_700_000_000_000,
        localPath: "C:/data/attachments/m1/abc",
      }),
    ]);
    const user = await openMail(backend);

    const button = await screen.findByTestId("attachment-download");
    expect(button).toHaveTextContent("已下载");
    expect(button).toBeDisabled();

    // 「打开」走的是本地的 opener 命令，不需要再连服务器。
    await click(user, screen.getByTestId("attachment-open"));
    await settle();
    expect(backend.callsTo("download_attachment")).toHaveLength(0);
    expect(backend.lastCallTo("open_attachment")?.args).toMatchObject({
      messageId: "m1",
      attachmentId: "<m1@example.com>/2",
    });
  });

  it("没下载过的点「打开」会先下载再打开", async () => {
    const backend = backendWith([makeAttachment()]);
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-open"));
    await settle();

    const cmds = backend.calls.map((one) => one.cmd);
    expect(cmds.indexOf("download_attachment")).toBeGreaterThan(-1);
    expect(cmds.indexOf("open_attachment")).toBeGreaterThan(
      cmds.indexOf("download_attachment"),
    );
  });

  it("另存为：先问存哪，再让后端复制过去", async () => {
    vi.mocked(saveFileDialog).mockResolvedValue("D:/下载/报告.pdf");
    const backend = backendWith([
      makeAttachment({ downloaded: true, localPath: "C:/data/attachments/m1/abc" }),
    ]);
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-save-as"));
    await settle();

    expect(backend.lastCallTo("save_attachment_as")?.args).toMatchObject({
      messageId: "m1",
      attachmentId: "<m1@example.com>/2",
      targetPath: "D:/下载/报告.pdf",
    });
    // 复制在 Rust 侧做，前端只是把目标路径告诉它。
    expect(await screen.findByTestId("attachment-saved")).toHaveTextContent(
      "D:/下载/报告.pdf",
    );
  });

  it("另存为时取消对话框就什么都不做", async () => {
    vi.mocked(saveFileDialog).mockResolvedValue(null);
    const backend = backendWith([
      makeAttachment({ downloaded: true, localPath: "C:/data/attachments/m1/abc" }),
    ]);
    const user = await openMail(backend);

    await click(user, await screen.findByTestId("attachment-save-as"));
    await settle();

    expect(backend.callsTo("save_attachment_as")).toHaveLength(0);
  });

  it("中文文件名照常显示（不转义、不截断）", async () => {
    await openMail(
      backendWith([makeAttachment({ filename: "2024 年度报销明细（重发）.xlsx" })]),
    );
    expect(
      await screen.findByText("2024 年度报销明细（重发）.xlsx"),
    ).toBeTruthy();
  });
});

describe("内联图片（cid）", () => {
  it("后端替换好的 data: 图片照常渲染，正文里的链接照常交给系统", async () => {
    await openMail(
      backendWith([], {
        messageBody: {
          messageId: "<m1@example.com>",
          textPlain: null,
          // 这是后端 `MessageBodyView::from_domain_with_images` 的输出形状：
          // `cid:` 已经被换成 `data:image/png;base64,...`。
          htmlSafe:
            '<p>见图 <img src="data:image/png;base64,iVBORw0KGgo=" alt="公司 logo"></p><p><a href="https://example.com/">官网</a></p>',
          charset: "utf-8",
        },
      }),
    );

    const image = document.querySelector("img");
    expect(image).not.toBeNull();
    expect(image?.getAttribute("src")).toMatch(/^data:image\/png;base64,/);
    expect(image?.getAttribute("alt")).toBe("公司 logo");
    // 不能有残留的 cid: 引用。
    expect(document.body.innerHTML).not.toMatch(/cid:/);
    expect(openUrl).not.toHaveBeenCalled();
  });

  it("内联图片不会被当成附件列出来", async () => {
    // 后端只把「当文件」的部件塞进 list_attachments；内联图走正文。
    await openMail(
      backendWith([], {
        messageBody: {
          messageId: "<m1@example.com>",
          textPlain: null,
          htmlSafe: '<p><img src="data:image/gif;base64,R0lGOD" alt=""></p>',
          charset: "utf-8",
        },
      }),
    );
    expect(screen.queryByTestId("message-attachments")).toBeNull();
  });
});
