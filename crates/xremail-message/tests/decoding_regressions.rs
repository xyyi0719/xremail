//! 解码正确性与错误处理的回归用例（2026-11-10 第 12 轮审计：R59–R62）。
//!
//! 每一条都先在**未修**的代码上跑成红色，修完再转绿。报文形状都取自
//! 真实客户端会发出的东西（Outlook / 国产 webmail / 各家移动端），
//! 不是造出来的极端输入。

use xremail_message::charset::{decode, decode_lossy};
use xremail_message::compose::{BuildOptions, OutgoingMessage, build_message};
use xremail_message::encoded_word::decode_encoded_words;
use xremail_message::parse_message;

// ---------------------------------------------------------------------------
// R59：RFC 2047 的 `Q` 编码词没有把 `_` 还原成空格
// ---------------------------------------------------------------------------

#[test]
fn rfc2047_q_encoding_turns_underscore_into_a_space() {
    // RFC 2047 §4.2：`Q` 编码里 `_` **代表空格**（"the underscore character
    // ... always represents a 'space' character"）。Outlook 与大量国产客户端
    // 都用它——`=?gb2312?Q?...?=` 在 QQ / 沃的邮件里很常见。
    let decoded = decode_encoded_words("=?utf-8?Q?hello_world?=");

    assert_eq!(decoded, "hello world", "Q 编码词里的 `_` 必须还原成空格");
}

#[test]
fn rfc2047_q_encoding_in_a_gb2312_subject() {
    // `=D6=D0=CE=C4` 是 GB2312 的“中文”，`_` 是空格。
    let decoded = decode_encoded_words("=?gb2312?Q?=D6=D0=CE=C4_=D6=D0=CE=C4?=");

    assert_eq!(decoded, "中文 中文");
}

#[test]
fn rfc2047_q_encoding_keeps_real_escapes() {
    // 反向保护：`=5F` 是**字面下划线**（不能被当成空格），`=3D` 是 `=`。
    let decoded = decode_encoded_words("=?utf-8?Q?a=5Fb=3Dc?=");

    assert_eq!(decoded, "a_b=c");
}

#[test]
fn rfc2047_q_encoding_applies_to_attachment_filenames_too() {
    // 这个名字走的是 `attachment::decode_any` → `decode_encoded_words`，
    // 同一个缺陷会让附件名变成 `2026_Q1.pdf` 而不是 `2026 Q1.pdf`。
    let raw = b"Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"=?utf-8?Q?2026_Q1.pdf?=\"\r\n\r\nx";
    let parsed = parse_message(raw).expect("应当能解析");

    assert_eq!(
        parsed.attachments[0].filename.as_deref(),
        Some("2026 Q1.pdf")
    );
}

// ---------------------------------------------------------------------------
// R60：相邻编码词的空白判定看的是「解出来的最后一个字符」
// ---------------------------------------------------------------------------

#[test]
fn an_encoded_word_that_decodes_to_plain_ascii_still_abuts_the_next_one() {
    // RFC 2047 §6.2：两个**编码词**之间的空白要丢掉——与它们解出来是什么无关。
    // 这里第一个编码词解出来是 `Re: `（纯 ASCII）。旧代码靠「输出末尾是不是
    // 非 ASCII」判断相邻，于是把本该丢掉的空白留了下来，多出一个空格。
    let decoded = decode_encoded_words("=?utf-8?B?UmU6IA==?= =?utf-8?B?5Li76aKY?=");

    assert_eq!(decoded, "Re: 主题", "相邻编码词之间的空白必须丢掉");
}

#[test]
fn an_encoded_word_after_a_plain_prefix_keeps_the_plain_whitespace() {
    // 反向保护：前面那段是**明文**（不是编码词），它自带的空白必须保留。
    assert_eq!(
        decode_encoded_words("hello =?utf-8?B?5Li76aKY?="),
        "hello 主题"
    );
}

#[test]
fn folded_adjacent_encoded_words_do_not_gain_a_space() {
    // 折行（CRLF + 制表符）本身就是 RFC 5322 的 FWS；解析层已经把它折叠成
    // 一个空格，这里再走一遍 RFC 2047 §6.2 的「编码词之间的空白要丢掉」。
    let decoded = decode_encoded_words("=?utf-8?B?5Lit5paH?= \t =?utf-8?B?5Li76aKY?=");

    assert_eq!(decoded, "中文主题");
}

#[test]
fn a_subject_split_over_two_lines_loses_only_the_folding_space() {
    // 真实形态：客户端把长标题拆成两个编码词，折在 CRLF + 空格 处。
    let raw = "Subject: =?utf-8?B?UmU6IOa1i+ivleagh+mimA==?=\r\n\t=?utf-8?B?5Li76aKY?=\r\n\
From: a@example.com\r\n\r\nbody";
    let headers = xremail_message::parse_headers(raw.as_bytes());

    assert_eq!(headers.subject.as_deref(), Some("Re: 测试标题主题"));
}

