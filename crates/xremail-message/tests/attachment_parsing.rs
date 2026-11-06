//! 附件解析的端到端测试：**真邮件字节进，附件元数据出**。
//!
//! 这些用例覆盖真实邮箱里见过的形状：
//! * `multipart/mixed` 里嵌 `multipart/alternative`（正文两版 + 一份附件）；
//! * 附件用 base64 / quoted-printable 传输；
//! * 中文文件名通过 RFC 2231（`filename*`）、RFC 2047（`=?utf-8?B?…?=`）
//!   和「头里直接写 UTF-8」三种方式出现；
//! * 内联图片：`inline` + `Content-ID` + `filename` 进附件表，
//!   `inline` 但没有名字的不进；
//! * 部件号与 IMAP 的 `BODY[n]` 编号一致（下载时要用）。

use xremail_message::{MAX_ATTACHMENT_PARTS, parse_message};

/// 把若干行拼成 CRLF 结尾的邮件字节。
fn mail(lines: &[&str]) -> Vec<u8> {
    lines.join("\r\n").into_bytes()
}

#[test]
fn nested_multipart_yields_numbered_attachments() {
    // 顶层 multipart/mixed 自己**占 1 号**（IMAP 的规则），它的子部件是 1.1–1.3：
    //   1     multipart/mixed
    //   1.1     multipart/alternative
    //   1.1.1     text/plain
    //   1.1.2     text/html
    //   1.2     application/pdf（附件）
    //   1.3     image/png（内联图）
    let raw = mail(&[
        "Subject: 带附件的邮件",
        "From: sender@example.com",
        "Content-Type: multipart/mixed; boundary=\"outer\"",
        "",
        "--outer",
        "Content-Type: multipart/alternative; boundary=\"inner\"",
        "",
        "--inner",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "纯文本正文",
        "--inner",
        "Content-Type: text/html; charset=utf-8",
        "",
        "<p>HTML 正文</p>",
        "--inner--",
        "--outer",
        "Content-Type: application/pdf",
        "Content-Disposition: attachment; filename=\"合同.pdf\"",
        "Content-Transfer-Encoding: base64",
        "",
        "JVBERi0xLjQK", // "%PDF-1.4\n"
        "--outer",
        "Content-Type: image/png",
        "Content-Disposition: inline; filename=\"logo.png\"",
        "Content-ID: <logo@example.com>",
        "Content-Transfer-Encoding: base64",
        "",
        "iVBORw0KGgo=",
        "--outer--",
        "",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(parsed.text_plain.as_deref(), Some("纯文本正文"));
    assert_eq!(parsed.html_raw.as_deref(), Some("<p>HTML 正文</p>"));
    assert!(parsed.has_attachment);
    assert_eq!(parsed.attachments.len(), 2, "{:#?}", parsed.attachments);

    let pdf = &parsed.attachments[0];
    assert_eq!(pdf.part_number, "1.2");
    assert_eq!(pdf.filename.as_deref(), Some("合同.pdf"));
    assert_eq!(pdf.content_type, "application/pdf");
    assert_eq!(pdf.size, 9, "解码后是 `%PDF-1.4\\n` 共 9 字节");
    assert!(!pdf.is_inline);
    assert_eq!(pdf.content_id, None);

    let logo = &parsed.attachments[1];
    assert_eq!(logo.part_number, "1.3");
    assert!(logo.is_inline);
    assert_eq!(logo.content_id.as_deref(), Some("logo@example.com"));
    assert_eq!(logo.content_type, "image/png");
    assert_eq!(logo.size, 8, "iVBORw0KGgo= 解出 8 字节");
}

#[test]
fn quoted_printable_attachment_is_measured_after_decoding() {
    let raw = mail(&[
        "Content-Type: multipart/mixed; boundary=b",
        "",
        "--b",
        "Content-Type: text/plain",
        "",
        "hello",
        "--b",
        "Content-Type: text/plain; charset=utf-8; name=\"=?utf-8?B?5pel5b+XLnR4dA==?=\"",
        "Content-Disposition: attachment; filename=\"=?utf-8?B?5pel5b+XLnR4dA==?=\"",
        "Content-Transfer-Encoding: quoted-printable",
        "",
        // 尾部的 `=\r\n` 是 **QP 软换行**（这里写成一行，`mail()` 再拼 CRLF）。
        "=E6=97=A5=E5=BF=97 =E7=AC=AC=E4=B8=80 =\\r\n=E8=A1=8C",
        "--b--",
        "",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(parsed.attachments.len(), 1);
    let log = &parsed.attachments[0];
    assert_eq!(log.part_number, "1.2");
    assert_eq!(log.filename.as_deref(), Some("日志.txt"), "RFC 2047 名字");
    // QP 里 `=`+CRLF 是软换行，去掉后是「日志 第一 =一行」（等号前那个空格是硬空格，
    // 不在行尾，所以不会被编码）；UTF-8 汉字各 3 字节，共 6 个汉字 = 18 字节，+3 = 21。
    let expected = "日志 第一 =一行".len();
    assert_eq!(log.size as usize, expected, "解码后字节数");
    assert_eq!(expected, 21);
    assert_eq!(log.content_type, "text/plain");
}

#[test]
fn rfc2231_chinese_filename() {
    let raw = mail(&[
        "Content-Type: multipart/mixed; boundary=b",
        "",
        "--b",
        "Content-Type: application/vnd.ms-excel",
        "Content-Disposition: attachment; filename=\"fallback.xls\"; \
         filename*=utf-8''%E9%94%80%E5%94%AE%E9%A2%84%E6%B5%8B.xlsx",
        "",
        "data",
        "--b--",
        "",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(
        parsed.attachments[0].filename.as_deref(),
        Some("销售预测.xlsx"),
        "扩展参数优先于裸 filename"
    );
}

#[test]
fn inline_part_without_filename_is_not_listed() {
    let raw = mail(&[
        "Content-Type: multipart/related; boundary=r",
        "",
        "--r",
        "Content-Type: text/html",
        "",
        "<img src=\"cid:bg@example.com\">",
        "--r",
        "Content-Type: image/gif",
        "Content-Disposition: inline",
        "Content-ID: <bg@example.com>",
        "",
        "GIF89a",
        "--r--",
        "",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert!(
        parsed.attachments.is_empty(),
        "没有文件名的 inline 部件是正文资源，不列进附件：{:#?}",
        parsed.attachments
    );
}

#[test]
fn top_level_attachment_is_numbered_text() {
    let raw = mail(&[
        "Content-Type: application/octet-stream",
        "Content-Disposition: attachment; filename=data.bin",
        "",
        "raw-bytes",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].part_number, "TEXT");
    assert_eq!(parsed.attachments[0].filename.as_deref(), Some("data.bin"));
    assert_eq!(parsed.attachments[0].size, 9);
}

#[test]
fn a_forwarded_message_is_numbered_through_its_zero_layer() {
    // message/rfc822 在服务器上的号是「父号.0」；内嵌邮件自己的顶层 multipart
    // **不再加**那层 1，所以里面第 2 段直接是 `1.2.0.2`：
    //   1                          multipart/mixed（顶层）
    //   1.1                        text/plain
    //   1.2.0                      message/rfc822 里的那封内嵌邮件
    //   1.2.0.1 / 1.2.0.2          └ 内嵌邮件的正文 + 附件（那层 multipart 不占号）
    let raw = mail(&[
        "Content-Type: multipart/mixed; boundary=\"outer\"",
        "",
        "--outer",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "请转发的邮件见下",
        "--outer",
        "Content-Type: message/rfc822",
        "",
        "Subject: 内嵌邮件",
        "Content-Type: multipart/mixed; boundary=\"inner\"",
        "",
        "--inner",
        "Content-Type: text/plain; charset=utf-8",
        "",
        "内嵌正文",
        "--inner",
        "Content-Type: application/pdf",
        "Content-Disposition: attachment; filename=\"内嵌.pdf\"",
        "Content-Transfer-Encoding: base64",
        "",
        "JVBERi0xLjQK",
        "--inner--",
        "--outer--",
        "",
    ]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(parsed.attachments.len(), 1, "{:#?}", parsed.attachments);
    assert_eq!(parsed.attachments[0].part_number, "1.2.0.2");
    assert_eq!(parsed.attachments[0].filename.as_deref(), Some("内嵌.pdf"));
}

#[test]
fn attachment_count_is_capped() {
    let mut lines: Vec<String> = vec![
        "Content-Type: multipart/mixed; boundary=b".to_string(),
        String::new(),
    ];
    for index in 0..(MAX_ATTACHMENT_PARTS + 5) {
        lines.push("--b".to_string());
        lines.push(format!(
            "Content-Type: text/plain\r\nContent-Disposition: attachment; filename=f{index}.txt"
        ));
        lines.push(String::new());
        lines.push("x".to_string());
    }
    lines.push("--b--".to_string());
    let raw = lines.join("\r\n").into_bytes();

    let parsed = parse_message(&raw).expect("应当能解析");

    assert_eq!(parsed.attachments.len(), MAX_ATTACHMENT_PARTS);
    assert_eq!(parsed.attachments[0].filename.as_deref(), Some("f0.txt"));
}

#[test]
fn a_message_without_attachments_has_an_empty_list() {
    let raw = mail(&["Content-Type: text/plain; charset=utf-8", "", "只有正文"]);

    let parsed = parse_message(&raw).expect("应当能解析");

    assert!(parsed.attachments.is_empty());
    assert!(!parsed.has_attachment);
}
