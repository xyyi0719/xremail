import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Account, DkimConfig, Notice } from "../types";
import "./DkimPanel.css";

interface Props {
  account: Account;
  onNotice: (notice: Notice) => void;
}

/**
 * 账户的 DKIM 配置面板（对应命令 `get_dkim_config` / `set_dkim_config` /
 * `clear_dkim_config`）。
 *
 * ## 私钥只进不出
 *
 * 私钥只从**这个输入框**流向平台钥匙串，来回各一次就没了：
 *
 * * 保存成功后立刻清空输入框，不让明文继续待在内存里；
 * * `get_dkim_config` 的返回值里**没有**私钥（只有选择器、域名和公钥），
 *   所以这里也不可能把它显示出来——已经配置过时只显示
 *   「已配置 selector@domain」和贴进 DNS 的记录。
 *
 * 这与授权码的处理口径一致（SECURITY §2.1）：密钥进操作系统的钥匙串，
 * 不进应用自己的数据库。
 */
function DkimPanel({ account, onNotice }: Props) {
  const [config, setConfig] = useState<DkimConfig | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [selector, setSelector] = useState("");
  const [domain, setDomain] = useState("");
  const [privateKeyPem, setPrivateKeyPem] = useState("");
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const current = await invoke<DkimConfig | null>("get_dkim_config", {
        accountId: account.id,
      });
      setConfig(current);
      if (current) {
        setSelector(current.selector);
        setDomain(current.domain);
      }
    } catch (e) {
      onNotice({ kind: "error", text: `读取 DKIM 配置失败：${String(e)}` });
    } finally {
      setLoaded(true);
    }
  }, [account.id, onNotice]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    setBusy(true);
    try {
      const saved = await invoke<DkimConfig>("set_dkim_config", {
        accountId: account.id,
        selector: selector.trim(),
        domain: domain.trim(),
        privateKeyPem,
      });
      setConfig(saved);
      // 私钥已经交给系统钥匙串了，别让它继续待在组件里。
      setPrivateKeyPem("");
      onNotice({
        kind: "ok",
        text: `DKIM 已配置：${saved.selector}@${saved.domain}`,
      });
    } catch (e) {
      onNotice({ kind: "error", text: `保存 DKIM 配置失败：${String(e)}` });
    } finally {
      setBusy(false);
    }
  };

  const clear = async () => {
    setBusy(true);
    try {
      await invoke<boolean>("clear_dkim_config", { accountId: account.id });
      setConfig(null);
      setSelector("");
      setDomain("");
      setPrivateKeyPem("");
      onNotice({ kind: "info", text: "已清除 DKIM 配置，发信不再签名" });
    } catch (e) {
      onNotice({ kind: "error", text: `清除 DKIM 配置失败：${String(e)}` });
    } finally {
      setBusy(false);
    }
  };

  // 首次加载完成之前什么都不说，免得界面先闪一下「未配置」。
  if (!loaded) {
    return null;
  }

  return (
    <div className="dkim-panel">
      <div className="dkim-status" data-testid="dkim-status">
        {config === null
          ? "DKIM：未配置（发出的邮件不带签名）"
          : `DKIM：已配置 ${config.selector}@${config.domain}（${config.headerAlg}）`}
      </div>

      {config !== null && config.dnsName !== null && (
        <div className="dkim-dns" data-testid="dkim-dns">
          在 DNS 里添加 TXT 记录 <code>{config.dnsName}</code>：
          <code className="dkim-record">{config.dnsRecord}</code>
        </div>
      )}

      <label className="dkim-label" htmlFor={`dkim-selector-${account.id}`}>
        选择器（DNS 记录名里 <code>&lt;selector&gt;._domainkey</code> 的那段）
      </label>
      <input
        id={`dkim-selector-${account.id}`}
        type="text"
        className="sync-input"
        value={selector}
        autoComplete="off"
        onChange={(e) => setSelector(e.target.value)}
        placeholder="例如 default"
      />

      <label className="dkim-label" htmlFor={`dkim-domain-${account.id}`}>
        签名域名（<code>d=</code>）
      </label>
      <input
        id={`dkim-domain-${account.id}`}
        type="text"
        className="sync-input"
        value={domain}
        autoComplete="off"
        onChange={(e) => setDomain(e.target.value)}
        placeholder="例如 example.com"
      />

      <label className="dkim-label" htmlFor={`dkim-key-${account.id}`}>
        私钥 PEM（PKCS#8 或 PKCS#1，未加密；只会存进系统钥匙串）
      </label>
      {/* 私钥用 password 输入框：屏幕上不显示明文，也不参与浏览器自动填充。 */}
      <input
        id={`dkim-key-${account.id}`}
        type="password"
        className="sync-input dkim-key"
        value={privateKeyPem}
        autoComplete="off"
        spellCheck={false}
        onChange={(e) => setPrivateKeyPem(e.target.value)}
        placeholder="-----BEGIN PRIVATE KEY-----"
      />

      <div className="sync-actions">
        <button
          type="button"
          className="account-save-dkim"
          disabled={
            busy ||
            selector.trim() === "" ||
            domain.trim() === "" ||
            (privateKeyPem.trim() === "" && config === null)
          }
          onClick={() => void save()}
        >
          {busy ? "保存中…" : config === null ? "保存 DKIM 配置" : "更新 DKIM 配置"}
        </button>
        {config !== null && (
          <button
            type="button"
            className="account-clear-dkim"
            disabled={busy}
            onClick={() => void clear()}
          >
            清除 DKIM 配置
          </button>
        )}
      </div>
      {config !== null && (
        <span className="sync-hint">
          只想换选择器或域名？留空私钥直接保存即可，钥匙串里的私钥不会被改动。
        </span>
      )}
    </div>
  );
}

export default DkimPanel;
