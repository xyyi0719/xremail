//! 账户配置同步服务：本地账户 ↔ 加密载荷 ↔ WebDAV。
//!
//! ## 职责
//!
//! * **导出**：读本地全部账户 + 从钥匙串取授权码 → 组装 [`VaultPayload`] →
//!   用主密码加密 → 上传 WebDAV。
//! * **导入**：从 WebDAV 下载密文 → 主密码解密 → 把账户写进本地库、
//!   把授权码写进本机钥匙串。
//!
//! ## 安全铁律
//!
//! 1. **授权码只以密文形式离开设备**。`VaultPayload` 明文只在内存里，加密后立即丢弃。
//! 2. **主密码不落盘**。每次同步由用户输入，用完即弃（不存钥匙串、不存数据库）。
//! 3. 导入时若本机已有同邮箱账户：**跳过并报告**，不静默覆盖用户本机的配置。
//!
//! ## 冲突策略（本版）
//!
//! 本版是**单文件全量覆盖 + 导入时按邮箱去重**：
//! * 导出总是覆盖远端那份（用户手动点了「上传」，说明以本机为准）；
//! * 导入遇到同邮箱则跳过，不覆盖。
//!
//! 这满足「换设备同步一次就能用」的主诉求，且**不会丢数据**（最坏情况是
//! 用户手动删了再导）。真正的双向合并留给后续（L2 完整版）。

use xremail_api::SecretValue;
use xremail_core::model::{Account, ProviderKind, SmtpAuth, SyncMode, TlsMode};
use xremail_core::{AppError, AppResult};
use xremail_vault::webdav::{WebDavClient, WebDavConfig};
use xremail_vault::{VaultAccount, VaultPayload};

use crate::service::AccountService;
use crate::service::credential::CredentialService;

/// 远端文件名。
pub const REMOTE_FILENAME: &str = "xremail-vault.bin";

/// 同步结果（回给前端展示）。
#[derive(Debug, Clone)]
pub struct UploadSummary {
    /// 上传了多少个账户。
    pub account_count: usize,
    /// 密文字节数。
    pub ciphertext_bytes: usize,
}

/// 导入结果。
#[derive(Debug, Clone)]
pub struct ImportSummary {
    /// 新导入的账户数。
    pub imported: usize,
    /// 因本机已存在同邮箱而跳过的账户（邮箱列表）。
    pub skipped_existing: Vec<String>,
    /// 因本机无钥匙串适配等原因导致授权码没写进去的账户（邮箱列表）。
    /// 这些账户仍会被创建，但用户需在设置里重新填授权码。
    pub without_credential: Vec<String>,
}

/// 同步服务。
pub struct SyncVaultService {
    accounts: AccountService,
    credentials: CredentialService,
}

impl SyncVaultService {
    /// 构造。
    pub fn new(accounts: AccountService) -> Self {
        Self {
            credentials: CredentialService::new(accounts.clone()),
            accounts,
        }
    }

    /// 把本地全部账户（含授权码）加密上传。
    ///
    /// `device_id` 仅用于载荷元信息（排查用）。
    pub async fn upload(
        &self,
        dav: &WebDavConfig,
        passphrase: &str,
        device_id: &str,
        now_ms: i64,
    ) -> AppResult<UploadSummary> {
        if passphrase.trim().is_empty() {
            return Err(AppError::Invalid("主密码不能为空".into()));
        }

        let accounts = self.accounts.list().await?;

        // 逐个从钥匙串取授权码。取不到的账户**跳过**——没有授权码的账户
        // 同步过去也没用（新设备上照样要重填），不如不污染载荷。
        let mut vault_accounts = Vec::with_capacity(accounts.len());
        for account in &accounts {
            let credential = match self.credentials.resolve_credential(account).await? {
                Some(secret) => String::from_utf8_lossy(secret.as_bytes()).to_string(),
                None => {
                    // 没有授权码：跳过。**不报错**——用户可能有些账户就是不想同步。
                    tracing::debug!(account = %account.email, "无授权码，跳过同步该账户");
                    continue;
                }
            };
            vault_accounts.push(to_vault_account(account, credential));
        }

        if vault_accounts.is_empty() {
            return Err(AppError::Invalid("没有任何带授权码的账户可同步".into()));
        }

        let payload = VaultPayload::new(vault_accounts.clone(), now_ms, device_id);
        let ciphertext = xremail_vault::seal_payload(
            &payload,
            passphrase.as_bytes(),
            xremail_vault::KdfParams::DEFAULT,
        )
        .map_err(|e| AppError::Storage(e.to_string()))?;

        let dav_owned = dav.clone();
        let bytes = ciphertext.clone();
        // ureq 是阻塞的：放到阻塞线程池，别堵 tokio 的 worker。
        tokio::task::spawn_blocking(move || -> Result<(), xremail_vault::webdav::WebDavError> {
            let client = WebDavClient::new(dav_owned)?;
            client.ensure_dir()?;
            client.put(REMOTE_FILENAME, &bytes)?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Storage(format!("上传任务失败：{e}")))?
        .map_err(|e| AppError::Storage(e.to_string()))?;

        Ok(UploadSummary {
            account_count: vault_accounts.len(),
            ciphertext_bytes: ciphertext.len(),
        })
    }

    /// 从 WebDAV 下载并导入。
    pub async fn import(
        &self,
        dav: &WebDavConfig,
        passphrase: &str,
        now_ms: i64,
    ) -> AppResult<ImportSummary> {
        if passphrase.trim().is_empty() {
            return Err(AppError::Invalid("主密码不能为空".into()));
        }

        let dav_owned = dav.clone();
        let ciphertext = tokio::task::spawn_blocking(
            move || -> Result<Option<Vec<u8>>, xremail_vault::webdav::WebDavError> {
                let client = WebDavClient::new(dav_owned)?;
                client.get(REMOTE_FILENAME)
            },
        )
        .await
        .map_err(|e| AppError::Storage(format!("下载任务失败：{e}")))?
        .map_err(|e| AppError::Storage(e.to_string()))?;

        let Some(ciphertext) = ciphertext else {
            return Err(AppError::Invalid(
                "远端没有找到同步数据（可能还没上传过）".into(),
            ));
        };

        let payload = xremail_vault::open_payload(&ciphertext, passphrase.as_bytes()).map_err(
            |e| match e {
                xremail_vault::VaultError::Open => {
                    AppError::Invalid("主密码不对，或同步数据已损坏".into())
                }
                other => AppError::Invalid(other.to_string()),
            },
        )?;

        let existing = self.accounts.list().await?;
        let existing_emails: Vec<String> = existing.iter().map(|a| a.email.clone()).collect();

        let mut imported = 0usize;
        let mut skipped_existing = Vec::new();
        let mut without_credential = Vec::new();

        for vault_account in &payload.accounts {
            // 本机已有同邮箱 → 跳过（不覆盖用户本机配置）。
            if existing_emails
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&vault_account.email))
            {
                skipped_existing.push(vault_account.email.clone());
                continue;
            }

            let request = vault_to_new_account_request(vault_account)?;
            let created = self.accounts.create(request).await?;
            imported += 1;

            // 把授权码写进本机钥匙串。写不进去**不阻断**：账户已经建好，
            // 用户只需在设置里补填一次——比整个导入失败要好。
            match self
                .credentials
                .save_credential(
                    &created.id,
                    SecretValue::from_str_lossless(&vault_account.credential),
                )
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(account = %vault_account.email, %error, "授权码未能写入本机钥匙串");
                    without_credential.push(vault_account.email.clone());
                }
            }
        }

        let _ = now_ms;
        Ok(ImportSummary {
            imported,
            skipped_existing,
            without_credential,
        })
    }
}

