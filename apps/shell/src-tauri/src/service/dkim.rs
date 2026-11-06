//! DKIM 应用服务：账户级的「选择器 / 签名域 / 私钥」配置与发信时的签名。
//!
//! ## 三个东西各放哪
//!
//! | 东西 | 放哪 | 为什么 |
//! |---|---|---|
//! | `selector`（`s=`）、`domain`（`d=`）、算法、是否启用 | 数据库 `account_dkim` 表 | 不是机密，用户要能看见、能改 |
//! | 私钥（PEM） | 平台钥匙串，引用名 `xremail:dkim:<account_id>` | 数据库文件会被备份、同步、拷走 |
//! | 公钥（`p=`） | **不存**：保存配置时从私钥现算，只在 `get_dkim_config` 里回读 | 公钥可以随便给，放进 DNS 才是它该待的地方 |
//!
//! 与授权码一样：**先写钥匙串，成功了再写数据库**。反过来一旦写钥匙串失败，
//! 库里会留下一条指向空条目的配置，发信时会报「私钥取不到」。
//!
//! ## 私钥绝不出现在日志、事件、命令返回值里
//!
//! 私钥全程只在栈上的一份 String 里流转，用完立刻 zeroize；本模块的任何错误信息、
//! `get_dkim_config` 的返回值都**不含**私钥，也不含 `b=` 的完整签名。
//!
//! ## 签名发生在「冻结字节」之前
//!
//! [`crate::service::compose::ComposeService`] 在 `build_message` 之后、
//! `queue_for_sending` 之前调用 [`DkimService::sign_message`]：
//! 签完的那份字节就是被冻结的字节，重试发的也是同一份。
//! **没配 DKIM 的账户一个字节都不变**。

use xremail_api::{SecretStoreError, SecretValue};
use xremail_core::{AppError, AppResult};
use xremail_dkim::{
    DkimError, SignRequest, SigningKey, VerifyingKey, pem_to_der, sign_message as dkim_sign,
};
use xremail_store::dao::dkim as dkim_dao;
use xremail_store::dao::dkim::DkimConfigRow;
use zeroize::Zeroize;

use crate::secrets;
use crate::service::AccountService;

/// 目前唯一支持的签名算法（`a=`）。
pub const HEADER_ALG: &str = "rsa-sha256";

/// 给前端看的 DKIM 配置。**不含私钥**，只含可以被公开的公钥。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DkimConfigDto {
    /// 账户 id。
    pub account_id: String,
    /// DNS 选择器（`s=`）。
    pub selector: String,
    /// 签名域（`d=`）。
    pub domain: String,
    /// 签名算法（`a=`）。
    pub header_alg: String,
    /// 发信时是否真的签名。
    pub enabled: bool,
    /// DNS 里要发布的公钥（`p=` 的值，base64）。
    ///
    /// 从钥匙串里的私钥现算；取不到私钥时是 `None`（前端照样显示配置，只是提示
    /// 「公钥读不出来」）。
    pub public_key: Option<String>,
    /// DNS 记录名，方便用户复制：`<selector>._domainkey.<domain>`。
    pub dns_name: String,
    /// 建议发布的 TXT 记录内容：`v=DKIM1; k=rsa; p=…`。
    pub dns_record: Option<String>,
}

/// 待保存的 DKIM 配置（命令层的入参）。
#[derive(Debug, Clone)]
pub struct DkimConfigInput<'a> {
    pub account_id: &'a str,
    pub selector: &'a str,
    pub domain: &'a str,
    /// 私钥 PEM（PKCS#8 或 PKCS#1）。
    pub private_key_pem: &'a str,
}

/// DKIM 服务。
#[derive(Debug, Clone)]
pub struct DkimService {
    accounts: AccountService,
}

impl DkimService {
    /// 用账户服务构造（账户校验用）。
    pub fn new(accounts: AccountService) -> Self {
        Self { accounts }
    }

    /// 钥匙串里那条私钥的引用名。
    ///
    /// 与授权码的 `xremail:account:<id>` 同一风格，但前缀不同：
    /// 两条条目互不相干，删授权码不该把签名钥一起删掉。
    pub fn secret_ref_for(account_id: &str) -> String {
        format!("xremail:dkim:{account_id}")
    }

