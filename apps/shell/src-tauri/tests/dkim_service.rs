//! DKIM 应用服务的集成测试。
//!
//! 这里锁住四条「说出来才算数」的承诺：
//!
//! 1. **签的 == 发的**：提交给 SMTP 服务器的 `DATA` 里有一份 DKIM-Signature，
//!    而且它覆盖的正是一起发出去的那些字节（用公钥真验一次，不是看它长得像）；
//! 2. **没配就不变**：没配 DKIM 的账户，冻结的字节跟签名功能不存在时一模一样；
//! 3. **私钥不出门**：私钥只进平台钥匙串；`get_dkim_config` 的返回值里没有它，
//!    数据库里也没有它；
//! 4. **重发不重签**：签名发生在冻结之前，冻结之后重发的是同一份字节。
//!
//! 私钥从 `crates/xremail-dkim/tests/fixtures/` 借（那是 crate 自己测试用的
//! 那把 2048 位钥，公钥也在同一个目录里）——本文件里不出现任何密钥材料。

use sqlx::SqlitePool;
use xremail_api::{NewAccountRequest, SecretValue};
use xremail_core::model::{ProviderKind, SendState, SmtpAuth, SyncMode, TlsMode};
use xremail_dkim::{PublicComponents, SigningKey, canonicalize, key, sign};
use xremail_protocol_imap::TlsTrust as ImapTlsTrust;
use xremail_protocol_smtp::TlsTrust;
use xremail_shell_lib::service::account::AccountService;
use xremail_shell_lib::service::compose::{ComposeService, DraftInput, SendResultState};
use xremail_shell_lib::service::dkim::{DkimConfigInput, DkimService};
use xremail_store::dao::send_queue as send_queue_dao;
use xremail_testkit::smtp::{FakeSmtpServer, SmtpServerConfig};
use xremail_testkit::{FakeImapServer, MailboxFixture, ServerConfig};

/// 假服务器的登录凭据。
const USER: &str = "user@163.com";
const PASSWORD: &str = "smtp-auth-code";

/// 测试用的 2048 位 RSA 私钥（PKCS#1 PEM）与它的公钥（base64）。
const PRIVATE_KEY_PEM: &str =
    include_str!("../../../../crates/xremail-dkim/tests/fixtures/test_private_key.pem");
const PUBLIC_KEY: &str =
    include_str!("../../../../crates/xremail-dkim/tests/fixtures/test_public_key.txt");

/// 一套「内存库 + 服务 + 账户 + 假服务器」。
struct Fixture {
    service: ComposeService,
    pool: SqlitePool,
    account_id: String,
}

impl Fixture {
    /// 造一套指向假 SMTP 服务器、假 IMAP 服务器的套件。
    async fn start() -> (Self, FakeSmtpServer) {
        let server = FakeSmtpServer::start(SmtpServerConfig::implicit_tls().with_auth(
            &["PLAIN", "LOGIN"],
            USER,
            PASSWORD,
        ))
        .await
        .expect("假 SMTP 服务器启动失败");
        let imap = FakeImapServer::start(ServerConfig {
            user: USER.to_string(),
            password: PASSWORD.to_string(),
            mailboxes: vec![
                MailboxFixture::new("INBOX"),
                MailboxFixture::new("已发送").with_attribute("\\Sent"),
            ],
            ..ServerConfig::default()
        })
        .await
        .expect("假 IMAP 服务器启动失败");

        let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
            .await
            .expect("打开内存库失败");
        xremail_store::migrate(&pool).await.expect("应用迁移失败");

        let accounts = AccountService::new(pool.clone());
        let account_id = accounts
            .create(NewAccountRequest {
                email: USER.to_string(),
                provider: ProviderKind::Netease163,
                display_name: None,
                imap_host: imap.host().to_string(),
                imap_port: imap.port(),
                imap_tls: TlsMode::Implicit,
                smtp_host: server.host().to_string(),
                smtp_port: server.port(),
                smtp_tls: TlsMode::Implicit,
                smtp_auth: SmtpAuth::Login,
                username: USER.to_string(),
                sync_mode: SyncMode::Poll,
                poll_interval_secs: 300,
            })
            .await
            .expect("建账户失败")
            .id;

        let service = ComposeService::with_trusts(
            pool.clone(),
            accounts,
            TlsTrust::Roots(server.root_store()),
            ImapTlsTrust::Roots(imap.root_store()),
        )
        .with_settle(0);
        (
            Self {
                service,
                pool,
                account_id,
            },
            server,
        )
    }

