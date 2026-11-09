//! T10 的「随机化」半边：MIME 解析器的**不变量**测试。
//!
//! 与 [`xremail-core` 的消毒器属性测试](../../xremail-core/tests/sanitizer_properties.rs)
//! 同一套思路：用**固定种子**的伪随机字节去撞一组「无论输入是什么都必须成立」
//! 的性质。失败的输入可以逐字复制进用例复现，这是 fuzz 崩溃样例给不了的。
//!
//! ## 为什么 MIME 值得这么测
//!
//! 解析器的输入**完全来自外部**（服务器、发件人），且历史上有过实打实的事故：
//! 部件号与 IMAP 口径不一致导致附件取错段（R44）、`from_utf8_lossy` 把 GBK
//! 正文换成 U+FFFD、段尾多带一个换行。这些都不是「写错了逻辑」，而是
//! **在边界输入上**才暴露的问题。
//!
//! ## 被钉住的承诺
//!
//! 1. **不 panic、不挂起**：任意字节，包括非法的 UTF-8、截断的 base64、
//!    声明长度与实际不符的 multipart。
//! 2. **附件条数有上限**：一封塞几千个 part 的邮件不能把内存吃满
//!    （`MAX_ATTACHMENT_PARTS`）。
//! 3. **部件号不重复**：每个附件有唯一的部件号，否则下载会取错段。
//! 4. **出错只用契约里的错误**：要么 `Ok`，要么 `Err(MessageError::Undecodable)`——
//!    不能有第三种「半解析」的形态。

use xremail_message::{MAX_ATTACHMENT_PARTS, MessageError, parse_headers, parse_message};

