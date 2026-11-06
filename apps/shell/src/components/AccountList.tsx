import { invoke } from "@tauri-apps/api/core";
import type { Account, Notice, SyncResult } from "../types";
import { PROVIDER_LABELS, SYNC_MODE_LABELS, TLS_LABELS } from "../types";
import SyncPanel from "./SyncPanel";
import DkimPanel from "./DkimPanel";

interface Props {
  accounts: Account[];
  onSelect: (account: Account) => void;
  onDelete: (id: string) => void;
  onSynced: (result: SyncResult) => void;
  onError: (message: string) => void;
  onNotice: (notice: Notice) => void;
  /** 账户信息变了（凭据存进 / 删掉）之后重新拉一遍账户列表。 */
  onChanged: () => void;
}

/** 账户列表。数据来自 SQLite，不是硬编码。 */
function AccountList({
  accounts,
  onSelect,
  onDelete,
  onSynced,
  onError,
  onNotice,
  onChanged,
}: Props) {
  if (accounts.length === 0) {
    return (
      <section className="panel">
        <h2 className="panel-title">账户</h2>
        <p className="panel-empty">还没有账户。用上面的表单添加一个。</p>
      </section>
    );
  }

  /**
   * 从系统凭据库里删掉这个账户的授权码。
   *
   * 删完要重新拉账户列表：`secretRef` 变了，「凭据已保存到系统」
   * 得变成「凭据未落盘」。
   */
  const forget = async (account: Account) => {
    try {
      const removed = await invoke<boolean>("forget_credential", {
        accountId: account.id,
      });
      onNotice(
        removed
          ? { kind: "ok", text: "已从系统凭据库移除该授权码" }
          : { kind: "info", text: "系统凭据库里本来就没有这个授权码" },
      );
      onChanged();
    } catch (e) {
      onNotice({ kind: "error", text: `忘记凭据失败：${String(e)}` });
    }
  };

  return (
    <section className="panel">
      <h2 className="panel-title">账户（{accounts.length}）</h2>
      <ul className="account-list">
        {accounts.map((account) => (
          <li key={account.id} className="account-item" data-testid="account-item">
            <div className="account-main">
              <button
                type="button"
                className="account-email"
                onClick={() => onSelect(account)}
              >
                {account.email}
              </button>
              <span className="account-tag">
                {PROVIDER_LABELS[account.provider] ?? account.provider}
              </span>
              {account.displayName && (
                <span className="account-tag account-tag-muted">
                  {account.displayName}
                </span>
              )}
            </div>
            <div className="account-detail">
              IMAP {account.server.imapHost}:{account.server.imapPort}（
              {TLS_LABELS[account.server.imapTls]}） · SMTP{" "}
              {account.server.smtpHost}:{account.server.smtpPort}（
              {TLS_LABELS[account.server.smtpTls]}） ·{" "}
              {SYNC_MODE_LABELS[account.syncMode]}
            </div>
            <div className="account-detail account-detail-muted">
              用户名 {account.username}
              {account.secretRef === null
                ? " · 凭据未落盘"
                : " · 凭据已保存到系统"}
            </div>
            <div className="account-actions">
              <SyncPanel
                account={account}
                onSynced={onSynced}
                onError={onError}
                onNotice={onNotice}
                onChanged={onChanged}
              />
              {account.secretRef !== null && (
                <button
                  type="button"
                  className="account-forget"
                  onClick={() => void forget(account)}
                >
                  忘记凭据
                </button>
              )}
              <button
                type="button"
                className="account-delete"
                onClick={() => onDelete(account.id)}
              >
                删除
              </button>
            </div>
            {/* DKIM 会改变「发出的字节」，所以单独一块、默认折叠的内容放在
                账户操作区下面；未配置时它只显示一行「未配置」。 */}
            <DkimPanel account={account} onNotice={onNotice} />
          </li>
        ))}
      </ul>
    </section>
  );
}

export default AccountList;