    /// 读某个账户的配置（不碰钥匙串）。
    async fn row(
        &self,
        pool: &sqlx::SqlitePool,
        account_id: &str,
    ) -> AppResult<Option<DkimConfigRow>> {
        dkim_dao::get_dkim_config(pool, account_id).await
    }

    /// 读配置并附上公钥（给前端看）。
    ///
    /// 私钥**不**出现在返回值里：这里只调用 `VerifyingKey`，把私钥换算成公钥后
    /// 立刻丢弃私钥的那份拷贝。
    pub async fn get(
        &self,
        pool: &sqlx::SqlitePool,
        account_id: &str,
    ) -> AppResult<Option<DkimConfigDto>> {
        let Some(row) = self.row(pool, account_id).await? else {
            return Ok(None);
        };
        let public_key = self.public_key(&row.private_key_ref).await?;
        let dns_name = format!("{}._domainkey.{}", row.selector, row.domain);
        let dns_record = public_key
            .as_ref()
            .map(|key| format!("v=DKIM1; k=rsa; p={}", key.selector_public_key()));
        Ok(Some(DkimConfigDto {
            account_id: row.account_id,
            selector: row.selector,
            domain: row.domain,
            header_alg: row.header_alg,
            enabled: row.enabled,
            public_key: public_key.map(|key| key.selector_public_key().to_string()),
            dns_name,
            dns_record,
        }))
    }

    /// 从钥匙串里取私钥、换算成公钥。
    ///
    /// ## 「没找到」与「读失败」必须分开
    ///
    /// 钥匙串返回「未找到」是**正常**情况（配置行还在、私钥被用户手工删了），
    /// 这时回 `Ok(None)`：界面显示配置了但没有公钥。
    ///
    /// 钥匙串返回别的错误（拒绝访问、后端不可用……）则**原样往上抛**，
    /// 绝不能吞成 `None`——那样界面会以为「没配置」，发信时会静默发出
    /// 一封没有签名的信。这与信路上的口径一致：签名失败就报错，不降级。
    async fn public_key(&self, secret_ref: &str) -> AppResult<Option<VerifyingKey>> {
        let store = secrets::platform_store();
        let Some(secret) = store
            .get(secret_ref)
            .await
            .map_err(|error| AppError::Storage(secret_detail(&error)))?
        else {
            return Ok(None);
        };
        let mut pem = secret_to_pem(&secret)?;
        let public = public_key_from_pem(&pem);
        pem.zeroize();
        Ok(Some(public?))
    }

    /// 保存配置：先写钥匙串，成功了再写数据库。
    ///
    /// 私钥会先被**完整校验一遍**（能解出来、能签一次）再落盘——
    /// 一把抄错的钥不会在保存时报错，只会在收信方那边悄悄失败，
    /// 所以这里宁可多一次签名。
    pub async fn set(
        &self,
        pool: &sqlx::SqlitePool,
        input: &DkimConfigInput<'_>,
        now: i64,
    ) -> AppResult<DkimConfigDto> {
        self.require_account(input.account_id).await?;
        let selector = normalize_selector(input.selector)?;
        let domain = normalize_domain(input.domain)?;
        let secret_ref = Self::secret_ref_for(input.account_id);

        // 私钥为空 = 「只改选择器 / 域名，钥不动」；但前提是已经有一把存着的钥。
        if input.private_key_pem.trim().is_empty() {
            if self.row(pool, input.account_id).await?.is_none() {
                return Err(AppError::Invalid(
                    "请填写 DKIM 私钥（PEM）。重新生成选择器时也要把对应的私钥贴进来".to_string(),
                ));
            }
        } else {
            // 先校验：解不出来 / 太短 / 带口令的钥，一律在写钥匙串之前就拒掉。
            let mut pem = input.private_key_pem.to_string();
            let prepared =
                load_signing_key(&pem).and_then(|key| self_verify(&key, &pem).map(|()| key));
            let stored = SecretValue::from_str_lossless(pem.as_str());
            // 私钥已经拷进钥匙串那一步要用的值里了，栈上这份马上抹掉。
            pem.zeroize();
            drop(prepared?);
            secrets::platform_store()
                .set(&secret_ref, &stored)
                .await
                .map_err(|error| AppError::Storage(save_error_message(&error)))?;
        }

        dkim_dao::upsert_dkim_config(
            pool,
            input.account_id,
            &selector,
            &domain,
            HEADER_ALG,
            &secret_ref,
            true,
            now,
        )
        .await?;

        self.get(pool, input.account_id)
            .await?
            .ok_or_else(|| AppError::Storage("DKIM 配置写入后读不回来".to_string()))
    }