/// 确定性伪随机发生器（SplitMix64），与 core 侧同一个实现。
///
/// 重复一份而不是抽成公共 crate：测试用的几行东西不值得为它新增一个依赖
/// （本仓库的约束是「不加依赖」）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// 每个用例跑多少轮（`XREMAIL_PROPERTY_ROUNDS` 可覆盖）。
///
/// 默认值是按「整组跑完仍远快于编译时间」选的。要更狠地扫一遍：
///
/// ```bash
/// XREMAIL_PROPERTY_ROUNDS=2000000 cargo test -p xremail-message --test parser_properties --release
/// ```
fn rounds() -> usize {
    std::env::var("XREMAIL_PROPERTY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROUNDS)
}

/// 默认轮数：能在 CI 里默认跑，又真能撞到东西。
const DEFAULT_ROUNDS: usize = 20_000;

/// 结构正确的 multipart 片段，用来当「骨架」。
const SKELETON: &[&str] = &[
    "Content-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n",
    "Content-Type: text/html; charset=gb2312\r\n\r\n<p>中文</p>\r\n",
    "Content-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n",
    "--boundary\r\n",
    "--boundary--\r\n",
    "\r\n",
    "\n",
    "\r",
    "Content-Type: multipart/mixed; boundary=\"boundary\"\r\n\r\n",
    "Content-Type: message/rfc822\r\n\r\n",
];

/// 会破坏结构的碎屑。
const JUNK: &[&str] = &[
    "Content-Type:",
    "boundary=",
    "--",
    "--boundary",
    "charset=",
    "=?utf-8?B?",
    "?=",
    ";",
    ":",
    "\"",
    "'",
    "=",
    "<",
    ">",
    "\u{FFFD}",
    "\u{0}",
    "base64",
    "Content-Disposition: attachment",
    "filename=",
    "name=",
    "text/plain",
    "multipart/mixed",
    "\r\n\r\n",
];

/// 拼一封随机邮件（字节形态，允许非法 UTF-8）。
fn random_message(rng: &mut Rng, atoms: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for _ in 0..atoms {
        match rng.below(8) {
            0..=4 => out.extend_from_slice(rng.pick(SKELETON).as_bytes()),
            5..=6 => out.extend_from_slice(rng.pick(JUNK).as_bytes()),
            // 直接怼原始字节：非法 UTF-8、全 0xFF、折行。
            7 => {
                let len = rng.below(8) + 1;
                for _ in 0..len {
                    out.push(rng.byte());
                }
            }
            _ => {}
        }
    }
    out
}

#[test]
fn parsing_never_panics_on_random_bytes() {
    let mut rng = Rng::new(0xB16_0001);
    for _ in 0..rounds() {
        let atoms = rng.below(40) + 1;
        let raw = random_message(&mut rng, atoms);
        // 不 panic 就是通过；结果本身（Ok/Err）两种都合法。
        let _ = parse_message(&raw);
        let _ = parse_headers(&raw);
    }
}

#[test]
fn attachments_never_exceed_the_cap() {
    // 一封塞了几千个 part 的邮件：解析侧必须自己刹车（模块文档里
    // `MAX_ATTACHMENT_PARTS` 明写了这是防内存吃满的硬上限）。
    let mut rng = Rng::new(0xB16_0002);
    let mut worst = 0;
    // **这一条不跟 `rounds()`**：它每轮要生成最多 3000 个原子（模拟「一封塞了几千个
    // part 的邮件」），单轮成本是别的用例的上百倍。按统一轮数跑 20 000 轮要 34 秒，
    // 而它要证明的东西（「解析侧会不会在超限时刹车」）几百轮就能撞出来。
    // 想加大只需 `XREMAIL_PROPERTY_ROUNDS`——这个值会跟着放大，但跑之前先想清楚时间。
    let heavy_rounds = (rounds() / 50).max(200);
    for _ in 0..heavy_rounds {
        let atoms = rng.below(3000) + 1;
        let raw = random_message(&mut rng, atoms);
        if let Ok(parsed) = parse_message(&raw) {
            worst = worst.max(parsed.attachments.len());
            assert!(
                parsed.attachments.len() <= MAX_ATTACHMENT_PARTS,
                "附件条数 {} 超过上限 {MAX_ATTACHMENT_PARTS}",
                parsed.attachments.len()
            );
        }
    }
    // 顺带记录一下实际撞到的最大值，方便判断这条用例是不是「真的在测上限」。
    assert!(worst <= MAX_ATTACHMENT_PARTS);
}

#[test]
fn attachment_part_numbers_are_unique() {
    // 部件号是下载附件时的寻址依据（R44 的教训：口径错一位就取错段）。
    // 同一封信里出现两个相同的部件号，说明编号逻辑在某个边界上重了。
    let mut rng = Rng::new(0xB16_0003);
    for round in 0..rounds() {
        let atoms = rng.below(60) + 1;
        let raw = random_message(&mut rng, atoms);
        let Ok(parsed) = parse_message(&raw) else {
            continue;
        };
        let mut seen: Vec<&str> = Vec::new();
        for attachment in &parsed.attachments {
            let path = attachment.part_number.as_str();
            assert!(
                !seen.contains(&path),
                "第 {round} 轮：部件号 {path:?} 重复出现\n原始输入：{:?}",
                String::from_utf8_lossy(&raw)
            );
            seen.push(path);
        }
    }
}

#[test]
fn errors_stay_within_the_contract() {
    // 只有一种错误变体（`Undecodable`）。这条用例的价值在于：如果将来有人
    // 往 `MessageError` 里加了会「泄漏内部状态」的变体，这里会提醒他先想清楚
    // 上层（同步引擎）能不能处理。
    let mut rng = Rng::new(0xB16_0004);
    for _ in 0..rounds() {
        let atoms = rng.below(30) + 1;
        let raw = random_message(&mut rng, atoms);
        if let Err(error) = parse_message(&raw) {
            assert!(
                matches!(error, MessageError::Undecodable(_)),
                "出现了契约之外的错误变体：{error:?}"
            );
            // 错误信息不能为空——它会被写进日志给排障的人看。
            assert!(!error.to_string().is_empty(), "错误信息不能是空串");
        }
    }
}

#[test]
fn base64_style_mimetype_labels_round_trip_without_panic() {
    // RFC 2047 编码词与 charset 标签是最容易出问题的一类输入：
    // 截断的 `=?utf-8?B?`、不存在的 charset 名、超长的标签。
    let labels = [
        "=?utf-8?B?5Lit5paH?=",
        "=?utf-8?B?",
        "=?gb2312?B?xOO6ww==?=",
        "=?x-unknown-charset?Q?abc?=",
        "=?utf-8?Q?a_b?=",
        "=?=?=?",
        "charset=not-a-real-charset",
        "charset=",
        "charset=\"\"",
    ];
    let mut rng = Rng::new(0xB16_0005);
    for _ in 0..rounds() {
        let label = rng.pick(&labels);
        let raw = format!(
            "Subject: {label}\r\nContent-Type: text/plain; charset={label}\r\n\r\nbody {label}"
        );
        let _ = parse_message(raw.as_bytes());
        let _ = parse_headers(raw.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// 下面两条是**回归用例**，钉住属性测试撞出来的一个真 bug。
// ---------------------------------------------------------------------------

#[test]
fn a_forwarded_message_with_two_attachments_gets_distinct_part_numbers() {
    // 这是「整封转发（作为附件）」的形态：顶层就是 `message/rfc822`。
    //
    // 曾经的行为：两条附件都被记成 `"TEXT"`。根因在 `attachment_meta`——
    // 它用「顶层是不是 `multipart/*`」决定要不要丢路径，而顶层是
    // `message/rfc822` 时那个判定为假，于是**整条路径被清空**，
    // 内嵌邮件里各个子部件的号全塌成了 `TEXT`。
    //
    // 为什么这不只是「编号难看」：`xremail-sync` 的附件 id 是
    // `{message_id}/{part_number}`，落库时靠 `ON CONFLICT (message_id, part_key)`
    // 去重。两条都是 `TEXT` 就是**同一个 key**——两个附件在库里互相顶掉，
    // 用户只能看到一个，而且点下载取到的是哪一个还取决于插入顺序。
    let raw = b"Content-Type: message/rfc822\r\n\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\n\r\n\
AAA\r\n\
--b\r\n\
Content-Type: image/png\r\n\
Content-Disposition: attachment; filename=\"b.png\"\r\n\r\n\
BBB\r\n\
--b--\r\n";

    let parsed = parse_message(raw).expect("这封邮件结构是合法的，必须能解析");
    let numbers: Vec<&str> = parsed
        .attachments
        .iter()
        .map(|a| a.part_number.as_str())
        .collect();
    assert_eq!(numbers.len(), 2, "两个附件都要在库里：{numbers:?}");
    assert_ne!(
        numbers[0], numbers[1],
        "两个附件不能共用同一个部件号（否则落库时会互相顶掉）：{numbers:?}"
    );
}

#[test]
fn a_plain_single_part_message_is_still_text() {
    // 另一头不能被修坏：整封就是一个部件时，仍然记 `TEXT`
    // （`BODY.PEEK[0]` 只返回结构，用 `1` 又会与「顶层 multipart 占 1 号」混淆）。
    let raw = b"Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"only.pdf\"\r\n\r\n\
AAA\r\n";
    let parsed = parse_message(raw).expect("单部件邮件必须能解析");
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].part_number, "TEXT");
}