/// 本地 `Account` → 载荷条目（带上明文授权码）。
fn to_vault_account(account: &Account, credential: String) -> VaultAccount {
    VaultAccount {
        email: account.email.clone(),
        provider: provider_to_str(account.provider).to_string(),
        display_name: account.display_name.clone(),
        imap_host: account.server.imap_host.clone(),
        imap_port: account.server.imap_port,
        imap_tls: tls_to_str(account.server.imap_tls).to_string(),
        smtp_host: account.server.smtp_host.clone(),
        smtp_port: account.server.smtp_port,
        smtp_tls: tls_to_str(account.server.smtp_tls).to_string(),
        smtp_auth: smtp_auth_to_str(account.server.smtp_auth).to_string(),
        username: account.username.clone(),
        sync_mode: sync_mode_to_str(account.sync_mode).to_string(),
        poll_interval_secs: account.poll_interval_secs,
        signature_html: account.signature_html.clone(),
        credential,
    }
}

/// 载荷条目 → 新建账户请求。
fn vault_to_new_account_request(
    self_v: &VaultAccount,
) -> AppResult<xremail_api::account::NewAccountRequest> {
    Ok(xremail_api::account::NewAccountRequest {
        email: self_v.email.clone(),
        provider: provider_from_str(&self_v.provider),
        display_name: self_v.display_name.clone(),
        imap_host: self_v.imap_host.clone(),
        imap_port: self_v.imap_port,
        imap_tls: tls_from_str(&self_v.imap_tls),
        smtp_host: self_v.smtp_host.clone(),
        smtp_port: self_v.smtp_port,
        smtp_tls: tls_from_str(&self_v.smtp_tls),
        smtp_auth: smtp_auth_from_str(&self_v.smtp_auth),
        username: self_v.username.clone(),
        sync_mode: sync_mode_from_str(&self_v.sync_mode),
        poll_interval_secs: self_v.poll_interval_secs,
    })
}

// ---- 枚举 ↔ 字符串（与 core 的 serde 表现保持一致）----

fn provider_to_str(p: ProviderKind) -> &'static str {
    match serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
    {
        Some(s) => Box::leak(s.into_boxed_str()),
        None => "generic",
    }
}

fn provider_from_str(s: &str) -> ProviderKind {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(ProviderKind::Generic)
}

fn tls_to_str(t: TlsMode) -> &'static str {
    match t {
        TlsMode::None => "none",
        TlsMode::Starttls => "starttls",
        TlsMode::Implicit => "implicit",
    }
}

fn tls_from_str(s: &str) -> TlsMode {
    match s {
        "starttls" => TlsMode::Starttls,
        "implicit" => TlsMode::Implicit,
        _ => TlsMode::None,
    }
}

fn smtp_auth_to_str(a: SmtpAuth) -> &'static str {
    match a {
        SmtpAuth::None => "none",
        SmtpAuth::Plain => "plain",
        SmtpAuth::Login => "login",
        SmtpAuth::XOAuth2 => "xoauth2",
    }
}

fn smtp_auth_from_str(s: &str) -> SmtpAuth {
    match s {
        "plain" => SmtpAuth::Plain,
        "login" => SmtpAuth::Login,
        "xoauth2" => SmtpAuth::XOAuth2,
        _ => SmtpAuth::None,
    }
}

fn sync_mode_to_str(m: SyncMode) -> &'static str {
    match m {
        SyncMode::Idle => "idle",
        SyncMode::Poll => "poll",
    }
}

fn sync_mode_from_str(s: &str) -> SyncMode {
    match s {
        "idle" => SyncMode::Idle,
        _ => SyncMode::Poll,
    }
}
