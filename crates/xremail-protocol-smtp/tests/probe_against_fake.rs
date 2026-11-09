//! 用假 SMTP 服务器验证整条探测链路。
//!
//! 这是**交付可信度的关键**：工具在用户真机上跑之前必须先在本地被证明可用。
//!
//! 覆盖：
//! * EHLO 扩展的解析（全集 + 支持度布尔）。
//! * STARTTLS 路径（明文 → STARTTLS → 握手 → 重新 EHLO → 证书信息）。
//! * 隐式 TLS 路径。
//! * AUTH 成功与失败（失败时报告里**不含口令**）。
//! * 服务器不宣告 STARTTLS 时**明确报错**，不静默降级。
//! * 只读红线：一次投递类命令都不许发。

use std::time::Duration;

use xremail_protocol_smtp::{
    AuthMechanism, AuthOutcome, ProbeConfig, SmtpConfig, SmtpError, TlsMode, TlsTrust,
    probe_with_trust,
};
use xremail_testkit::{FakeSmtpServer, SmtpServerConfig};

const USER: &str = "user@example.com";
const PASSWORD: &str = "s3cret-pass";

/// 造一个指向假服务器的探测配置。
fn config_for(server: &FakeSmtpServer, mode: TlsMode) -> ProbeConfig {
    let mut config = match mode {
        TlsMode::Direct => SmtpConfig::direct(server.host(), server.port()),
        TlsMode::Starttls => SmtpConfig::starttls(server.host(), server.port()),
    };
    config.connect_timeout = Duration::from_secs(5);
    config.command_timeout = Duration::from_secs(5);
    ProbeConfig::new(config)
}

/// 假服务器只用自签证书，测试要把那张 CA 显式交给客户端。
fn trust(server: &FakeSmtpServer) -> TlsTrust {
    TlsTrust::Roots(server.root_store())
}

#[tokio::test]
async fn ehlo_extensions_are_parsed_with_support_flags() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new().with_extensions(&[
        "SIZE 52428800",
        "8BITMIME",
        "SMTPUTF8",
    ]))
    .await
    .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    server.shutdown().await;

    // 扩展全集：服务器宣告的都列出来了（含服务器自己补的 STARTTLS / AUTH）。
    let keywords: Vec<&str> = report.ehlo.iter().map(|e| e.keyword.as_str()).collect();
    for expected in ["SIZE", "8BITMIME", "SMTPUTF8", "STARTTLS", "AUTH"] {
        assert!(
            keywords.contains(&expected),
            "扩展 {expected} 不在报告里，实际: {keywords:?}"
        );
    }

    // 原文逐条保留（开发者要照着服务器的真实写法抄配置）。
    let size = report
        .ehlo
        .iter()
        .find(|e| e.keyword == "SIZE")
        .expect("应当有 SIZE");
    assert_eq!(size.raw, "SIZE 52428800");
    assert_eq!(size.params, vec!["52428800".to_string()]);

    // 支持度布尔。
    assert!(report.extensions.eight_bit_mime);
    assert!(report.extensions.smtputf8);
    assert!(report.extensions.starttls);
    assert!(report.extensions.auth);
    assert_eq!(report.extensions.size, Some(52428800));
    // 服务器**没有**宣告的，必须是 false（不能默认 true）。
    assert!(!report.extensions.pipelining);
    assert!(!report.extensions.dsn);
    assert!(!report.extensions.chunking);
}

#[tokio::test]
async fn starttls_path_upgrades_then_re_ehlos() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new())
        .await
        .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    server.shutdown().await;

    assert!(report.starttls_performed, "STARTTLS 应当真的执行了");

    // 证书信息非空。
    let tls = report.tls.expect("TLS 信息应当有");
    assert!(tls.verified);
    assert!(tls.chain_len >= 1, "证书链长度应当至少是 1");
    assert!(!tls.protocol.is_empty());
    let certificate = tls.certificate.expect("证书体检结果应当有");
    assert_eq!(certificate.subject_cn.as_deref(), Some("localhost"));
    assert!(!certificate.expired);
    assert!(certificate.days_remaining > 0);
    // 假证书是**现在**签的，`notBefore` 就在此刻附近——它不该被判成「尚未生效」。
    //
    // 这一条是 R68 的反向保护：`not_yet_valid` 是 2026-11-10 新加的字段
    // （之前只把 notBefore 格式化成字符串打印、从不比较，于是一个「明天才生效」
    // 的证书会显示成「未过期」）。这里确认正常证书**不会**被误判成尚未生效。
    assert!(!certificate.not_yet_valid, "刚签发的证书不该被判成尚未生效");

    // 握手后**重新 EHLO**：明文阶段服务器不宣告 AUTH，TLS 阶段才宣告。
    assert!(
        report.auth_mechanisms_before_tls.is_empty(),
        "明文阶段不该宣告 AUTH，实际: {:?}",
        report.auth_mechanisms_before_tls
    );
    assert_eq!(
        report.auth_mechanisms,
        vec!["LOGIN".to_string(), "PLAIN".to_string()]
    );
    // 明文阶段的 EHLO 里仍然有 STARTTLS。
    assert!(
        report
            .ehlo_before_tls
            .iter()
            .any(|e| e.keyword == "STARTTLS")
    );
}

#[tokio::test]
async fn implicit_tls_path_connects_directly() {
    let server = FakeSmtpServer::start(SmtpServerConfig::implicit_tls())
        .await
        .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Direct);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    server.shutdown().await;

    assert!(!report.starttls_performed, "隐式 TLS 不该再发 STARTTLS");
    assert!(report.tls.is_some(), "隐式 TLS 也该有 TLS 信息");
    assert!(report.ehlo_before_tls.is_empty());
    assert!(report.extensions.eight_bit_mime);
    assert!(report.extensions.size == Some(52428800));
}

