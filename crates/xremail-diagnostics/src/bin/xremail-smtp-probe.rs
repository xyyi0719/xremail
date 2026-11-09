//! 只读 SMTP 探测工具。
//!
//! **严格只读**：只执行 `EHLO` / `STARTTLS` / `AUTH` / `QUIT`。
//! 绝不执行 `MAIL FROM` / `RCPT TO` / `DATA`，也不发信。
//!
//! ## 为什么需要它
//!
//! QQ / 163 / Gmail 的 SMTP 参数——端口、STARTTLS 语义、`AUTH` 机制、
//! `8BITMIME` 支持度、服务端证书——**无法从代码推断，必须实测**。
//! 这个工具让用户在自己机器上跑一次，拿到一份 markdown 报告贴回来。
//!
//! ## 凭据处理
//!
//! 凭据从**本机文件**读取（`%USERPROFILE%/.xremail/accounts.txt`），
//! 按 `[provider]` 段落选择账号。
//!
//! * 凭据**不会**出现在命令行参数里（命令行参数会进 shell 历史）。
//! * 凭据**不会**出现在本程序的任何输出里（报告里只有脱敏账号）。
//! * 本程序**不**写任何日志文件。
//!
//! 用法：
//!
//! ```text
//! cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --list
//! cargo run -p xremail-diagnostics --bin xremail-smtp-probe -- --provider 163 --host smtp.163.com --port 465 --tls-mode auto
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use xremail_core::model::{SmtpAuth, TlsMode as DomainTlsMode};
use xremail_protocol_imap::credential::{CredentialStore, default_accounts_path};
use xremail_protocol_smtp::{
    AuthMechanism, AuthOutcome, ProbeConfig, ProbeReport, SmtpConfig, TlsMode, TlsTrust,
};

/// 命令行上的 TLS 模式选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsModeArg {
    /// 465 走隐式 TLS，其它端口走明文 + STARTTLS。
    Auto,
    /// 隐式 TLS（连上就是 TLS）。
    Direct,
    /// 明文 + STARTTLS。
    Starttls,
}

impl TlsModeArg {
    /// 解析参数值。
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.to_ascii_lowercase().as_str() {
            "auto" => Ok(TlsModeArg::Auto),
            "direct" => Ok(TlsModeArg::Direct),
            "starttls" => Ok(TlsModeArg::Starttls),
            other => Err(format!(
                "--tls-mode 只能是 auto / direct / starttls，实际: {other}"
            )),
        }
    }
}

/// 命令行参数。
struct Args {
    host: String,
    port: u16,
    tls_mode: TlsModeArg,
    credentials: PathBuf,
    provider: String,
    no_auth: bool,
    list_only: bool,
    /// 额外信任的 CA（PEM）。
    ca_file: Option<PathBuf>,
    /// 严格 TLS：服务器拒绝 ClientHello 就直接报错，不降级重试。
    strict_tls: bool,
    /// 强制只用 TLS 1.2（不先试 1.3）。
    tls12_only: bool,
}

impl Args {
    /// 实际使用的 TLS 模式。
    fn resolve_tls_mode(&self) -> TlsMode {
        match self.tls_mode {
            // 465 是隐式 TLS 的约定端口；其余端口按 STARTTLS 处理。
            TlsModeArg::Auto => {
                if self.port == 465 {
                    TlsMode::Direct
                } else {
                    TlsMode::Starttls
                }
            }
            TlsModeArg::Direct => TlsMode::Direct,
            TlsModeArg::Starttls => TlsMode::Starttls,
        }
    }
}

/// 各 provider 的 SMTP 默认地址（隐式 TLS，465）。
///
/// **地址不靠猜**：这些是官方/社区文档里常见的入口，
/// 但企业邮箱的节点**因企业而异**，所以 `--host` / `--port` 随时能覆盖。
/// 真正连不上的原因，最终以探测报告为准（见 `docs/PROTOCOLS.md`）。
fn default_smtp_endpoint(provider: &str) -> (&'static str, u16) {
    match provider.trim().to_ascii_lowercase().as_str() {
        "163" => ("smtp.163.com", 465),
        "126" => ("smtp.126.com", 465),
        "qq" => ("smtp.qq.com", 465),
        "wo" => ("smtp.wo.cn", 465),
        "qiye" => ("smtp.qiye.163.com", 465),
        _ => ("smtp.163.com", 465),
    }
}