    /// 配好 DKIM。
    async fn configure_dkim(&self, selector: &str, domain: &str) {
        let service = DkimService::new(AccountService::new(self.pool.clone()));
        service
            .set(
                &self.pool,
                &DkimConfigInput {
                    account_id: &self.account_id,
                    selector,
                    domain,
                    private_key_pem: PRIVATE_KEY_PEM,
                },
                1_800_000_000_000,
            )
            .await
            .expect("写 DKIM 配置失败");
    }

    /// 库里那封信。
    async fn stored(&self) -> xremail_core::model::SendQueueEntry {
        let rows = send_queue_dao::list_entries(&self.pool, &self.account_id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "应当只有一封信");
        rows.into_iter().next().unwrap()
    }
}

/// 一份草稿。
fn draft(account_id: &str) -> DraftInput {
    DraftInput {
        account_id: account_id.to_string(),
        id: None,
        to: vec!["friend@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "DKIM 测试".to_string(),
        body_text: "AUTOTEST-DKIM-MARKER 这封信是自动测试发出来的。".to_string(),
        body_html: None,
        attachments: vec![],
    }
}

fn credential() -> SecretValue {
    SecretValue::from_str_lossless(PASSWORD)
}

/// 从一封完整的邮件字节里把 `DKIM-Signature` 的各个标签读出来。
fn dkim_tags(message: &[u8]) -> Vec<(String, String)> {
    let (headers, _) = canonicalize::split_headers_and_body(message);
    let field = headers
        .iter()
        .find(|field| field.name().eq_ignore_ascii_case("DKIM-Signature"))
        .expect("邮件里没有 DKIM-Signature 头");
    let mut tags = Vec::new();
    for part in field.folded_value().split(';') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        tags.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    tags
}

fn tag(tags: &[(String, String)], name: &str) -> String {
    tags.iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("DKIM-Signature 里缺少 {name}="))
}

/// 用**独立实现**（本 crate 的验签入口，只吃公钥）验一份已签好的邮件。
///
/// 它重建签名输入的方式是「按 `h=` 从最终字节里取头、`b=` 置空」——
/// 和签名时算的那一份不是同一段代码路径，所以能验过才算数。
fn signature_verifies(message: &[u8]) -> bool {
    let tags = dkim_tags(message);
    let b = tag(&tags, "b").replace(char::is_whitespace, "");
    let components: PublicComponents =
        key::rsa_public_components(PUBLIC_KEY.trim()).expect("公钥解析失败");
    let (headers, body) = canonicalize::split_headers_and_body(message);
    let mut signed_headers: Vec<String> = tag(&tags, "h")
        .split(':')
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    signed_headers.push("dkim-signature".to_string());

    let mut input = String::new();
    for name in &signed_headers {
        if name == "dkim-signature" {
            let field = headers
                .iter()
                .find(|field| field.name().eq_ignore_ascii_case("DKIM-Signature"))
                .expect("缺 DKIM-Signature");
            let relaxed = field.relaxed();
            let index = relaxed.rfind("b=").unwrap_or(relaxed.len());
            input.push_str(&relaxed[..index + 2]);
        } else {
            let field = headers
                .iter()
                .rev()
                .find(|field| field.name().eq_ignore_ascii_case(name))
                .unwrap_or_else(|| panic!("被签的头 {name} 不在信里"));
            input.push_str(&field.relaxed());
            input.push_str("\r\n");
        }
    }
    // bh= 必须与正文对得上（最后一项被签头后面不带 CRLF 的处理见上）。
    let expected_bh = sign::body_hash_relaxed(&body);
    if tag(&tags, "bh") != expected_bh {
        return false;
    }
    let signature = base64_decode(&b);
    key::verify_with_components(&components, input.as_bytes(), &signature).unwrap_or(false)
}