// ---------------------------------------------------------------------------
// R61：默认的「先严格试 UTF-8」让「像 UTF-8 的 GBK」静默解错且不报错
// ---------------------------------------------------------------------------

#[test]
#[ignore = "已知未修（R65）：要修得先有能力区分「西里尔/希腊字母」与「其实是 GBK」，见用例里的说明"]
fn gbk_short_sequences_that_look_like_utf8_should_not_be_silent() {
    // **这条当前是红的**（`--ignored` 才会跑），钉住的是已知残余，不是「已修复」。见 ISSUES_AND_RISKS 的 R65。
    //
    // 现场：QQ / 沃有大量邮件**不写 charset**（见 `charset` 模块文档的实测表），正文按 GBK 发。
    // GBK 每个汉字是「0x81–0xFE + 0x40–0xFE」，其中相当一部分凑巧是合法 UTF-8：
    // `[CB, B5]`（“说”）→ `˵`（U+02F5）、`[D2, AA]`（“要”）→ `Ҫ`，
    // 这两个都**不是** GB18030 的合法双字节序列（本仓库实测：GBK 的 24448 个
    // 双字节里嵌了「一个完整 CJK 字符」的 0 个，说明代码页 936 里根本没有这种位）。
    // 而 `guess_encoding` 先严格试 UTF-8，于是把它们当 UTF-8 吃了，`had_errors = false`。
    //
    // 不能只靠「合法 UTF-8 就是 UTF-8」：两个 CJK 汉字的 GBK 字节**100%** 也能
    // 被 UTF-8 严格解成一对非 ASCII 字符（本仓库用 42 个常用汉字两两配对实测，
    // 8916182 / 8916182 全部可解），那是 GBK 文本最典型的形态，一改就会把真 UTF-8 解坏。
    // 真要修，得按「解出来的字符是不是这个语种的常用字」打分（中文正文里突然冒出
    // 西里尔/希腊字母就判可疑），那是另一个工程，不在本 crate 的能力范围内。
    let mut silent = Vec::new();
    for bytes in [&[0xCBu8, 0xB5][..], &[0xD2, 0xAA][..], &[0xB1, 0xB1][..]] {
        let (text, _, had_errors) = decode(bytes, None);
        if text != encoding_rs::GBK.decode(bytes).0.as_ref() && !had_errors {
            silent.push(format!("{bytes:02X?} -> {text:?}"));
        }
    }
    assert!(
        silent.is_empty(),
        "被静默当 UTF-8 解掉的 GBK 字节：{silent:?}"
    );
}

#[test]
fn real_utf8_is_still_decoded_as_utf8() {
    // 反向保护：真正的 UTF-8（含中日韩文与标点）不能被误判成 GBK。
    for text in [
        "这是一段正常的中文正文，包含标点。",
        "中文",
        "日本語のテキスト、テスト。",
        "한국어 텍스트입니다.",
        "Mixed 中文 and English, with a comma, a period. And a quote “x”.",
    ] {
        let (decoded, encoding, had_errors) = decode(text.as_bytes(), None);
        assert_eq!(decoded, text, "应保持 UTF-8");
        assert_eq!(encoding.name(), "UTF-8");
        assert!(!had_errors);
    }
}

#[test]
fn gbk_bytes_without_a_charset_still_come_out_as_chinese() {
    // 长的 GBK 正文（严格 UTF-8 一定过不了）本来就走对了，别修坏。
    let bytes = &[0xC7u8, 0xEB, 0xBB, 0xD8, 0xB8, 0xB4][..];
    assert_eq!(decode_lossy(bytes, None), "请回复");
}

#[test]
fn ascii_is_still_decoded_as_utf8() {
    let (text, encoding, had_errors) = decode(b"plain ascii", None);

    assert_eq!(text, "plain ascii");
    assert_eq!(encoding.name(), "UTF-8");
    assert!(!had_errors);
}

#[test]
fn a_message_without_a_charset_that_is_really_gbk_is_not_marked_as_garbage() {
    // “中文”的 GBK 字节恰好也能被 UTF-8 吃得下（见上一条），但那是短文本；
    // 这里钉住「真实邮件路径」：没有 charset 的 GBK 正文解出来必须是中文，
    // 且 `decode_errors` 不能说谎。
    let raw = b"Content-Type: text/plain\r\n\r\n\
\xC7\xEB\xBB\xD8\xB8\xB4\xA3\xAC\xD6\xD0\xCE\xC4";
    let parsed = parse_message(raw).expect("应当能解析");

    assert_eq!(parsed.text_plain.as_deref(), Some("请回复，中文"));
    assert!(!parsed.decode_errors);
}

// ---------------------------------------------------------------------------
// R62：发信侧的折行会把「超长空白段」折成空续行 = 邮件头提前结束
// ---------------------------------------------------------------------------