fn parse_args() -> Result<Args, String> {
    // 主机与端口可以由 --host / --port 覆盖；没给就按 provider 查表。
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut tls_mode = TlsModeArg::Auto;
    let mut credentials = default_accounts_path();
    let mut provider = "163".to_string();
    let mut no_auth = false;
    let mut list_only = false;
    let mut ca_file = None;
    let mut strict_tls = false;
    let mut tls12_only = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => {
                host = Some(args.next().ok_or("--host 缺少取值")?);
            }
            "--port" => {
                let raw = args.next().ok_or("--port 缺少取值")?;
                port = Some(raw.parse().map_err(|_| "--port 不是数字")?);
            }
            "--tls-mode" => {
                let raw = args.next().ok_or("--tls-mode 缺少取值")?;
                tls_mode = TlsModeArg::parse(&raw)?;
            }
            "--credentials" => {
                credentials = PathBuf::from(args.next().ok_or("--credentials 缺少取值")?);
            }
            "--provider" => {
                provider = args.next().ok_or("--provider 缺少取值")?;
            }
            "--no-auth" => {
                no_auth = true;
            }
            "--ca-file" => {
                ca_file = Some(PathBuf::from(args.next().ok_or("--ca-file 缺少取值")?));
            }
            "--strict-tls" => {
                strict_tls = true;
            }
            "--tls12-only" => {
                tls12_only = true;
            }
            "--list" => {
                list_only = true;
            }
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }

    let (default_host, default_port) = default_smtp_endpoint(&provider);
    let host = host.unwrap_or_else(|| default_host.to_string());
    let port = port.unwrap_or(default_port);

    Ok(Args {
        host,
        port,
        tls_mode,
        credentials,
        provider,
        no_auth,
        list_only,
        ca_file,
        strict_tls,
        tls12_only,
    })
}

const HELP: &str = "\
用法：xremail-smtp-probe [选项]

  --list                     只列出凭据文件里已配置的 provider
  --provider <标识>          用哪个账号（对应文件里的 [段落名]），默认 163
  --host <主机名>            默认按 provider 查表（163/126/qq/wo/qiye）
  --port <端口>              默认 465（隐式 TLS）
  --tls-mode <模式>          auto（默认）/ direct / starttls
                             auto = 465 走隐式 TLS，其它端口走明文 + STARTTLS
  --credentials <文件>       默认 %USERPROFILE%/.xremail/accounts.txt
  --no-auth                  只探测不认证（不读口令、不发口令）
  --ca-file <PEM 文件>      额外信任一个 CA（自建/测试服务器用）。
                             **不**关闭校验：证书照样必须通过验证。
  --strict-tls              严格模式：服务器拒绝 TLS ClientHello 就直接报错，
                             不降级重试（默认允许回退到 TLS 1.2 并在报告里写明）
  --tls12-only              强制只用 TLS 1.2（不先试 1.3）

