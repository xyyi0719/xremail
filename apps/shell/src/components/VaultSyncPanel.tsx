import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CloudUpload, CloudDownload, ShieldCheck } from "lucide-react";

/**
 * 账户配置同步（WebDAV + 端到端加密）。
 *
 * ## 用户在做什么
 *
 * 「我在 A 机器配好 5 个邮箱，换到 B 机器同步一下，5 个邮箱直接能用。」
 *
 * * **上传**：把本机全部账户（含授权码）用主密码**加密**后传到 WebDAV。
 * * **下载**：从 WebDAV 取回密文，用同一个主密码解密，导入账户与授权码。
 *
 * ## 安全约束（在 UI 上要如实告诉用户）
 *
 * * 主密码**不上传、不保存**，只用来加解密；忘了它数据就解不开了。
 * * 上传的是**密文**，WebDAV 服务商（坚果云）看不到授权码。
 * * 本机已有的同邮箱账户**不会被覆盖**（导入时跳过）。
 */
const JIANGUO_BASE = "https://dav.jianguoyun.com/dav/";

interface UploadResult {
  accountCount: number;
  ciphertextBytes: number;
}
interface ImportResult {
  imported: number;
  skippedExisting: string[];
  withoutCredential: string[];
}

export default function VaultSyncPanel({ onImported }: { onImported?: () => void } = {}) {
  // 服务端固定坚果云，但保留可改（未来支持别的 WebDAV）。
  const [baseUrl, setBaseUrl] = useState(JIANGUO_BASE);
  const [username, setUsername] = useState("");
  const [appPassword, setAppPassword] = useState("");
  const [remoteDir, setRemoteDir] = useState("xremail");
  // 主密码：每次操作手动输入，用完即弃。
  const [passphrase, setPassphrase] = useState("");
  const [busy, setBusy] = useState<"upload" | "import" | null>(null);
  const [message, setMessage] = useState<{ kind: "ok" | "warn" | "err"; text: string } | null>(null);

  const canSubmit =
    baseUrl.trim() !== "" &&
    username.trim() !== "" &&
    appPassword.trim() !== "" &&
    passphrase.trim() !== "" &&
    busy === null;

  const params = () => ({
    baseUrl: baseUrl.trim(),
    username: username.trim(),
    appPassword,
    remoteDir: remoteDir.trim(),
  });

  const handleUpload = async () => {
    setBusy("upload");
    setMessage(null);
    try {
      const r = await invoke<UploadResult>("vault_upload", {
        params: params(),
        passphrase,
      });
      setMessage({
        kind: "ok",
        text: `已上传 ${r.accountCount} 个账户（密文 ${(r.ciphertextBytes / 1024).toFixed(1)} KB）。换设备后用同样的主密码「下载」即可恢复。`,
      });
    } catch (e) {
      setMessage({ kind: "err", text: String(e) });
    } finally {
      setBusy(null);
    }
  };

  const handleImport = async () => {
    setBusy("import");
    setMessage(null);
    try {
      const r = await invoke<ImportResult>("vault_import", {
        params: params(),
        passphrase,
      });
      const parts = [`已导入 ${r.imported} 个账户`];
      if (r.skippedExisting.length > 0) {
        parts.push(`跳过 ${r.skippedExisting.length} 个本机已存在的账户（${r.skippedExisting.join("、")}）`);
      }
      if (r.withoutCredential.length > 0) {
        parts.push(
          `其中 ${r.withoutCredential.join("、")} 的授权码没能写入本机钥匙串，请在账户设置里重新填写`,
        );
      }
      setMessage({
        kind: r.withoutCredential.length > 0 ? "warn" : "ok",
        text: parts.join("；") + "。",
      });
      onImported?.();
    } catch (e) {
      setMessage({ kind: "err", text: String(e) });
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="panel" data-testid="vault-sync-panel">
      <h2 className="panel-title">账户同步（WebDAV）</h2>
      <p className="panel-hint">
        把账户配置和授权码<strong>加密</strong>后存到坚果云，换设备时同步一下就能用。
        主密码不上传、不保存——<strong>忘了它数据就解不开</strong>；
        上传的是密文，坚果云看不到你的授权码。
      </p>

      <form className="account-form" onSubmit={(e) => e.preventDefault()}>
        <label className="field">
          <span>服务端地址</span>
          <input
            type="url"
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder={JIANGUO_BASE}
            spellCheck={false}
          />
        </label>
        <label className="field">
          <span>坚果云账号（邮箱）</span>
          <input
            type="email"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            placeholder="you@example.com"
            autoComplete="off"
          />
        </label>
        <label className="field">
          <span>应用密码</span>
          <input
            type="password"
            value={appPassword}
            onChange={(e) => setAppPassword(e.target.value)}
            placeholder="坚果云「第三方应用管理」生成，不是登录密码"
            autoComplete="off"
            spellCheck={false}
          />
        </label>
        <label className="field">
          <span>远端目录</span>
          <input
            type="text"
            value={remoteDir}
            onChange={(e) => setRemoteDir(e.target.value)}
            placeholder="xremail"
            spellCheck={false}
          />
        </label>
        <label className="field">
          <span>主密码</span>
          <input
            type="password"
            value={passphrase}
            onChange={(e) => setPassphrase(e.target.value)}
            placeholder="用于加解密同步数据，请牢记"
            autoComplete="off"
            spellCheck={false}
          />
        </label>

        <div className="vault-actions">
          <button
            type="button"
            className="btn-primary"
            onClick={() => void handleUpload()}
            disabled={!canSubmit}
          >
            <CloudUpload size={16} strokeWidth={1.9} />
            {busy === "upload" ? "上传中…" : "上传到坚果云"}
          </button>
          <button
            type="button"
            className="btn-secondary"
            onClick={() => void handleImport()}
            disabled={!canSubmit}
          >
            <CloudDownload size={16} strokeWidth={1.9} />
            {busy === "import" ? "下载中…" : "从这里下载"}
          </button>
        </div>
      </form>

      {message && (
        <p
          className={
            message.kind === "ok"
              ? "vault-message vault-ok"
              : message.kind === "warn"
                ? "vault-message vault-warn"
                : "vault-message vault-err"
          }
          data-testid="vault-message"
        >
          {message.kind === "ok" && <ShieldCheck size={14} strokeWidth={2} />}
          {message.text}
        </p>
      )}
    </section>
  );
}