    /// 清掉配置：删钥匙串条目 + 删表里的行。
    ///
    /// 返回是否真的删掉了表里那一行。删完钥匙串条目之后，即使删表失败，
    /// 下次发信也只会报「私钥取不到」而不会用别的账户的钥去签。
    pub async fn clear(&self, pool: &sqlx::SqlitePool, account_id: &str) -> AppResult<bool> {
        let stored_ref = self
            .row(pool, account_id)
            .await?
            .map(|row| row.private_key_ref);
        let canonical_ref = Self::secret_ref_for(account_id);

        let store = secrets::platform_store();
        let mut seen: Vec<String> = Vec::new();
        for candidate in [stored_ref, Some(canonical_ref)].into_iter().flatten() {
            if seen.contains(&candidate) {
                continue;
            }
            seen.push(candidate.clone());
            store
                .delete(&candidate)
                .await
                .map_err(|error| AppError::Storage(delete_error_message(&error)))?;
        }

        dkim_dao::delete_dkim_config(pool, account_id).await
    }

    /// 发信时给一封已经构建好的 MIME 字节加签名。
    ///
    /// * 账户没配 DKIM（表里没有行）→ `Ok(None)`，字节**原样**返回；
    /// * 配了但被禁用 → `Ok(None)`；
    /// * 配了且启用 → `Ok(Some(新字节))`。
    ///
    /// 取不到私钥时**不降级成「不签名」**：那样用户以为邮件是签过的，
    /// 实际没有。这里宁可报错，让用户去修配置。
    pub async fn sign_message(
        &self,
        pool: &sqlx::SqlitePool,
        account_id: &str,
        message: &[u8],
        now_ms: i64,
    ) -> AppResult<Option<Vec<u8>>> {
        let Some(row) = self.row(pool, account_id).await? else {
            return Ok(None);
        };
        if !row.enabled {
            return Ok(None);
        }
        if row.header_alg != HEADER_ALG {
            return Err(AppError::Invalid(format!(
                "账户的 DKIM 配置用了不支持的算法 {}（目前只支持 {HEADER_ALG}）",
                row.header_alg
            )));
        }

        let store = secrets::platform_store();
        let secret = store
            .get(&row.private_key_ref)
            .await
            .map_err(|error| AppError::Storage(read_error_message(&error)))?
            .ok_or_else(|| {
                AppError::Storage(format!(
                    "账户的 DKIM 私钥不在平台钥匙串里（{}），请重新填写",
                    row.private_key_ref
                ))
            })?;
        let mut pem = secret_to_pem(&secret)?;
        let key = match load_signing_key(&pem) {
            Ok(key) => key,
            Err(error) => {
                pem.zeroize();
                return Err(error);
            }
        };
        // 私钥已经解成 ring 的钥对象了，栈上这份文本立刻抹掉。
        pem.zeroize();

        let request = SignRequest::new(message, &row.domain, &row.selector, &key)
            .with_timestamp(now_ms / 1000);
        let signed = dkim_sign(&request).map_err(sign_error_message)?;
        Ok(Some(signed.bytes))
    }

    /// 账户必须存在（不存在就别往库里写配置）。
    async fn require_account(&self, account_id: &str) -> AppResult<()> {
        match self.accounts.get(account_id).await? {
            Some(_) => Ok(()),
            None => Err(AppError::NotFound(format!("账户 {account_id} 不存在"))),
        }
    }
}

/// 把私钥 PEM 载入成签名钥（错误信息里不带密钥材料）。
fn load_signing_key(pem: &str) -> AppResult<SigningKey> {
    SigningKey::from_pem(pem).map_err(key_error_message)
}