只读探测：只执行 EHLO / STARTTLS / AUTH / QUIT。
绝不执行 MAIL FROM / RCPT TO / DATA，不会发信，也不会修改服务端任何状态。
";

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("参数错误: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    let store = match CredentialStore::read_from_file(&args.credentials) {
        Ok(store) => store,
        Err(e) => {
            if args.no_auth {
                // 不认证 → 没有凭据文件也能探。
                CredentialStore::parse("").expect("空凭据文件")
            } else {
                eprintln!("{e}");
                eprintln!("凭据文件位置: {}", args.credentials.display());
                eprintln!("模板见同目录的 accounts.template");
                return ExitCode::from(2);
            }
        }
    };

    if args.list_only {
        println!("凭据文件: {}", args.credentials.display());
        if store.is_empty() {
            println!("（没有配置任何账号）");
        } else {
            for provider in store.providers() {
                let account = store.get(provider).unwrap();
                println!("- {provider:<8} {}", account.masked_user());
            }
        }
        return ExitCode::SUCCESS;
    }

    // 不认证时，凭据可有可无（--no-auth 就是为了「手上没口令也想看看服务器长什么样」）。
    let credential = match store.get(&args.provider) {
        Ok(credential) => Some(credential),
        Err(e) => {
            if args.no_auth {
                None
            } else {
                eprintln!("{e}");
                eprintln!("已配置的 provider: {:?}", store.providers());
                return ExitCode::from(2);
            }
        }
    };

    let masked_user = credential
        .map(|c| c.masked_user())
        .unwrap_or_else(|| "（未使用）".to_string());

    // 口令只在真的要认证时才读；**不打印、不写日志**。
    let password = if args.no_auth {
        String::new()
    } else {
        match credential.and_then(|c| c.password.as_str()) {
            Some(password) => password.to_string(),
            None => {
                eprintln!("授权码不是合法的 UTF-8 文本");
                return ExitCode::from(2);
            }
        }
    };

    let tls_mode = args.resolve_tls_mode();
    let server = match tls_mode {
        TlsMode::Direct => SmtpConfig::direct(args.host.clone(), args.port),
        TlsMode::Starttls => SmtpConfig::starttls(args.host.clone(), args.port),
    };

    let mut config = ProbeConfig::new(server);
    if args.no_auth {
        config = config.without_auth();
    }
    if args.strict_tls {
        config = config.strict_tls();
    }
    if args.tls12_only {
        config.server = config.server.with_tls12_only();
    }

    println!("# Xremail SMTP 探测报告");
    println!();
    println!("- 目标       : {}:{}", args.host, args.port);
    println!("- provider   : {}", args.provider);
    println!("- 账号       : {masked_user}");
    println!("- 凭据来源   : {}", args.credentials.display());
    println!("- TLS 模式   : {}", tls_mode.as_str());
    println!(
        "- 信任来源   : {}",
        if args.ca_file.is_some() {
            "系统根 + 额外 CA（--ca-file）"
        } else {
            "系统根（rustls-native-certs）"
        }
    );
    println!("- 客户端     : xremail-protocol-smtp（rustls，强制校验系统根）");
    println!(
        "- TLS 版本   : {}",
        if args.strict_tls {
            "严格（服务器拒绝 ClientHello 就报错，不降级）"
        } else {
            "默认 1.3+1.2（服务器拒绝 ClientHello 时改用 1.2 重试，报告会写明）"
        }
    );
    println!();

    let trust = match &args.ca_file {
        Some(path) => match TlsTrust::system_roots_with_pem(path) {
            Ok(trust) => trust,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        },
        None => TlsTrust::SystemRoots,
    };

    let result = xremail_protocol_smtp::probe_with_trust(
        &config,
        &trust,
        credential.map(|c| c.user.as_str()).unwrap_or(""),
        &password,
    )
    .await;

    match result {
        Ok(report) => {
            print_report(&report, tls_mode, &args.host, args.port);
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("## 结果");
            println!();
            println!("- **失败阶段**: {e}");
            println!("- 可重试     : {}", e.is_retryable());
            println!("- 需用户处理 : {}", e.requires_user_action());
            eprintln!("探测失败: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 打印脱敏报告。
fn print_report(report: &ProbeReport, tls_mode: TlsMode, host: &str, port: u16) {
    println!("## TLS");
    println!();
    if let Some(fallback) = &report.tls_version_fallback {
        println!(
            "> ⚠️ **发生过 TLS 版本回退**：服务器不接受 TLS 1.3 的 ClientHello，已改用 **{}** 重试并成功。",
            fallback.used_version
        );
        println!(">");
        println!("> 服务器的原话：`{}`", fallback.rejected_detail);
        println!(">");
        println!(
            "> 这只是**版本问题**的处置：证书仍然按系统根校验、主机名仍然核对，**校验一步都没有放松**。"
        );
        println!();
    }
    println!("| 协议版本 | 加密套件 | 校验 | 证书链长度 |");
    println!("|---|---|---|---|");
    match &report.tls {
        Some(tls) => {
            println!(
                "| {} | {} | {} | {} |",
                tls.protocol,
                tls.cipher,
                if tls.verified {
                    "✅ 通过（rustls 强制校验）"
                } else {
                    "❌ 未通过"
                },
                tls.chain_len
            );
            println!();
            println!("| 证书主题 | 签发者 | 生效日 | 过期日 |");
            println!("|---|---|---|---|");
            match &tls.certificate {
                Some(cert) => {
                    println!(
                        "| {} | {} | {} | {} |",
                        cert.subject_cn.as_deref().unwrap_or("（无 CN）"),
                        cert.issuer_cn.as_deref().unwrap_or("（无 CN）"),
                        cert.not_before,
                        cert.not_after
                    );
                    println!();
                    println!(
                        "- 证书链体检：{}；剩余 {} 天；{}",
                        if cert.self_signed {
                            "自签证书"
                        } else {
                            "非自签"
                        },
                        cert.days_remaining,
                        // 三种状态分开说：**尚未生效**的证书同样是不可用的，
                        // 但文案不能写成「已过期」——那会把人引向错误的排查方向
                        // （去看续期，而实际该看的是服务器时钟或部署时间）。
                        if cert.expired {
                            "⚠️ **已过期**"
                        } else if cert.not_yet_valid {
                            "⚠️ **尚未生效**（当前时间早于证书的 notBefore）"
                        } else {
                            "在有效期内"
                        }
                    );
                    println!("- 主题全名：`{}`", cert.subject);
                    println!("- 签发者全名：`{}`", cert.issuer);
                }
                None => {
                    println!("| （解析失败） | — | — | — |");
                }
            }
        }
        None => {
            println!("| — | — | — | — |");
            println!();
            println!("- 没有 TLS 会话（这不该发生：探测要么走隐式 TLS，要么走 STARTTLS）。");
        }
    }
    println!();

    println!("## 服务器问候与能力");
    println!();
    println!("- 问候原文：`{}`", report.greeting);
    println!(
        "- STARTTLS：{}",
        if report.starttls_performed {
            "已执行（明文 → STARTTLS → 重新 EHLO）"
        } else {
            "未执行（隐式 TLS，连上就是 TLS）"
        }
    );
    println!();

    println!("## EHLO 扩展");
    println!();
    if report.ehlo.is_empty() {
        println!("（服务器没有返回任何扩展）");
    } else {
        println!("| 扩展（服务器原文） | 支持 | 对本项目的意义 |");
        println!("|---|---|---|");
        for extension in &report.ehlo {
            let support = match extension_support_flag(report, &extension.keyword) {
                Some(true) => "✅",
                Some(false) => "❌",
                None => "—",
            };
            println!(
                "| `{}` | {} | {} |",
                extension.raw,
                support,
                extension_meaning(&extension.keyword)
            );
        }
    }
    println!();

    if !report.ehlo_before_tls.is_empty() {
        println!("### STARTTLS 之前的 EHLO（明文阶段）");
        println!();
        for extension in &report.ehlo_before_tls {
            println!("- `{}`", extension.raw);
        }
        println!();
    }

    println!("## 认证");
    println!();
    println!(
        "- AUTH 机制（TLS 阶段）：{}",
        join_or_none(&report.auth_mechanisms)
    );
    println!(
        "- AUTH 机制（明文阶段）：{}",
        join_or_none(&report.auth_mechanisms_before_tls)
    );
    match &report.auth {
        AuthOutcome::Skipped => {
            println!("- 认证结果：⏭️ 未认证（--no-auth）");
        }
        AuthOutcome::Unsupported { mechanisms } => {
            println!(
                "- 认证结果：⚠️ 服务器宣告的机制本客户端都不能用：{}",
                join_or_none(mechanisms)
            );
        }
        AuthOutcome::Success {
            mechanism,
            response,
        } => {
            println!(
                "- 认证结果：✅ 成功（机制 {}）—— `{}`",
                mechanism.as_str(),
                response
            );
        }
        AuthOutcome::Failed {
            mechanism,
            response,
        } => {
            println!(
                "- 认证结果：❌ 失败（机制 {}）—— `{}`",
                mechanism.as_str(),
                response
            );
        }
    }
    println!();

    println!("## 建议配置");
    println!();
    println!("可以直接抄进应用配置的 SMTP 参数块（取值与 xremail-core 领域模型的 serde 名一致）：");
    println!();
    println!("```toml");
    println!("smtp_host = \"{host}\"");
    println!("smtp_port = {port}");
    println!("smtp_tls = {}", domain_tls_name(tls_mode));
    println!("smtp_auth = {}", domain_auth_name(report));
    println!("```");
    println!();
    println!("- `smtp_tls`：`implicit` = 隐式 TLS（465）；`starttls` = 明文 + STARTTLS（587）。");
    println!(
        "- `smtp_auth`：`login` = `AUTH LOGIN`；`plain` = `AUTH PLAIN`；`xoauth2` = `AUTH XOAUTH2`（本工具未实现）。"
    );
    if let Some(size) = report.extensions.size {
        println!("- 单封邮件大小上限：`SIZE {size}` 字节。");
    }
    if report.extensions.eight_bit_mime {
        println!("- 支持 `8BITMIME`：发信时可以声明 `BODY=8BITMIME`。");
    } else {
        println!("- **不支持** `8BITMIME`：正文要按 7bit 发送（或先做 quoted-printable 编码）。");
    }
    if report.extensions.smtputf8 {
        println!("- 支持 `SMTPUTF8`：可以用 UTF-8 邮箱地址与非 ASCII 标题。");
    } else {
        println!("- **不支持** `SMTPUTF8`：标题里的非 ASCII 字符要按 MIME 编码（RFC 2047）。");
    }
    if report.extensions.pipelining {
        println!("- 支持 `PIPELINING`：可以流水线下发命令。");
    }
    if report.extensions.dsn {
        println!("- 支持 `DSN`：可以请求投递状态通知。");
    }
}

/// 扩展的支持度布尔（`None` = 没归类过的扩展）。
fn extension_support_flag(report: &ProbeReport, keyword: &str) -> Option<bool> {
    let support = &report.extensions;
    Some(match keyword {
        "8BITMIME" => support.eight_bit_mime,
        "SMTPUTF8" => support.smtputf8,
        "STARTTLS" => support.starttls,
        "PIPELINING" => support.pipelining,
        "DSN" => support.dsn,
        "CHUNKING" => support.chunking,
        "BINARYMIME" => support.binary_mime,
        "ENHANCEDSTATUSCODES" => support.enhanced_status_codes,
        "REQUIRETLS" => support.requiretls,
        "AUTH" => support.auth,
        "SIZE" => support.size.is_some(),
        _ => return None,
    })
}

/// 扩展对本项目的意义。
fn extension_meaning(keyword: &str) -> &'static str {
    match keyword {
        "8BITMIME" => "可发 8 位正文（`BODY=8BITMIME`）",
        "SMTPUTF8" => "可发 UTF-8 邮箱地址与非 ASCII 标题",
        "SIZE" => "单封邮件大小上限（字节）",
        "STARTTLS" => "支持明文连接后升级到 TLS",
        "PIPELINING" => "支持命令流水线",
        "DSN" => "支持投递状态通知",
        "CHUNKING" => "支持 `BDAT` 分块传输",
        "BINARYMIME" => "支持二进制 MIME 传输",
        "ENHANCEDSTATUSCODES" => "增强状态码（`250 2.1.5 …`）",
        "REQUIRETLS" => "服务器**要求**必须走 TLS",
        "AUTH" => "可用的认证机制",
        _ => "（本项目暂未归类的扩展）",
    }
}

/// 领域模型里的 TLS 模式名（serde 名）。
fn domain_tls_name(mode: TlsMode) -> String {
    let value = match mode {
        TlsMode::Direct => DomainTlsMode::Implicit,
        TlsMode::Starttls => DomainTlsMode::Starttls,
    };
    serde_json::to_string(&value).unwrap_or_else(|_| "\"unknown\"".to_string())
}

/// 领域模型里的认证机制名（serde 名）。
fn domain_auth_name(report: &ProbeReport) -> String {
    let mechanism = match &report.auth {
        AuthOutcome::Success { mechanism, .. } | AuthOutcome::Failed { mechanism, .. } => {
            Some(*mechanism)
        }
        _ => None,
    };

    let value = match mechanism {
        Some(AuthMechanism::Plain) => SmtpAuth::Plain,
        Some(AuthMechanism::Login) => SmtpAuth::Login,
        Some(AuthMechanism::XOAuth2) => SmtpAuth::XOAuth2,
        None => SmtpAuth::None,
    };
    serde_json::to_string(&value).unwrap_or_else(|_| "\"none\"".to_string())
}

/// 列表拼成一行。
fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "（未宣告）".to_string()
    } else {
        items.join(", ")
    }
}