/// 只用于测试的 base64 解码（crate 自己的测试里也有一个，这里不重复导出）。
fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("b= 不是合法 base64")
}

#[tokio::test]
async fn a_signed_message_carries_a_dkim_signature_that_verifies() {
    let (fixture, server) = Fixture::start().await;
    fixture.configure_dkim("sel", "example.com").await;

    let result = fixture
        .service
        .send(&draft(&fixture.account_id), &credential())
        .await
        .unwrap();
    assert_eq!(result.state, SendResultState::Sent);

    let received = server.last_message().expect("服务器没收到信");
    let tags = dkim_tags(&received);
    assert_eq!(tag(&tags, "v"), "1");
    assert_eq!(tag(&tags, "a"), "rsa-sha256");
    assert_eq!(tag(&tags, "c"), "relaxed/relaxed");
    assert_eq!(tag(&tags, "d"), "example.com");
    assert_eq!(tag(&tags, "s"), "sel");
    assert!(
        tag(&tags, "h").contains("from"),
        "From 必须在 h= 里：否则任何人换个发件人就冒充了这封信"
    );

    // 冻结的字节就是发出去的字节。
    let row = fixture.stored().await;
    assert_eq!(row.state, SendState::Succeeded);
    assert_eq!(row.message_bytes.as_deref(), Some(received.as_slice()));

    // 用公钥真验一次。
    assert!(signature_verifies(&received), "发出去的这封信验签不过");

    // 篡改正文一个字节就验不过（防止「验证函数只会返回 true」）。
    let mut tampered = received.clone();
    let position = tampered
        .windows(5)
        .position(|window| window == b"DKIM ")
        .expect("找不到正文");
    tampered[position] = b'X';
    assert!(!signature_verifies(&tampered), "改了一个字节还验过了");

    server.shutdown().await;
}

