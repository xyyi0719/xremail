//! T10 的「随机化」半边：HTML 消毒器的**不变量**测试。
//!
//! ## 这里做的不是 libFuzzer，而是一件更窄、但能进 CI 的事
//!
//! `cargo-fuzz` 需要 nightly 与 `libfuzzer-sys`，且它跑的是**覆盖率引导的探索**，
//! 结论以「发现了一个 panic」的形式给出——它不进 PR 的 CI（跑一次动辄几分钟，
//! 且结果不固定）。本仓库的约束是「不加依赖、CI 用 stable」，所以 T10 在这里
//! 落地成：**用确定性的伪随机输入去撞一组「无论输入是什么都必须成立」的性质**。
//!
//! 这两者不冲突：fuzz 负责找「没想到的输入」，这里负责钉住「想到了的承诺」。
//! 每一个用例的输入都由**固定种子**生成，所以失败可以逐字复现——这是 fuzz
//! 崩溃样例做不到的事。
//!
//! ## 被钉住的承诺（都来自 `sanitize_html` 的模块文档）
//!
//! 1. **不 panic、不挂起**：任意字节序列（含非法 UTF-8 被替换后的 U+FFFD）。
//! 2. **输出是良构的**：结束标签不越界、不残留未闭合的开标签。
//! 3. **幂等**：`sanitize(sanitize(x)) == sanitize(x)`——消毒结果再走一遍不变，
//!    否则「二次渲染」会悄悄改变内容。
//! 4. **危险构造一个都不剩**：`script` / 事件属性 / `javascript:` / 远程图片。
//!
//! 第 4 条是**安全承诺**，比前三条重要；它的断言写得最直白，
//! 且不依赖消毒器的任何实现细节（只查输出里有没有那个子串）。

use xremail_core::html::sanitize_html;

