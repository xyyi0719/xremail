import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  Account,
  NewAccountRequest,
  ProviderKind,
  SmtpAuth,
  SyncMode,
  TlsMode,
} from "../types";
import {
  PROVIDER_LABELS,
  PROVIDER_PRESETS,
  type ProviderPreset,
  SMTP_AUTH_LABELS,
  SYNC_MODE_LABELS,
  TLS_LABELS,
} from "../types";

/** 表单的初始值。 */
// **初始就要填好默认服务商（163）的服务器参数**。
//
// 之前这里是空串，而预填只在「切换下拉」时触发——默认就是 163，用户从没切过，
// 于是 IMAP/SMTP 地址永远是空的（2026-11-12 用户反馈的真 bug）。
// 直接从 PROVIDER_PRESETS 取，保证与切换逻辑同源。
const DEFAULT_PRESET = PROVIDER_PRESETS.netease_163!;

const EMPTY: NewAccountRequest = {
  email: "",
  provider: "netease_163",
  displayName: "",
  imapHost: DEFAULT_PRESET.imapHost,
  imapPort: DEFAULT_PRESET.imapPort,
  imapTls: DEFAULT_PRESET.imapTls,
  smtpHost: DEFAULT_PRESET.smtpHost,
  smtpPort: DEFAULT_PRESET.smtpPort,
  smtpTls: DEFAULT_PRESET.smtpTls,
  smtpAuth: DEFAULT_PRESET.smtpAuth,
  username: "",
  syncMode: "poll",
  pollIntervalSecs: 300,
};

/**
 * 新增账户表单。
 *
 * **服务器地址按品牌自动填，但只填实测过的**：163 / 企业邮 / QQ / 沃 的参数
 * 已在 R01/R02 里实测闭环（记于 docs/PROTOCOLS.md），选中服务商即预填。
 * 126 / Gmail / Exchange / 自定义**没有可信预设**，仍然让用户手填——
 * 宁可多填一行，也不拿看起来合理的值糊弄。
 *
 * 用户手改过的字段不会被切换服务商覆盖（见 `applyProvider`）。
 */