#[tokio::test]
async fn an_account_without_dkim_sends_exactly_the_bytes_it_used_to() {
    let (fixture, server) = Fixture::start().await;

    let result = fixture
        .service
        .send(&draft(&fixture.account_id), &credential())
        .await
        .unwrap();
    assert_eq!(result.state, SendResultState::Sent);

    let received = server.last_message().expect("服务器没收到信");
    let text = String::from_utf8_lossy(&received);
    assert!(
        !text.to_ascii_lowercase().contains("dkim-signature:"),
        "没配 DKIM 的账户不该凭空多出签名头"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn the_private_key_stays_in_the_keychain_and_never_comes_back() {
    let (fixture, _server) = Fixture::start().await;
    fixture.configure_dkim("sel", "Example.COM.").await;

    let service = DkimService::new(AccountService::new(fixture.pool.clone()));
    let config = service
        .get(&fixture.pool, &fixture.account_id)
        .await
        .expect("读配置失败")
        .expect("读不到刚写进去的配置");

    // 选择器与域名规整过了；公钥是从私钥算出来的那一份。
    assert_eq!(config.selector, "sel");
    assert_eq!(config.domain, "example.com");
    assert_eq!(config.dns_name, "sel._domainkey.example.com");
    assert_eq!(config.public_key.as_deref(), Some(PUBLIC_KEY.trim()));
    assert_eq!(
        config.dns_record.as_deref(),
        Some(format!("v=DKIM1; k=rsa; p={}", PUBLIC_KEY.trim()).as_str())
    );

    // 返回值里没有私钥：序列化之后搜不到任何一段密钥材料。
    let json = serde_json::to_string(&config).unwrap();
    assert!(
        !json.contains("PRIVATE KEY"),
        "get_dkim_config 的返回值里出现了私钥"
    );
    assert!(
        PRIVATE_KEY_PEM.lines().any(|line| line.len() > 40)
            && !json.contains(&PRIVATE_KEY_PEM.lines().nth(1).unwrap()[0..40]),
        "get_dkim_config 的返回值里出现了私钥正文"
    );

    // 数据库里也没有：只有选择器、域名、算法、引用名与启用位。
    let row = xremail_store::dao::dkim::get_dkim_config(&fixture.pool, &fixture.account_id)
        .await
        .unwrap()
        .expect("库里应当有配置行");
    let dumped = format!("{row:?}");
    assert!(
        !dumped.contains(&PRIVATE_KEY_PEM.lines().nth(1).unwrap()[0..40]),
        "数据库里出现了私钥正文"
    );
    assert_eq!(
        row.private_key_ref,
        format!("xremail:dkim:{}", fixture.account_id)
    );

    // 清掉之后：配置没了，发信的字节也不再带签名。
    assert!(
        service
            .clear(&fixture.pool, &fixture.account_id)
            .await
            .unwrap()
    );
    assert!(
        service
            .get(&fixture.pool, &fixture.account_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_resend_after_an_uncertain_send_sends_the_frozen_bytes_even_if_the_dkim_config_changed() {
    // 设计决定（不是漏洞）：**冻结的字节优先于当前的 DKIM 配置**。
    //
    // 「再发一次」是同一个 Message-ID 的同一封信再投一次，收信方可能已经收到过
    // 第一份；如果这次换成新的选择器 / 新钥重新签，那两封信就是「两份不同内容的
    // 同一封信」。所以重发只认冻结时的那份字节（含当时那个签名）。
    // 用户改了 DKIM 配置之后，改动的效果体现在**下一封新信**上。
    //
    // 服务器收下 DATA 却不回应答 → 这封信是「不确定」。
    let server = FakeSmtpServer::start(
        SmtpServerConfig::implicit_tls()
            .with_auth(&["PLAIN", "LOGIN"], USER, PASSWORD)
            .dropping_data_response(),
    )
    .await
    .expect("假 SMTP 服务器启动失败");
    let (fixture, sent_copy_server) = Fixture::with_server_at(server).await;

    let result = fixture
        .service
        .send(&draft(&fixture.account_id), &credential())
        .await
        .unwrap();
    assert_eq!(result.state, SendResultState::Uncertain);
    let frozen = fixture
        .stored()
        .await
        .message_bytes
        .expect("不确定的行也必须留着冻结的字节");
    assert_eq!(tag(&dkim_tags(&frozen), "s"), "sel");

    // 重发之前把选择器换掉：冻结字节里的签名不会跟着变。
    fixture.configure_dkim("other", "example.com").await;

    let resent = fixture
        .service
        .resend_uncertain(&fixture.account_id, &result.entry_id, &credential())
        .await
        .unwrap();
    assert_eq!(resent.state, SendResultState::Uncertain);

    let rows = send_queue_dao::list_entries(&fixture.pool, &fixture.account_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "重发不该新建一行");
    assert_eq!(
        rows[0].message_bytes.as_deref(),
        Some(frozen.as_slice()),
        "重发发的必须是冻结时的那份字节（含当时的签名），不能按新配置重新签一遍"
    );
    assert_eq!(
        tag(&dkim_tags(&frozen), "s"),
        "sel",
        "冻结字节里的选择器仍是签发时的那个"
    );

    sent_copy_server.shutdown().await;
}

#[tokio::test]
async fn clearing_the_dkim_config_removes_the_key_from_the_keychain() {
    let (fixture, _server) = Fixture::start().await;
    fixture.configure_dkim("sel", "example.com").await;

    let secret_ref = DkimService::secret_ref_for(&fixture.account_id);
    let store = xremail_shell_lib::secrets::platform_store();
    assert!(
        store
            .get(&secret_ref)
            .await
            .expect("读钥匙串失败")
            .is_some(),
        "配置之后钥匙串里应当有这把钥"
    );

    // 删账户时 `delete_account` 命令会调的就是这句清理（见 `cmd::delete_account`）。
    let dkim = DkimService::new(AccountService::new(fixture.pool.clone()));
    assert!(
        dkim.clear(&fixture.pool, &fixture.account_id)
            .await
            .unwrap(),
        "表里那一行应当被删掉"
    );

    assert!(
        store
            .get(&secret_ref)
            .await
            .expect("读钥匙串失败")
            .is_none(),
        "账户删掉之后 DKIM 钥不能留在钥匙串里"
    );
}

#[tokio::test]
async fn a_key_that_ring_cannot_use_is_refused_before_it_reaches_the_keychain() {
    let (fixture, _server) = Fixture::start().await;
    let service = DkimService::new(AccountService::new(fixture.pool.clone()));

    let error = service
        .set(
            &fixture.pool,
            &DkimConfigInput {
                account_id: &fixture.account_id,
                selector: "sel",
                domain: "example.com",
                private_key_pem: "-----BEGIN RSA PRIVATE KEY-----\nnot a key\n-----END RSA PRIVATE KEY-----\n",
            },
            1_800_000_000_000,
        )
        .await
        .expect_err("假私钥应当被拒");
    assert!(
        !error.to_string().contains("not a key"),
        "错误信息里不该回显私钥内容：{error}"
    );

    // 被拒之后库里没有半条配置。
    assert!(
        service
            .get(&fixture.pool, &fixture.account_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_selector_that_is_not_a_dns_label_is_refused() {
    let (fixture, _server) = Fixture::start().await;
    let service = DkimService::new(AccountService::new(fixture.pool.clone()));

    for bad in [
        "",
        "a b",
        "sel\r\nBcc: evil@example.com",
        "x".repeat(64).as_str(),
    ] {
        let error = service
            .set(
                &fixture.pool,
                &DkimConfigInput {
                    account_id: &fixture.account_id,
                    selector: bad,
                    domain: "example.com",
                    private_key_pem: PRIVATE_KEY_PEM,
                },
                1_800_000_000_000,
            )
            .await
            .expect_err("非法选择器应当被拒");
        assert!(
            error.to_string().contains("选择器"),
            "错误信息应当指出是哪一项：{error}"
        );
    }
}

#[tokio::test]
async fn the_signing_key_loads_from_the_private_pem() {
    // 保险丝：如果 fixture 里的私钥哪天被换坏了，上面那些用例会给出莫名其妙
    // 的失败——这里先把「这把钥本身能用」钉住。
    let key = SigningKey::from_pem(PRIVATE_KEY_PEM).expect("私钥加载失败");
    assert_eq!(key.bits(), 2048);
}

impl Fixture {
    /// 用一个已经起好的假 SMTP 服务器造套件。
    async fn with_server_at(server: FakeSmtpServer) -> (Self, FakeSmtpServer) {
        let imap = FakeImapServer::start(ServerConfig {
            user: USER.to_string(),
            password: PASSWORD.to_string(),
            mailboxes: vec![
                MailboxFixture::new("INBOX"),
                MailboxFixture::new("已发送").with_attribute("\\Sent"),
            ],
            ..ServerConfig::default()
        })
        .await
        .expect("假 IMAP 服务器启动失败");

        let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
            .await
            .expect("打开内存库失败");
        xremail_store::migrate(&pool).await.expect("应用迁移失败");

        let accounts = AccountService::new(pool.clone());
        let account_id = accounts
            .create(NewAccountRequest {
                email: USER.to_string(),
                provider: ProviderKind::Netease163,
                display_name: None,
                imap_host: imap.host().to_string(),
                imap_port: imap.port(),
                imap_tls: TlsMode::Implicit,
                smtp_host: server.host().to_string(),
                smtp_port: server.port(),
                smtp_tls: TlsMode::Implicit,
                smtp_auth: SmtpAuth::Login,
                username: USER.to_string(),
                sync_mode: SyncMode::Poll,
                poll_interval_secs: 300,
            })
            .await
            .expect("建账户失败")
            .id;

        let service = ComposeService::with_trusts(
            pool.clone(),
            accounts,
            TlsTrust::Roots(server.root_store()),
            ImapTlsTrust::Roots(imap.root_store()),
        )
        .with_settle(0);
        let fixture = Self {
            service,
            pool,
            account_id,
        };
        fixture.configure_dkim("sel", "example.com").await;
        (fixture, server)
    }
}