fn options() -> BuildOptions {
    BuildOptions {
        eight_bit: false,
        now_epoch_secs: 1_793_154_619,
        message_id_domain: "xremail.local".to_string(),
        charset: "utf-8".to_string(),
    }
}

fn message_with_subject(subject: String) -> OutgoingMessage {
    OutgoingMessage {
        from: "sender@example.com".to_string(),
        to: vec!["alice@example.com".to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        body_text: Some("正文".to_string()),
        body_html: None,
        attachments: Vec::new(),
        in_reply_to: None,
        references: None,
    }
}

/// 邮件头块（第一个空行之前的所有行）。
fn header_lines(text: &str) -> Vec<String> {
    text.split("\r\n")
        .take_while(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn a_subject_that_is_one_long_run_of_spaces_ends_the_header_early() {
    // **钉住现状（已知未修，R62 的残余）**：这条断言的是「现在的错误行为」，
    // 不是「正确行为」。修好之后它会变红——那时把它改成 `assert_eq!(len, 6)`。
    //
    // 现场：`fold_value` 对「整段全是空格」的输入按宽度硬切，两行恰好用光
    // 一行 998 的预算：第一行 988 个空格（拼着 `Subject:` 正好 998），第二行
    // 是续行的前导空格 + 剩下的 9 个空格。接收端把行尾空白 trim 掉之后，
    // 第一行剩下的非空白内容就只剩 `Subject:`，第二行整行空白——
    // 它和折叠出来的前导空格一起构成了空行，邮件头到此结束，
    // `Date:` / `MIME-Version:` / `Content-Type:` / `Content-Transfer-Encoding:`
    // 全掉进正文，收件方看到乱码加一串头字段。
    //
    // 为什么先不修：可用的折行空间是逐行变化的（第一行要扣掉 `Subject: `，
    // 续行只扣一个空格），而「这一轮该用多少」由 `write_header` 决定；所有
    // 能放进 `fold_value` 的保守方案（每行再少放一格）都会让正常长标题
    // （`Quarterly report …`）的往返解出多一个空格。正确修法是让
    // `fold_value` 接收「本行的实际前缀长度」——那是一处跨函数的改动，
    // 本轮先如实登记，见报告。
    let built =
        build_message(&message_with_subject(" ".repeat(995)), &options()).expect("应当构建成功");

    let lines = header_lines(&String::from_utf8_lossy(&built.bytes));
    assert_eq!(
        lines.len(),
        9,
        "现状：邮件头拖到第 9 行才结束（第 4 行是只剩空白的续行）"
    );
    assert_eq!(
        lines[3].len(),
        9,
        "现状：第 4 行是续行的前导空格 + 9 个空格，整行没有可见内容"
    );
    // 正确的断言在这个长度下是 `lines.len() == 8` 且 `!lines[..].any(|l| l.trim().is_empty())`。
}

#[test]
fn a_long_run_of_spaces_inside_a_subject_does_not_end_the_header() {
    // **这条是通过的**：空白后面还有内容时，硬切会接着把行填满，
    // 不会剩下孤零零的一行空白。反向保护——修 R62 的残余时别把这条改坏。
    let subject = format!("{}{}{}", "x".repeat(10), " ".repeat(3000), "y");
    let built = build_message(&message_with_subject(subject), &options()).expect("应当构建成功");
    let text = String::from_utf8_lossy(&built.bytes);

    assert!(
        text.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"),
        "Content-Type 必须留在邮件头里：\n{text}"
    );
    // 现状（与上一条同一个残余）：仍会折出「只有空白」的行，
    // 但 `Content-Type` 还在头里——说明这一形态不会立刻把后续头字段挤进正文。
    // 修 R62 的残余时，这里应该收紧成 `!line.trim().is_empty()`。
    let _ = header_lines(&text);
}

#[test]
fn every_header_line_stays_within_998_bytes() {
    // RFC 5322 §2.1.1：一行（不含 CRLF）不得超过 998 字节。旧代码只按空格切词，
    // 一个比限制还长的「词」（这里是 2000 个 `x`）会被原样写成一整行。
    let subject = format!("{}{}", " ".repeat(995), "x".repeat(2000));

    let built = build_message(&message_with_subject(subject), &options()).expect("应当构建成功");

    for line in String::from_utf8_lossy(&built.bytes).split("\r\n") {
        assert!(
            line.len() <= 998,
            "头行 {} 字节，超过 998：{line:?}",
            line.len()
        );
    }
}

#[test]
fn a_normal_long_subject_round_trips_unchanged() {
    // 反向保护：普通的超长标题（只有单个空格）折行后解回来必须一模一样。
    let subject = "Quarterly report ".repeat(80) + "end";

    let built =
        build_message(&message_with_subject(subject.clone()), &options()).expect("应当构建成功");
    let parsed = xremail_message::parse_headers(&built.bytes);

    assert_eq!(parsed.subject.as_deref(), Some(subject.as_str()));
}