function AccountForm({ onCreated }: { onCreated?: () => void } = {}) {
  const [form, setForm] = useState<NewAccountRequest>(EMPTY);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);
  // 授权码与账户**一起填**：以前要「先建账户 → 再去卡片里存授权码」两步，
  // 用户建完账户发现还不能收发，得再找一次入口（2026-11-12 用户反馈）。
  // 现在创建成功后立刻调 save_credential 存进平台钥匙串。
  const [credential, setCredential] = useState("");

  const set = <K extends keyof NewAccountRequest>(
    key: K,
    value: NewAccountRequest[K],
  ) => {
    setForm((prev) => ({ ...prev, [key]: value }));
  };

  /**
   * 输入邮箱时，**顺带填好用户名与显示名**（用户通常不用改）。
   *
   * 规则：
   * - **用户名**默认与邮箱相同（163/QQ 都是拿完整邮箱登录；QQ 的纯数字账号
   *   也由后端按域名补齐，与真实用例一致）。
   * - **显示名**取 `@` 前的部分，作为账户在侧栏的标签。
   * - **只在用户没单独改过时跟随**：如果用户手工把显示名改成"工作邮箱"，
   *   之后再来改邮箱，不能把他的自定义名覆盖掉。判据是「当前值是否仍等于
   *   上一个邮箱推导出来的值」。
   */
  const applyEmail = (next: string) => {
    setForm((current) => {
      const local = next.includes("@") ? next.split("@")[0]! : next;
      const prevLocal = current.email.includes("@")
        ? current.email.split("@")[0]!
        : current.email;

      // 用户名：空着、或仍等于上一个邮箱 → 跟着走。
      const usernameUntouched =
        current.username === "" || current.username === current.email;
      // 显示名：空着、或仍等于上一个邮箱的 local part → 跟着走。
      const displayUntouched =
        (current.displayName ?? "") === "" || (current.displayName ?? "") === prevLocal;

      return {
        ...current,
        email: next,
        username: usernameUntouched ? next : current.username,
        displayName: displayUntouched ? local : (current.displayName ?? ""),
      };
    });
  };

  /**
   * 选服务商时把**实测过的**服务器参数填进去。
   *
   * 语义要点：
   * - **只填还有空的字段**（或该字段本来就等于上一个预设），不覆盖用户手改过
   *   的值——用户从 163 改成 QQ、又想保留自己填的端口，不该被抢回去。
   * - 选到**没有预设**的服务商（126 / Gmail / Exchange / 其它）时，**不清空**
   *   已填内容，只是不再自动改——用户可能只改了个标签。
   * - `generic` 是「自定义 IMAP」，本来就要手填，行为与上一条相同。
   */
  const applyProvider = (next: ProviderKind) => {
    const previousPreset = PROVIDER_PRESETS[form.provider];
    const preset = PROVIDER_PRESETS[next];

    setForm((current) => {
      const base = { ...current, provider: next };
      if (!preset) {
        return base;
      }
      // 「用户没动过」= 当前值等于上一个预设的值（或为空）。只在这种情况下覆盖。
      const untouched = (key: keyof ProviderPreset, currentValue: unknown) => {
        const prev = previousPreset?.[key];
        return prev === undefined || currentValue === prev || currentValue === "";
      };
      return {
        ...base,
        imapHost: untouched("imapHost", current.imapHost) ? preset.imapHost : current.imapHost,
        imapPort: untouched("imapPort", current.imapPort) ? preset.imapPort : current.imapPort,
        imapTls: untouched("imapTls", current.imapTls) ? preset.imapTls : current.imapTls,
        smtpHost: untouched("smtpHost", current.smtpHost) ? preset.smtpHost : current.smtpHost,
        smtpPort: untouched("smtpPort", current.smtpPort) ? preset.smtpPort : current.smtpPort,
        smtpTls: untouched("smtpTls", current.smtpTls) ? preset.smtpTls : current.smtpTls,
        smtpAuth: untouched("smtpAuth", current.smtpAuth) ? preset.smtpAuth : current.smtpAuth,
      };
    });
  };

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setDone(null);

    try {
      // **先建账户，再存凭据**：两者是两个 IPC（账户落库 / 凭据进钥匙串），
      // 顺序不能反——`save_credential` 会校验账户是否存在。
      const account = await invoke<Account>("create_account", { request: form });

      if (credential.trim() !== "") {
        try {
          await invoke("save_credential", {
            accountId: account.id,
            credential: credential.trim(),
          });
        } catch (credentialError) {
          // 账户已经建好了，凭据没存进去——**不能假装成功**。
          // 库里账户还在，但 `secretRef` 为空，调用方必须知道"还差一步"。
          setForm(EMPTY);
          setCredential("");
          setError(
            `账户已创建，但授权码没能存进系统凭据库：${String(credentialError)}。` +
              `请在设置的账户卡片里重新填写授权码。`,
          );
          return;
        }
      }

      setForm(EMPTY);
      setCredential("");
      // **必须通知外层刷新账户列表**：否则账户卡片显示的是创建前的旧快照，
      // 会写着「凭据未落盘」，和刚刚的「已存入系统凭据库」自相矛盾，
      // 用户会以为没存上而重复操作（2026-11-12 Android 实测发现）。
      onCreated?.();

      setDone(
        credential.trim() === ""
          ? "已添加。尚未填写授权码，暂不能收发邮件。"
          : "已添加，授权码已存入系统凭据库，可以开始收发邮件了。",
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="panel">
      <h2 className="panel-title">添加账户</h2>
      <p className="panel-hint">
        服务商只是标记，服务器地址需要自行填写——各服务商的参数要实测确认，
        暂不按品牌推定。
      </p>

      <form className="account-form" onSubmit={handleSubmit}>
        <label className="field">
          <span>邮箱</span>
          <input
            type="email"
            value={form.email}
            onChange={(e) => applyEmail(e.target.value)}
            placeholder="user@example.com"
            required
          />
        </label>

        {/* 授权码紧跟邮箱：建账户时要用的两样东西放在一起，一次填完。 */}
        <label className="field">
          <span>授权码</span>
          <input
            type="password"
            value={credential}
            onChange={(e) => setCredential(e.target.value)}
            placeholder="邮箱的客户端授权码（不是登录密码）"
            autoComplete="off"
            spellCheck={false}
          />
        </label>

        <label className="field">
          <span>服务商</span>
          <select
            value={form.provider}
            onChange={(e) => applyProvider(e.target.value as ProviderKind)}
          >
            {(Object.keys(PROVIDER_LABELS) as ProviderKind[]).map((key) => (
              <option key={key} value={key}>
                {PROVIDER_LABELS[key]}
              </option>
            ))}
          </select>
        </label>

        <label className="field">
          <span>显示名（可选）</span>
          <input
            type="text"
            value={form.displayName ?? ""}
            onChange={(e) => set("displayName", e.target.value)}
          />
        </label>

        <label className="field">
          <span>用户名</span>
          <input
            type="text"
            value={form.username}
            onChange={(e) => set("username", e.target.value)}
            placeholder="通常是完整邮箱地址"
            required
          />
        </label>

        <label className="field">
          <span>IMAP 服务器</span>
          <input
            type="text"
            value={form.imapHost}
            onChange={(e) => set("imapHost", e.target.value)}
            placeholder="imap.example.com"
            required
          />
        </label>

        <label className="field field-narrow">
          <span>IMAP 端口</span>
          <input
            type="number"
            value={form.imapPort}
            onChange={(e) => set("imapPort", Number(e.target.value))}
            min={1}
            max={65535}
            required
          />
        </label>

        <label className="field field-narrow">
          <span>IMAP 加密</span>
          <select
            value={form.imapTls}
            onChange={(e) => set("imapTls", e.target.value as TlsMode)}
          >
            {(Object.keys(TLS_LABELS) as TlsMode[]).map((key) => (
              <option key={key} value={key}>
                {TLS_LABELS[key]}
              </option>
            ))}
          </select>
        </label>

        <label className="field">
          <span>SMTP 服务器</span>
          <input
            type="text"
            value={form.smtpHost}
            onChange={(e) => set("smtpHost", e.target.value)}
            placeholder="smtp.example.com"
            required
          />
        </label>

        <label className="field field-narrow">
          <span>SMTP 端口</span>
          <input
            type="number"
            value={form.smtpPort}
            onChange={(e) => set("smtpPort", Number(e.target.value))}
            min={1}
            max={65535}
            required
          />
        </label>

        <label className="field field-narrow">
          <span>SMTP 加密</span>
          <select
            value={form.smtpTls}
            onChange={(e) => set("smtpTls", e.target.value as TlsMode)}
          >
            {(Object.keys(TLS_LABELS) as TlsMode[]).map((key) => (
              <option key={key} value={key}>
                {TLS_LABELS[key]}
              </option>
            ))}
          </select>
        </label>

        <label className="field field-narrow">
          <span>SMTP 认证</span>
          <select
            value={form.smtpAuth}
            onChange={(e) => set("smtpAuth", e.target.value as SmtpAuth)}
          >
            {(Object.keys(SMTP_AUTH_LABELS) as SmtpAuth[]).map((key) => (
              <option key={key} value={key}>
                {SMTP_AUTH_LABELS[key]}
              </option>
            ))}
          </select>
        </label>

        <label className="field field-narrow">
          <span>同步方式</span>
          <select
            value={form.syncMode}
            onChange={(e) => set("syncMode", e.target.value as SyncMode)}
          >
            {(Object.keys(SYNC_MODE_LABELS) as SyncMode[]).map((key) => (
              <option key={key} value={key}>
                {SYNC_MODE_LABELS[key]}
              </option>
            ))}
          </select>
        </label>

        <label className="field field-narrow">
          <span>轮询间隔（秒）</span>
          <input
            type="number"
            value={form.pollIntervalSecs}
            onChange={(e) => set("pollIntervalSecs", Number(e.target.value))}
            min={1}
            required
          />
        </label>

        <div className="form-actions">
          <button type="submit" disabled={busy}>
            {busy ? "保存中…" : "添加账户"}
          </button>
        </div>
      </form>

      {error && <p className="app-error">{error}</p>}
      {done && <p className="app-ok">{done}</p>}
    </section>
  );
}

export default AccountForm;