/// 确定性伪随机发生器（SplitMix64）。
///
/// 为什么不用 `rand`：本 crate **不许加新依赖**，而这里需要的只是
/// 「同一颗种子给同一串字节」。SplitMix64 足够，且只有几行。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        // SplitMix64：先加黄金比例常数，再做三轮移位异或混合。
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, n)` 里的一个数（`n` 为 0 时返回 0）。
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// 构成 HTML 的「危险原子」。
///
/// 故意混入**半截**和**畸形**的写法（`<script` 没有 `>`、属性没引号、
/// 大小写混杂、实体转义的 `<`），因为真正难缠的输入往往不是教科书式的
/// `<script>alert(1)</script>`，而是扫描器边界上的那些。
const DANGEROUS: &[&str] = &[
    "<script>alert(1)</script>",
    "<SCRIPT>alert(1)</SCRIPT>",
    "<ScRiPt src=//evil.example/x.js>",
    "<script",
    "<script>",
    "</script>",
    "<style>body{background:url(javascript:alert(1))}</style>",
    "<iframe src=\"https://evil.example\"></iframe>",
    "<object data=\"evil.swf\"></object>",
    "<embed src=\"evil.swf\">",
    "<svg onload=alert(1)>",
    "<img src=\"https://tracker.example/pixel.gif\">",
    "<img src=x onerror=alert(1)>",
    "<a href=\"javascript:alert(1)\">click</a>",
    "<a href='java\tscript:alert(1)'>click</a>",
    "<a href=\"JaVaScRiPt:alert(1)\">click</a>",
    "<a href=\"#\" onclick=\"alert(1)\">click</a>",
    "<div style=\"background:url(javascript:alert(1))\">x</div>",
    "<div style=\"width:expression(alert(1))\">x</div>",
    "<body onload=alert(1)>",
    "<svg><animate onbegin=alert(1) attributeName=x dur=1s>",
    "<math><mtext><script>alert(1)</script></mtext></math>",
    "<form action=\"javascript:alert(1)\"><input type=submit></form>",
    "<!--<script>alert(1)</script>-->",
    "<![CDATA[<script>alert(1)</script>]]>",
    "&lt;script&gt;alert(1)&lt;/script&gt;",
    "&#60;script&#62;alert(1)&#60;/script&#62;",
    "javascript:alert(1)",
    "onerror=alert(1)",
    "\u{0}<script>\u{0}",
];

/// 良性片段：用来把危险原子夹在中间，制造「危险内容藏在正常结构里」的形态。
const BENIGN: &[&str] = &[
    "<p>hello</p>",
    "<b>bold</b>",
    "<i>italic</i>",
    "<ul><li>item</li></ul>",
    "<table><tr><td colspan=2>cell</td></tr></table>",
    "<a href=\"https://example.com\">ok</a>",
    "<blockquote>quoted</blockquote>",
    "plain text",
    "中文正文",
    "<br>",
    "<hr>",
    "<ol start=3><li>x</li></ol>",
];

/// 直接往扫描器边界上怼的碎片。
const FRAGMENTS: &[&str] = &[
    "<", ">", "/", "\"", "'", "=", " ", "\n", "\t", "\r\n", "&", ";", "//", "\\", "\u{FFFD}",
    "\u{0}", "<>", "</>", "< =", "<<<", ">>>", "<a", "a>", "<a href=", "===", "--", "--!>",
];

/// 每个用例跑多少轮。
///
/// ## 为什么是这个数，而不是"越大越好"
///
/// 整组 6 条用例原先固定 1000–2000 轮，**全部跑完只要 0.07 秒**——
/// 成本低得几乎没有理由跑这么少。默认提到 20 000 轮之后整组约 0.2 秒，
/// 仍然远低于一次 `cargo test` 的编译时间。
///
/// 想更狠地跑一次（比如改了消毒器之后）：设 `XREMAIL_PROPERTY_ROUNDS`。
///
/// ```bash
/// XREMAIL_PROPERTY_ROUNDS=2000000 cargo test -p xremail-core --test sanitizer_properties
/// ```
///
/// 实测 200 万轮约 30 秒——那是**专项验证**的用法，不是日常 CI 的用法。
fn rounds() -> usize {
    std::env::var("XREMAIL_PROPERTY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ROUNDS)
}

/// 默认轮数：平衡「能在 CI 里默认跑」与「真的能撞到东西」。
const DEFAULT_ROUNDS: usize = 20_000;

/// 拼一串随机 HTML。
fn random_html(rng: &mut Rng, atoms: usize) -> String {
    let mut out = String::new();
    for _ in 0..atoms {
        match rng.below(10) {
            0..=4 => out.push_str(rng.pick(DANGEROUS)),
            5..=6 => out.push_str(rng.pick(BENIGN)),
            _ => out.push_str(rng.pick(FRAGMENTS)),
        }
    }
    out
}

/// 输出里**任何位置**都不许出现的危险标签（小写比较）。
///
/// 这些是**标签**：连出现都不该出现，与它是不是在属性里无关。
const FORBIDDEN_TAGS: &[&str] = &[
    "<script", "</script", "<style", "</style", "<iframe", "<object", "<embed", "<svg", "<math",
    "<form", "<input", "<img", "<link", "<meta", "<base", "<frame", "<applet", "<video", "<audio",
    "<source", "<track", "<animate", "<set",
];

/// 危险**协议**：只在「可能被当成可执行位置」时才算危险。
///
/// ## 为什么不能「任何位置都不许出现」
///
/// 一封说明邮件里写着 `javascript:alert(1)` 这几个字，是**正文**，
/// 消毒后原样保留是**正确**的——它就是用户要看到的内容。
/// 真正的危险是它出现在 `href` / `style` 这类**会被执行的属性值**里。
///
/// 所以这里查的是「协议后面跟着 `=` 或出现在属性位置」，而不是裸子串。
/// 这条区分是**安全性**与**可用性**的边界，写窄了会误报（本文件第一版就误报了），
/// 写宽了会漏报真正的注入。
const DANGEROUS_SCHEMES: &[&str] = &["javascript:", "vbscript:", "data:text/html"];

#[test]
fn sanitizing_never_panics_on_random_input() {
    // 2000 组输入，每组最多 40 个原子（约 8 万个片段）。
    // 本机实测这一条不到 1 秒，进 CI 不会拖慢。
    let mut rng = Rng::new(0x5EED_0001);
    for _ in 0..rounds() {
        let atoms = rng.below(40) + 1;
        let input = random_html(&mut rng, atoms);
        let _ = sanitize_html(&input); // 不 panic 就是通过
    }
}

#[test]
fn dangerous_constructs_never_survive() {
    // 这是**安全承诺**，优先级最高：无论输入怎么拼，输出里都不许有这些东西。
    let mut rng = Rng::new(0x5EED_0002);
    for round in 0..rounds() {
        let atoms = rng.below(40) + 1;
        let input = random_html(&mut rng, atoms);
        let output = sanitize_html(&input).to_ascii_lowercase();
        for needle in FORBIDDEN_TAGS {
            assert!(
                !output.contains(needle),
                "第 {round} 轮：消毒后的输出里仍有 {needle:?}\n输入：{input:?}\n输出：{output:?}"
            );
        }
        assert_no_dangerous_scheme(&output, &input, round);
    }
}

/// 断言输出里没有「落在可执行位置上」的危险协议。
///
/// ## 判据：只在**标签内部**、作为**属性值**出现才算
///
/// 第一版按「`javascript:` 前面紧挨 `=`」判，结果 `&lt;&gt;onerror=...` 这种
/// **被转义成正文**的东西也被算了进去（`&gt;onerror=alert(1)` 里的 `=`）。
/// 第二版按 `url(` 判，又把正文里的 `url(javascript:...)` 算成了危险。
///
/// 真正的判据只能是：**这个位置浏览器会不会把它当代码执行**。那就是——
/// 处在某个 `<tag ...>` 内部，且是某个属性的值。所以这里真的把输出的标签
/// 扫一遍，逐个属性看它的值，而不是在整段字符串里找子串。
fn assert_no_dangerous_scheme(output_lower: &str, input: &str, round: usize) {
    for (attr, value) in tag_attribute_values(output_lower) {
        let value = value.trim_start().to_ascii_lowercase();
        let compact: String = value
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '\u{0}')
            .collect();
        for scheme in DANGEROUS_SCHEMES {
            assert!(
                !compact.starts_with(scheme),
                "第 {round} 轮：属性 {attr:?} 的值以 {scheme:?} 开头（浏览器会执行它）\n输入：{input:?}\n输出：{output_lower:?}"
            );
        }
        // `style` 里的 `url(...)` / `expression(...)` 也是可执行位置。
        for marker in ["url(", "expression("] {
            if let Some(at) = compact.find(marker) {
                let rest = &compact[at + marker.len()..];
                for scheme in DANGEROUS_SCHEMES {
                    assert!(
                        !rest.starts_with(scheme) && !rest.starts_with('"'),
                        "第 {round} 轮：{attr:?} 里有 {marker}{scheme:?}\n输入：{input:?}\n输出：{output_lower:?}"
                    );
                }
            }
        }
    }
}