#[tokio::test]
async fn auth_success_reports_the_mechanism() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new().with_auth(
        &["PLAIN", "LOGIN"],
        USER,
        PASSWORD,
    ))
    .await
    .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    server.shutdown().await;

    match &report.auth {
        AuthOutcome::Success {
            mechanism,
            response,
        } => {
            assert_eq!(*mechanism, AuthMechanism::Login);
            assert!(response.contains("235"), "实际响应: {response}");
        }
        other => panic!("应当认证成功，实际: {other:?}"),
    }
}

#[tokio::test]
async fn auth_with_plain_only_falls_back_to_plain() {
    let server =
        FakeSmtpServer::start(SmtpServerConfig::new().with_auth(&["PLAIN"], USER, PASSWORD))
            .await
            .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    server.shutdown().await;

    match &report.auth {
        AuthOutcome::Success { mechanism, .. } => {
            assert_eq!(*mechanism, AuthMechanism::Plain);
        }
        other => panic!("应当认证成功，实际: {other:?}"),
    }
}

#[tokio::test]
async fn auth_failure_is_reported_without_the_password() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new().with_auth(
        &["PLAIN", "LOGIN"],
        USER,
        PASSWORD,
    ))
    .await
    .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let wrong_password = "wrong-password";
    let report = probe_with_trust(&config, &trust(&server), USER, wrong_password)
        .await
        .expect("认证失败也要出报告，不是探测失败");
    server.shutdown().await;

    match &report.auth {
        AuthOutcome::Failed {
            mechanism,
            response,
        } => {
            assert_eq!(*mechanism, AuthMechanism::Login);
            assert!(response.contains("535"), "实际响应: {response}");
            assert!(
                !response.contains(wrong_password),
                "报告里出现了口令: {response}"
            );
        }
        other => panic!("应当认证失败，实际: {other:?}"),
    }

    // 整个报告（含 Debug）都不该有口令。
    let dumped = format!("{report:?}");
    assert!(!dumped.contains(wrong_password), "Debug 输出里出现了口令");
    assert!(!dumped.contains(PASSWORD), "Debug 输出里出现了口令");
}

#[tokio::test]
async fn rejected_mechanism_is_reported_as_failure() {
    // 服务器宣告 LOGIN，但只接受 PLAIN。
    let server = FakeSmtpServer::start(
        SmtpServerConfig::new()
            .with_auth(&["PLAIN", "LOGIN"], USER, PASSWORD)
            .accepting_only("PLAIN"),
    )
    .await
    .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let report = probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("认证失败也要出报告");
    server.shutdown().await;

    match &report.auth {
        AuthOutcome::Failed { response, .. } => {
            assert!(response.contains("504"), "实际响应: {response}");
        }
        other => panic!("应当认证失败，实际: {other:?}"),
    }
}

#[tokio::test]
async fn missing_starttls_is_an_error_not_a_silent_downgrade() {
    let server = FakeSmtpServer::start(SmtpServerConfig::plaintext())
        .await
        .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    let result = probe_with_trust(&config, &trust(&server), USER, PASSWORD).await;
    let commands = server.commands();
    server.shutdown().await;

    let err = match result {
        Ok(_) => panic!("服务器没宣告 STARTTLS 时必须报错，不能静默降级"),
        Err(err) => err,
    };
    assert!(
        matches!(err, SmtpError::Unsupported(_)),
        "应当是「不支持该能力」，实际: {err}"
    );
    assert!(!err.is_retryable(), "能力缺失不是重试能解决的");

    // 红线：没有 TLS 就不该把口令送出去。
    assert!(
        !commands.contains(&"AUTH".to_string()),
        "没有 TLS 就不该认证，实际命令: {commands:?}"
    );
}

#[tokio::test]
async fn probe_never_sends_delivery_commands() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new())
        .await
        .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls);
    probe_with_trust(&config, &trust(&server), USER, PASSWORD)
        .await
        .expect("探测应当成功");
    let commands = server.commands();
    server.shutdown().await;

    for forbidden in ["MAIL", "RCPT", "DATA", "RSET"] {
        assert!(
            !commands.contains(&forbidden.to_string()),
            "探测发送了 {forbidden}，实际命令: {commands:?}"
        );
    }

    // 只读命令确实用上了。
    for expected in ["EHLO", "STARTTLS", "AUTH", "QUIT"] {
        assert!(
            commands.contains(&expected.to_string()),
            "应当发过 {expected}，实际命令: {commands:?}"
        );
    }

    // EHLO 在握手后应当出现两次（RFC 3207 §4.2 要求重新 EHLO）。
    let ehlo_count = commands.iter().filter(|c| *c == "EHLO").count();
    assert_eq!(ehlo_count, 2, "实际命令: {commands:?}");
}

#[tokio::test]
async fn no_auth_mode_skips_authentication_entirely() {
    let server = FakeSmtpServer::start(SmtpServerConfig::new())
        .await
        .expect("假服务器应当能启动");

    let config = config_for(&server, TlsMode::Starttls).without_auth();
    let report = probe_with_trust(&config, &trust(&server), USER, "")
        .await
        .expect("探测应当成功");
    let commands = server.commands();
    server.shutdown().await;

    assert!(matches!(report.auth, AuthOutcome::Skipped));
    assert!(
        !commands.contains(&"AUTH".to_string()),
        "不认证就不该发 AUTH，实际命令: {commands:?}"
    );
}
