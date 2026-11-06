import type { Account, Folder, Message, MessageBody } from "../src/types";

/**
 * E2E 用的数据样板。
 *
 * 和 `src/test/fixtures.ts` 是同一套形状，但**故意不复用**：单元测试的样板
 * 随时可以为了某个单元用例改动，E2E 是「用户看得到的那一层」的契约，
 * 数字和文案要自己盯住。两边各改各的，互不牵连。
 */

/** 账户邮箱（也是账户列表里那个按钮的可访问名）。 */
export const ACCOUNT_EMAIL = "me@example.com";
export const ACCOUNT_ID = "acc-1";
/** 收件箱。 */
export const INBOX_ID = "f-inbox";
export const INBOX_NAME = "收件箱";

export function makeAccount(): Account {
  return {
    id: ACCOUNT_ID,
    email: ACCOUNT_EMAIL,
    provider: "netease_163",
    displayName: null,
    color: null,
    authKind: "password",
    server: {
      imapHost: "imap.163.com",
      imapPort: 993,
      imapTls: "implicit",
      smtpHost: "smtp.163.com",
      smtpPort: 465,
      smtpTls: "implicit",
      smtpAuth: "login",
    },
    username: ACCOUNT_EMAIL,
    secretRef: "cred-1",
    oauthClientId: null,
    syncMode: "poll",
    pollIntervalSecs: 300,
    signatureHtml: null,
    createdAt: 1_700_000_000,
    updatedAt: 1_700_000_000,
  };
}

export function makeFolders(): Folder[] {
  return [
    {
      id: INBOX_ID,
      name: INBOX_NAME,
      rawName: "INBOX",
      delimiter: "/",
      uidvalidity: 1,
      lastUid: 102,
      lastSyncAt: 1_700_000_000,
    },
  ];
}

/** 两封邮件：uid 大的（102）在列表上面。 */
export function makeMessages(): Message[] {
  const base: Omit<Message, "id" | "uid" | "subject" | "from" | "snippet"> = {
    accountId: ACCOUNT_ID,
    folderId: INBOX_ID,
    uidvalidity: 1,
    messageId: null,
    to: [ACCOUNT_EMAIL],
    cc: [],
    bcc: [],
    dateSent: 1_700_000_000,
    internalDate: 1_700_000_000,
    flags: 0,
    size: 256,
    hasAttachment: false,
    createdAt: 1_700_000_000,
    updatedAt: 1_700_000_000,
  };
  return [
    {
      ...base,
      id: "m-older",
      uid: 101,
      messageId: "<older@example.com>",
      subject: "上周的项目周报",
      snippet: "这里是周报摘要",
      from: ["reports@example.com"],
    },
    {
      ...base,
      id: "m-newer",
      uid: 102,
      messageId: "<newer@example.com>",
      subject: "季度预算评审安排",
      snippet: "周三下午两点",
      from: ["boss@example.com"],
    },
  ];
}

/** 打开「季度预算评审安排」时后端返回的正文（纯文本）。 */
export function makeMessageBody(): MessageBody {
  return {
    messageId: "<newer@example.com>",
    textPlain: "评审安排在周三下午两点，地点是 3 楼会议室。",
    htmlSafe: null,
    charset: "utf-8",
  };
}