/// 取出输出里所有标签的属性 `(名字, 值)`。
///
/// 只认 `=` 连接的、值被引号包住的形态——消毒器的输出总是这样，
/// 无引号的值是它绝不会产生的形态（那正是它要消灭的东西）。
fn tag_attribute_values(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        // 标签体的终点：`>`（不在引号里）。
        let start = index + 1;
        let mut cursor = start;
        let mut quote: Option<u8> = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            match quote {
                Some(q) if byte == q => quote = None,
                Some(_) => {}
                None if byte == b'"' || byte == b'\'' => quote = Some(byte),
                None if byte == b'>' => break,
                None => {}
            }
            cursor += 1;
        }
        let tag_body = &html[start..cursor.min(bytes.len())];
        // 去掉标签名，然后按 `名字="值"` 抽。
        let mut rest = tag_body;
        if let Some(position) = rest.find(|c: char| c.is_whitespace() || c == '/') {
            rest = &rest[position..];
        } else {
            rest = "";
        }
        for piece in rest
            .split('"')
            .map(str::to_string)
            .collect::<Vec<_>>()
            .chunks(2)
        {
            // 奇数段是引号里的值；偶数段里藏着下一个属性名。
            if piece.len() == 2 {
                let name_part = &piece[0];
                let value = piece[1].clone();
                let name = name_part
                    .trim()
                    .split('=')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    out.push((name, value));
                }
            }
        }
        index = cursor + 1;
    }
    out
}

