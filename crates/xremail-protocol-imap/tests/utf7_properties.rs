//! T10 的「随机化」半边：IMAP 邮箱名（modified UTF-7）编解码的不变量测试。
//!
//! 与前两个属性测试同一个思路（`xremail-core/tests/sanitizer_properties.rs`、
//! `xremail-message/tests/parser_properties.rs`）：固定种子的伪随机输入，
//! 撞一组「无论输入是什么都必须成立」的性质。
//!
//! ## 为什么这条路值得单独测
//!
//! 邮箱名**来自服务器**（`LIST` / `LSUB` 的响应），是外部输入。而它是本应用
//! 和服务器之间**唯一**的「按名字认文件夹」的通道：解码错了会去访问不存在的
//! 文件夹（`EXAMINE` 回 `NO`），或者更糟——把信写进错的地方。
//!
//! 编码方向则是用户侧：用户给中文文件夹改名、或者应用要 `CREATE` 一个中文
//! 文件夹时，必须编出服务器认得的形状。
//!
//! ## 被钉住的承诺（来自 `utf7` 的模块文档）
//!
//! 1. **不 panic**：任意字符串，含孤立代理项、超长移位段、畸形 base64。
//! 2. **出错不静默**：非法输入给的是 `Err`，不是 `U+FFFD` 替换后的「看起来
//!    成功」——文档明说「解码错了会导致存进错误的文件夹，比报错更糟」。
//! 3. **往返一致**：`decode(encode(s)) == s`，对任意合法 UTF-8 字符串成立。
//! 4. **编码结果不含裸 `&`**：`&` 必须以 `&-` 或移位段起点的形式出现，
//!    否则服务器那边会把它当成一个没结束的移位段。

use xremail_protocol_imap::utf7::{decode, encode};

/// 确定性伪随机发生器（SplitMix64），与其它属性测试同一实现。
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

    /// 一个随机字符：覆盖 ASCII、中文、emoji（代理对）、控制字符。
    fn character(&mut self) -> char {
        match self.below(6) {
            0 => (b'a' + (self.below(26) as u8)) as char,
            1 => "  \t\r\n./\\&-[]"
                .chars()
                .nth(self.below(12))
                .unwrap_or(' '),
            2 => char::from_u32(0x4E00 + (self.below(0x5000) as u32)).unwrap_or('中'),
            3 => char::from_u32(0x1F300 + (self.below(0x200) as u32)).unwrap_or('😀'),
            4 => char::from_u32(self.below(0x80) as u32).unwrap_or(' '),
            _ => char::from_u32(0x370 + (self.below(0x100) as u32)).unwrap_or('α'),
        }
    }

    fn string(&mut self, len: usize) -> String {
        (0..len).map(|_| self.character()).collect()
    }
}

/// 会在编解码里走不同分支的固定样本。
const SAMPLES: &[&str] = &[
    "",
    "INBOX",
    "Sent Items",
    "已发送",
    "草稿箱",
    "垃圾邮件",
    "&",
    "&-",
    "&XfJT0ZAB-",
    "a&b",
    "a&-b",
    "&XfJT0ZAB",
    "&AAAA-",
    "&/v8-",
    "😀 表情",
    "Mixed 中文 and ASCII",
    "tab\there",
    "line\nbreak",
];

#[test]
fn decoding_never_panics_on_random_input() {
    let mut rng = Rng::new(0x007F_0001);
    for _ in 0..3000 {
        // 一半是「看起来像 modified UTF-7」的随机串，一半是完全随机的字符。
        let input = if rng.below(2) == 0 {
            let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,-&";
            let len = rng.below(40) + 1;
            (0..len)
                .map(|_| {
                    alphabet
                        .chars()
                        .nth(rng.below(alphabet.len()))
                        .unwrap_or('A')
                })
                .collect::<String>()
        } else {
            {
                let n = rng.below(30) + 1;
                rng.string(n)
            }
        };
        let _ = decode(&input); // 不 panic 就是通过
    }
}

