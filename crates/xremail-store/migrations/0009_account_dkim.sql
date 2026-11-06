-- 账户级 DKIM 签名配置（ROADMAP「签名（DKIM / 域签名）」）。
--
-- 为什么不往 `account` 表加列：`account` 的 DAO 用 `sqlx::query_as!`
-- 显式列清单，加列要连带重生 `.sqlx/` 离线数据，而且「没配 DKIM」的账户
-- 会多出一串永远为 NULL 的列。单独一张表还有一个好处：
-- **没有行 = 没配置 = 发信字节与以前完全一致**，这条不变量用 schema 本身就表达了。
--
-- 私钥**不在**这张表里：`private_key_ref` 只是操作系统钥匙串中那条条目的
-- 不透明引用，与 `account.secret_ref` 同一个约定（见 docs/SECURITY.md）。
-- 表里不出现任何密钥材料列——哪怕写成「加密后的私钥」也不行，
-- 本地优先客户端拿不到可用的主密钥，删掉账户行就该让密文失去意义。
--
-- 签名方式固定 relaxed/relaxed + rsa-sha256 起步；`header_alg` 留着给以后的
-- ed25519-sha256，现在只写 'rsa-sha256'。
CREATE TABLE account_dkim (
    account_id      TEXT    PRIMARY KEY NOT NULL
                            REFERENCES account (id) ON DELETE CASCADE,

    -- 选择器（`s=`）：DNS 里 `{selector}._domainkey.{domain}` 记录的名字。
    selector        TEXT    NOT NULL,
    -- 签名域（`d=`）。通常等于发信域，允许是它的父域（RFC 6376 §3.5）。
    domain          TEXT    NOT NULL,
    -- 签名算法（`a=`）。目前只有 rsa-sha256。
    header_alg      TEXT    NOT NULL,
    -- 钥匙串里那条私钥的不透明引用（**不是**私钥）。
    private_key_ref TEXT    NOT NULL,
    -- 是否在发信时签名。关掉时保留配置但**不改**字节（便于排障）。
    enabled         BOOLEAN NOT NULL DEFAULT 1,

    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