/// 从钥匙串里那条 SecretValue 取出一份 PEM 文本（调用方用完要 `zeroize`）。
fn secret_to_pem(secret: &SecretValue) -> AppResult<String> {
    secret.as_str().map(str::to_string).ok_or_else(|| {
        AppError::Invalid("钥匙串里的 DKIM 私钥不是合法的 UTF-8 文本，请重新填写".to_string())
    })
}

/// 用这把钥签一段固定内容，再用它自己的公钥验回来。
///
/// 目的：在**保存配置时**就发现「钥能解、但签出来的东西验不过」这类问题
/// （例如粘贴时混进了别的字符）。固定内容的签名是确定性的，验一次足够。
fn self_verify(key: &SigningKey, pem: &str) -> AppResult<()> {
    let public = public_key_from_pem(pem)?;
    let probe = b"xremail dkim self-test";
    let (algorithm, signature) = key.sign_raw(probe).map_err(key_error_message)?;
    debug_assert_eq!(algorithm.as_str(), HEADER_ALG);
    let verified = public
        .verify(probe, &signature)
        .map_err(key_error_message)?;
    if !verified {
        return Err(AppError::Invalid(
            "这把私钥签出来的内容用自己的公钥都验不过，请确认粘贴的是完整的私钥".to_string(),
        ));
    }
    Ok(())
}

/// 从私钥 PEM 算出公钥。
fn public_key_from_pem(pem: &str) -> AppResult<VerifyingKey> {
    let der = pem_to_der(pem).map_err(key_error_message)?;
    // 先确认它真是能用的私钥（ring 会拒掉太短 / 结构不对的钥），再换算公钥。
    let _key = SigningKey::from_der(&der).map_err(key_error_message)?;
    VerifyingKey::from_private(&der).map_err(key_error_message)
}

/// 选择器只许出现 DNS 标签里合法的字符，且不能为空 / 不能太长。
fn normalize_selector(raw: &str) -> AppResult<String> {
    let selector = raw.trim().to_string();
    if selector.is_empty() {
        return Err(AppError::Invalid("选择器（selector）不能为空".to_string()));
    }
    if selector.len() > 63 {
        return Err(AppError::Invalid(
            "选择器（selector）不能超过 63 个字符".to_string(),
        ));
    }
    let ok = selector
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if !ok {
        return Err(AppError::Invalid(
            "选择器（selector）只能包含字母、数字、点、连字符与下划线".to_string(),
        ));
    }
    Ok(selector)
}

/// 签名域必须是域名形状（不做 DNS 校验，只防手滑与注入）。
fn normalize_domain(raw: &str) -> AppResult<String> {
    let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return Err(AppError::Invalid("签名域（domain）不能为空".to_string()));
    }
    if domain.len() > 253 {
        return Err(AppError::Invalid(
            "签名域（domain）不能超过 253 个字符".to_string(),
        ));
    }
    let ok = domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    });
    if !ok {
        return Err(AppError::Invalid(
            "签名域（domain）要写成域名，例如 example.com".to_string(),
        ));
    }
    Ok(domain)
}

/// 密钥错误的用户可读文案（**不含**密钥材料）。
fn key_error_message(error: DkimError) -> AppError {
    // `DkimError` 是非穷尽枚举：文案由它的 Display impl 给出（同样不含密钥材料），
    // 这里不逐变体匹配，新增变体也不会悄悄落进 `_ =>`。
    AppError::Invalid(error.to_string())
}

/// 签名错误的用户可读文案。
fn sign_error_message(error: DkimError) -> AppError {
    key_error_message(error)
}

/// 钥匙串写入失败的文案。
fn save_error_message(error: &SecretStoreError) -> String {
    format!("DKIM 私钥没有存进平台钥匙串：{}", secret_detail(error))
}

/// 钥匙串读取失败的文案。
fn read_error_message(error: &SecretStoreError) -> String {
    format!("读不到 DKIM 私钥：{}", secret_detail(error))
}

/// 钥匙串删除失败的文案。
fn delete_error_message(error: &SecretStoreError) -> String {
    format!("清不掉钥匙串里的 DKIM 私钥：{}", secret_detail(error))
}

/// 取出钥匙串错误里那句说明。
fn secret_detail(error: &SecretStoreError) -> String {
    // `SecretStoreError` 是非穷尽枚举：Display 已经把「是哪一类 + 具体原因」写全了。
    error.to_string()
}