#[test]
fn no_event_handler_attribute_survives() {
    // 单独查 `on*=`：上面的黑名单按标签查，这里按属性查，两者互为补集。
    let mut rng = Rng::new(0x5EED_0003);
    for round in 0..rounds() {
        let atoms = rng.below(40) + 1;
        let input = random_html(&mut rng, atoms);
        let output = sanitize_html(&input).to_ascii_lowercase();
        for (name, _) in tag_attribute_values(&output) {
            assert!(
                !name.starts_with("on"),
                "第 {round} 轮：输出里仍有事件属性 {name:?}\n输入：{input:?}\n输出：{output:?}"
            );
        }
    }
}

#[test]
fn sanitizing_is_idempotent() {
    // 二次消毒必须不变。这不是洁癖：邮件正文会被「消毒 → 存库 → 再读出来显示」，
    // 若第二步会改变内容，就会出现「存的和显示的不是同一段」。
    let mut rng = Rng::new(0x5EED_0004);
    for round in 0..rounds() {
        let atoms = rng.below(30) + 1;
        let input = random_html(&mut rng, atoms);
        let once = sanitize_html(&input);
        let twice = sanitize_html(&once);
        assert_eq!(
            once, twice,
            "第 {round} 轮：消毒两次结果不同\n输入：{input:?}\n一次：{once:?}\n两次：{twice:?}"
        );
    }
}

#[test]
fn output_is_always_well_formed() {
    // 良构的定义（与模块文档一致）：
    // * 不残留未闭合的开标签——除了消毒器**刻意**自闭合的 `br` / `hr`；
    // * 不出现只有结束、没有对应的开标签。
    //
    // 用最朴素的方式数：按 `<name` 与 `</name>` 的出现次数比对。
    // 这依赖「输出总是良构」这条实现承诺，但**不依赖**它是怎么实现的。
    const VOID: &[&str] = &["br", "hr"];
    let mut rng = Rng::new(0x5EED_0005);
    for round in 0..rounds() {
        let atoms = rng.below(30) + 1;
        let input = random_html(&mut rng, atoms);
        let output = sanitize_html(&input);
        let lowered = output.to_ascii_lowercase();

        for tag in [
            "p",
            "b",
            "i",
            "u",
            "s",
            "ul",
            "ol",
            "li",
            "div",
            "span",
            "blockquote",
            "pre",
            "code",
            "table",
            "tr",
            "td",
            "th",
            "h1",
            "h2",
            "h3",
            "a",
            "em",
            "strong",
        ] {
            if VOID.contains(&tag) {
                continue;
            }
            let opens = count_tag_opens(&lowered, tag);
            let closes = count_tag_closes(&lowered, tag);
            assert_eq!(
                opens, closes,
                "第 {round} 轮：<{tag}> 开 {opens} 次、闭 {closes} 次，输出不良构\n输入：{input:?}\n输出：{output:?}"
            );
        }
    }
}

/// 数 `<tag` 开头（后面必须跟空白、`>` 或 `/`）的次数。
fn count_tag_opens(html: &str, tag: &str) -> usize {
    let needle = format!("<{tag}");
    html.match_indices(&needle)
        .filter(|(index, _)| {
            let next = html[index + needle.len()..].chars().next();
            match next {
                Some('>') | Some('/') => true,
                Some(c) => c.is_whitespace(),
                None => false,
            }
        })
        // 排掉 `</tag>` 的误命中（`</p>` 会命中 `</p`，但 needle 是 `<p`，
        // 不会匹配 `</p`，这里只是防 `a<p` 这种边界，保留判断更稳）。
        .count()
}

fn count_tag_closes(html: &str, tag: &str) -> usize {
    let needle = format!("</{tag}");
    html.match_indices(&needle)
        .filter(|(index, _)| {
            let next = html[index + needle.len()..].chars().next();
            match next {
                Some('>') => true,
                Some(c) => c.is_whitespace(),
                None => false,
            }
        })
        .count()
}

#[test]
fn every_single_dangerous_atom_is_neutralised() {
    // 上面几条是「随机拼」。这一条反过来：**每个原子单独喂**，逐个确认。
    // 随机测试有漏网的可能（某个原子恰好没被抽到），这条保证没有漏网的原子。
    for atom in DANGEROUS {
        let output = sanitize_html(atom).to_ascii_lowercase();
        for needle in FORBIDDEN_TAGS {
            assert!(
                !output.contains(needle),
                "原子 {atom:?} 消毒后仍有 {needle:?}，输出：{output:?}"
            );
        }
        assert_no_dangerous_scheme(&output, atom, 0);
    }
}
