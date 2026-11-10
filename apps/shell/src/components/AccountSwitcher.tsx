import type { Account } from "../types";

/**
 * 左栏顶部的**账户切换器**：只显示邮箱 + 是否选中，点一下切账户。
 *
 * ## 为什么不直接用 `AccountList`
 *
 * `AccountList` 是一张**完整的管理卡**：邮箱、服务商标签、IMAP/SMTP 地址、
 * 授权码输入框、同步按钮、忘记凭据、删除、DKIM 表单。它在单页布局里是合理的，
 * 但塞进三栏的左栏（11–14rem）之后，光它就占掉一屏——「文件夹」被挤到视口外，
 * 每次都要先滚一下才能换文件夹（2026-11-10 实测截图）。
 *
 * Gmail 的左栏也只有「写信 + 文件夹」，账户管理在设置里。这里照做：
 * **左栏只切换账户**，管理动作全部收进上面的「账户与设置」。
 */
export default function AccountSwitcher({
  accounts,
  selectedId,
  onSelect,
}: {
  accounts: Account[];
  selectedId: string | null;
  onSelect: (account: Account) => void;
}) {
  if (accounts.length === 0) {
    return null;
  }

  return (
    <nav className="account-switcher" aria-label="账户切换">
      {accounts.map((account) => (
        <button
          key={account.id}
          type="button"
          className={
            account.id === selectedId
              ? "account-switcher-item account-switcher-item-active"
              : "account-switcher-item"
          }
          title={account.email}
          aria-current={account.id === selectedId ? "true" : undefined}
          aria-label={`切换到 ${account.email}`}
          onClick={() => onSelect(account)}
        >
          {account.email}
        </button>
      ))}
    </nav>
  );
}