#[test]
fn malformed_input_is_an_error_not_a_silent_replacement() {
    // 文档的承诺：非法输入返回明确的错误，**不**替换成 U+FFFD。
    //
    // 这几条是确定的非法形态，必须全部 `Err`：
    for malformed in [
        "&",     // 移位段没结束
        "&ABC",  // 同上
        "&AAAA", // 同上（有内容但没 `-`）
        "&A-",   // 奇数长度（1 个字符 = 6 位，凑不出 UTF-16 的一个单元）
        "&AB-",  // 奇数长度
        "&ff-",  // `f` 不在 modified BASE64 字母表里
    ] {
        let result = decode(malformed);
        assert!(
            result.is_err(),
            "{malformed:?} 应当报错，实际返回 {result:?}——静默替换会让文件夹名悄悄变成别的东西"
        );
        // 错误信息不能为空：它要进日志。
        if let Err(error) = result {
            assert!(!error.to_string().is_empty());
        }
    }
}

#[test]
fn error_never_contains_the_replacement_character() {
    // 反方向的守门：就算某天有人把实现改成「能解就解、解不了兜底」，
    // 也不许用 U+FFFD 糊过去——那正是文档禁止的做法。
    let mut rng = Rng::new(0x007F_0002);
    for _ in 0..2000 {
        let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+,-&xyz";
        let len = rng.below(20) + 1;
        let input: String = (0..len)
            .map(|_| {
                alphabet
                    .chars()
                    .nth(rng.below(alphabet.len()))
                    .unwrap_or('A')
            })
            .collect();
        if let Ok(decoded) = decode(&input) {
            assert!(
                !decoded.contains('\u{FFFD}'),
                "{input:?} 解出了 U+FFFD，说明有非法字节被静默替换了：{decoded:?}"
            );
        }
    }
}

#[test]
fn encode_then_decode_round_trips_for_any_valid_text() {
    // 往返性质。对任意合法 UTF-8 字符串（含中文、emoji、控制字符）都必须成立。
    let mut rng = Rng::new(0x007F_0003);
    let mut cases: Vec<String> = SAMPLES.iter().map(|s| s.to_string()).collect();
    for _ in 0..2000 {
        cases.push({
            let n = rng.below(24);
            rng.string(n)
        });
    }

    for original in cases {
        let encoded = encode(&original);
        let decoded = decode(&encoded).unwrap_or_else(|error| {
            panic!("自己编出来的 {encoded:?} 自己解不开：{error}；原文 {original:?}")
        });
        assert_eq!(
            decoded, original,
            "往返不一致：原文 {original:?} -> 编码 {encoded:?} -> 解码 {decoded:?}"
        );
    }
}

#[test]
fn encoding_never_leaves_a_bare_ampersand() {
    // `&` 必须以 `&-`（字面量）或移位段起点（`&...-`）的形式出现。
    // 裸 `&` 会让服务器把后面的字符当成移位段的开头。
    let mut rng = Rng::new(0x007F_0004);
    let mut cases: Vec<String> = SAMPLES.iter().map(|s| s.to_string()).collect();
    for _ in 0..1500 {
        cases.push({
            let n = rng.below(20);
            rng.string(n)
        });
    }

    for original in cases {
        let encoded = encode(&original);
        let bytes: Vec<char> = encoded.chars().collect();
        for (index, ch) in bytes.iter().enumerate() {
            if *ch != '&' {
                continue;
            }
            let next = bytes.get(index + 1);
            assert!(
                next.is_some(),
                "编码结果以裸 `&` 结尾：{encoded:?}（原文 {original:?}）"
            );
            // `&-` 是转义的字面量 `&`；其它情况必须是移位段，且整串里能找到它的 `-`。
            if next == Some(&'-') {
                continue;
            }
            let rest: String = bytes[index + 1..].iter().collect();
            assert!(
                rest.contains('-'),
                "移位段没有结束符：{encoded:?}（原文 {original:?}）"
            );
        }
    }
}

#[test]
fn encoding_is_pure_ascii() {
    // modified UTF-7 的输出必须全是 ASCII——线路上的邮箱名不能带非 ASCII 字节，
    // 否则服务器那边会按 modified UTF-7 再解一次，得到别的东西。
    let mut rng = Rng::new(0x007F_0005);
    let mut cases: Vec<String> = SAMPLES.iter().map(|s| s.to_string()).collect();
    for _ in 0..1500 {
        cases.push({
            let n = rng.below(20);
            rng.string(n)
        });
    }

    for original in cases {
        let encoded = encode(&original);
        assert!(
            encoded.is_ascii(),
            "编码结果含非 ASCII：{encoded:?}（原文 {original:?}）"
        );
    }
}
