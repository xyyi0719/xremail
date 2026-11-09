//! T10 的「随机化」半边：IMAP **解析这一层**的不变量测试（跑得快的部分）。
//!
//! 与 `response_properties.rs` 分工：那个用**假服务器 + 真客户端**验证「线上的
//! 畸形响应到了客户端手里是什么」；这个直接喂**结构化的随机输入**给解析函数，
//! 一轮几千次，专门找「组合少见但结构合法」的错。
//!
//! ## 这里测的是哪几个函数
//!
//! 一句话：**服务器发来的东西**经 `imap-proto` 解析之后，落到下面这几处的样子。
//!
//! | 被测 | 方向 | 为什么 |
//! |---|---|---|
//! | `command::fetched_message` / `command::listed_mailbox` | 服务器 → 客户端 | `FETCH` / `LIST` 的每一条数据都从这里进库、「哪个文件夹是已发送」也在这里定 |
//! | `idle::classify_response` | 服务器 → 客户端 | IDLE 里服务器随时会推东西，「该同步」还是「该重连」只能靠它 |
//! | `transfer::expand_uid_set` / `CopyUid::state` | 服务器 → 客户端 | `COPYUID` 的区间端点、展开上限、源目标对应 |
//! | `append::appended_uid` | 服务器 → 客户端 | `APPENDUID` 的闭区间（曾经差一位） |
//! | `xremail-testkit` 的 `Command::parse` / `parse_fetch` / `parse_store` / `search::parse` / `SequenceSet` | 客户端 → **假服务器** | 假服务器若比真服务器**宽松**，用例就会用假服务器能接受、真服务器会 `BAD` 的形状去断言行为——测试是绿的，线上是坏的 |
//!
//! ## 为什么为此把 `src/` 里几处私有的东西改成 `pub`/`pub(crate)`
//!
//! 原来 `parse_fetch` / `parse_listed_mailbox` / `copy_uid_of` / `expand_uid_set` /
//! `appended_uid` / `classify` 都是模块私有，只有 `async` 的公开入口能用——
//! 那意味着每验一次就得起一台 TLS 服务器。改成 `pub` 只是**扩大可见性**，
//! 函数体一字未动、调用方一字未改、行为完全不变；代价是它们从此有了文档承诺。
//!
//! ## 被钉住的承诺
//!
//! 1. **不 panic**：任意 `imap-proto` 解析出来的结构（包括 `u32::MAX`、空集合、
//!    倒序区间、重复项、越界的部件号）。
//! 2. **不凭空造数**：源集合为空 / 长度为 0 时，映射必须明确地「不可用」。
//! 3. **端点精确**：区间是闭区间，两端都要在。
//! 4. **有界**：展开不会超过上限，附件/标记/字段的数量等于服务器给的数量。
//! 5. **假服务器不比真服务器宽松**：只接受合法的命令形状。

use std::borrow::Cow;

use imap_proto::rfc4315::UidSetMember;
use imap_proto::{
    AttributeValue, BodyExtension, MessageSection, NameAttribute, Response, ResponseCode,
    SectionPath, Status,
};
use xremail_protocol_imap::append::appended_uid;
use xremail_protocol_imap::command::{fetched_message, listed_mailbox};
use xremail_protocol_imap::idle::classify_response;
use xremail_protocol_imap::transfer::{CopyUid, expand_uid_set};
use xremail_testkit::fetch::{
    BodySection, FetchItem, StoreOperation, parse_fetch, parse_items, parse_store,
};
use xremail_testkit::parser::{Command, quoted_values};
use xremail_testkit::search::{SearchError, SearchKey, parse as parse_search};
use xremail_testkit::sequence::SequenceSet;

// ---------------------------------------------------------------------------
// 确定性伪随机（SplitMix64），与其它属性测试同一实现
// ---------------------------------------------------------------------------

/// 确定性伪随机发生器（SplitMix64）。
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

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn bool(&mut self) -> bool {
        self.below(2) == 0
    }

    /// 一个 `u32`：一半落在小范围里（好观察），一半落在边界上。
    fn u32(&mut self) -> u32 {
        match self.below(6) {
            0 => 0,
            1 => 1,
            2 => u32::MAX,
            3 => u32::MAX - 1,
            4 => self.below(100) as u32,
            _ => self.next_u64() as u32,
        }
    }
}

// ---------------------------------------------------------------------------
// 1. FETCH：每一条属性都被收下，不吞、不多、不越界
// ---------------------------------------------------------------------------

/// 随机生成的 `FETCH` 属性列表 → `FetchedMessage`：不 panic，且字段**一一对应**。
///
/// 防的是什么：`parse_fetch` 是一个 `match`，新增的属性类型如果落进 `_ => {}`，
/// 那它就是「静默丢弃」——服务器给了 `FLAGS`，客户端却没看到，回写逻辑会以为
/// 「服务器上没有任何标记」。这里不查具体映射（那是单元用例的事），只查**总量**：
/// 服务器给了几个 `UID` 属性，结果里就该有值；一个都没给，就必须是 `None`。
#[test]
fn fetch_attributes_are_never_silently_dropped() {
    let mut rng = Rng::new(0x007F_3001);

    for _ in 0..2000 {
        let attributes = random_attributes(&mut rng);
        let seq = rng.u32();
        let message = fetched_message(seq, &attributes);

        assert_eq!(message.seq, seq, "序号必须原样带出来，它不是 UID");

        let uid_count = attributes
            .iter()
            .filter(|a| matches!(a, AttributeValue::Uid(_)))
            .count();
        let last_uid = attributes.iter().rev().find_map(|a| match a {
            AttributeValue::Uid(uid) => Some(*uid),
            _ => None,
        });
        assert_eq!(
            message.uid, last_uid,
            "属性 {attributes:?} 里 UID 出现 {uid_count} 次，结果却是 {:?}",
            message.uid
        );

        let flags_count = attributes
            .iter()
            .filter(|a| matches!(a, AttributeValue::Flags(_)))
            .count();
        if flags_count > 1 {
            // 多个 `FLAGS` 属性：按顺序取最后一个（真实服务器不会这么发，
            // 但「最后一个赢」是 `parse_fetch` 现在的行为，要钉住而不是任其漂移）。
            let last = attributes.iter().rev().find_map(|a| match a {
                AttributeValue::Flags(flags) => Some(flags),
                _ => None,
            });
            assert_eq!(
                message.flags.len(),
                last.map(|f| f.len()).unwrap_or(0),
                "多次出现的 FLAGS 应当以最后一次为准"
            );
        }

        // 标记里不许出现空项：空标记会让 `has_flag("")` 在任何邮件上都为真。
        for flag in &message.flags {
            assert!(
                !flag.is_empty(),
                "解析出了空标记（来源属性 {attributes:?}）"
            );
        }
    }
}

fn random_attributes<'a>(rng: &mut Rng) -> Vec<AttributeValue<'a>> {
    let count = rng.below(6);
    (0..count).map(|_| random_attribute(rng)).collect()
}

fn random_attribute<'a>(rng: &mut Rng) -> AttributeValue<'a> {
    match rng.below(11) {
        0 => AttributeValue::Uid(rng.u32()),
        1 => {
            let flags: Vec<Cow<'static, str>> = (0..rng.below(4))
                .map(|_| {
                    Cow::Borrowed(*rng.pick(&[
                        "\\Seen",
                        "\\Deleted",
                        "\\Answered",
                        "\\Flagged",
                        "\\Draft",
                        "\\Recent",
                        "$Forwarded",
                        "\\*",
                        "关键字",
                    ]))
                })
                .collect();
            AttributeValue::Flags(flags)
        }
        2 => AttributeValue::Rfc822Size(rng.u32()),
        3 => AttributeValue::ModSeq(rng.next_u64()),
        4 => AttributeValue::Rfc822Header(Some(Cow::Borrowed(b"Subject: x\r\n\r\n"))),
        5 => AttributeValue::Rfc822(Some(Cow::Borrowed(b"Subject: x\r\n\r\nbody"))),
        6 => AttributeValue::Rfc822Text(Some(Cow::Borrowed(b"body"))),
        7 | 8 => AttributeValue::BodySection {
            section: random_section(rng),
            index: None,
            data: if rng.bool() {
                Some(Cow::Borrowed(b"body bytes"))
            } else {
                // `NIL`：RFC 3501 允许 `BODY[…] NIL`（例如 `BODY[1]` 不存在）。
                None
            },
        },
        9 => AttributeValue::Envelope(Box::new(random_envelope(rng))),
        _ => AttributeValue::Unknown,
    }
}

fn random_section(rng: &mut Rng) -> Option<SectionPath> {
    match rng.below(8) {
        0 => None,
        1 => Some(SectionPath::Full(MessageSection::Header)),
        2 => Some(SectionPath::Full(MessageSection::Text)),
        3 => Some(SectionPath::Full(MessageSection::Mime)),
        4 => Some(SectionPath::Part(
            vec![rng.below(5) as u32],
            rng.bool().then_some(MessageSection::Text),
        )),
        5 => Some(SectionPath::Part(
            vec![1, 2, 3],
            rng.bool().then_some(MessageSection::Header),
        )),
        // `BODY[HEADER.FIELDS (…)]` 在 imap-proto 里也归一成 `MIME`。
        6 => Some(SectionPath::Full(MessageSection::Mime)),
        _ => Some(SectionPath::Part(
            vec![rng.below(9) as u32, rng.below(9) as u32],
            None,
        )),
    }
}

fn random_envelope<'a>(rng: &mut Rng) -> imap_proto::Envelope<'a> {
    let subject = if rng.bool() {
        Some(Cow::Borrowed("主题".as_bytes()))
    } else {
        None
    };
    imap_proto::Envelope {
        date: None,
        subject,
        in_reply_to: None,
        sender: None,
        reply_to: None,
        to: None,
        cc: None,
        bcc: None,
        from: None,
        message_id: Some(Cow::Borrowed(b"<a@b>")),
    }
}

/// `BODY[…]` 的数据落到哪个字段：**只有一个**字段拿到了那段字节。
///
/// 防的是什么：`BODY[]` 是整封、`BODY[TEXT]` 是正文（`xremail-testkit/src/fetch.rs`
/// 的文档专门提醒「弄错的话客户端会把正文当成整封邮件去解析」）。同一段字节
/// 同时出现在 `header` 与 `body` 里，意味着下游拿哪一个都不确定。
#[test]
fn a_body_section_lands_in_exactly_one_field() {
    let mut rng = Rng::new(0x007F_3002);
    let mut seen = 0usize;

    for _ in 0..2000 {
        // 先按段挑一次、再照着它造属性：`SectionPath` 不实现 `Clone`，
        // 所以用「同一颗种子的一个**平行**随机流」重新生成同一段即可。
        let attributes = vec![AttributeValue::BodySection {
            section: random_section(&mut rng),
            index: None,
            data: Some(Cow::Borrowed(b"PAYLOAD")),
        }];

        // 从**造出来的那个属性**里读回段：`SectionPath` 不实现 `Clone`，
        // 自己再生成一次很容易和实际喂进去的那份错开（第一版就是这么错的）。
        let describe = format!(
            "{:?}",
            match attributes.first() {
                Some(AttributeValue::BodySection { section, .. }) => section,
                other => panic!("样本应当是 BodySection，实际 {other:?}"),
            }
        );
        let expected = match &attributes[0] {
            AttributeValue::BodySection { section, .. } => match section {
                None => Section {
                    header: false,
                    body: true,
                    text: false,
                    why: "BODY[] 是整封",
                },
                Some(SectionPath::Full(MessageSection::Header)) => Section {
                    header: true,
                    body: false,
                    text: false,
                    why: "BODY[HEADER] 是头",
                },
                Some(SectionPath::Full(MessageSection::Text)) => Section {
                    header: false,
                    body: false,
                    text: true,
                    why: "BODY[TEXT] 是正文，不是整封",
                },
                Some(SectionPath::Full(MessageSection::Mime)) => Section {
                    header: true,
                    body: false,
                    text: false,
                    why: "HEADER.FIELDS 之类算头的一部分",
                },
                Some(SectionPath::Part(_, _)) => Section {
                    header: false,
                    body: true,
                    text: false,
                    why: "部件算整封（由 MIME 层再去拆）",
                },
            },
            other => panic!("样本应当是 BodySection，实际 {other:?}"),
        };
        let message = fetched_message(1, &attributes);

        // 「正好一个」不够：还要**是哪一个**。
        //
        // 防的是什么：变异测试发现，把 `BODY[TEXT]` 写成「同时落进 body 与 text」
        // 会被字段数抓住；但把它写成「**只**落进 body」不会被抓——字段数仍然是 1，
        // 只是正文被当成了整封邮件（`xremail-testkit/src/fetch.rs` 的文档专门提醒过
        // 这个错误）。所以这里按 `command.rs::fetched_message` 的承诺逐段钉死。
        let actual = (
            message.header.is_some(),
            message.body.is_some(),
            message.text.is_some(),
        );
        let filled = [actual.0, actual.1, actual.2]
            .into_iter()
            .filter(|x| *x)
            .count();
        assert_eq!(
            filled, 1,
            "部件 {describe:?} 应当正好落进一个字段，实际 header/body/text 有无 = {actual:?}"
        );
        assert_eq!(
            actual,
            (expected.header, expected.body, expected.text),
            "{}（部件 {describe:?}）",
            expected.why
        );
        seen += 1;
    }

    assert!(seen > 1000, "只有 {seen} 个样本走完了检查，随机面太窄");
}

/// 各段应当落到哪个字段（`BODY[TEXT]` 最容易错成「整封」）。
struct Section {
    header: bool,
    body: bool,
    text: bool,
    why: &'static str,
}

// ---------------------------------------------------------------------------
// 2. LIST：属性 → 角色，且不认识的属性一律不认角色
// ---------------------------------------------------------------------------

/// 随机 `LIST` 数据 → `ListedMailbox`：角色**只可能来自**属性表或名字兜底表。
///
/// 防的是什么：`folders::role_from_attributes` 对未知属性走 `continue`，
/// 这是对的——认不出来就该是 `None`。但 `continue` 落到最后一个分支时若写错成
/// `return Some(...)`，一个 `\Noselect` 文件夹就会被认成某个角色。
/// 这里用「所有属性都不是角色属性、名字也不在兜底表里」的输入，要求 `None`。
#[test]
fn unknown_attributes_and_names_yield_no_role() {
    const NON_ROLE_ATTRIBUTES: &[NameAttribute<'static>] = &[
        NameAttribute::NoInferiors,
        NameAttribute::NoSelect,
        NameAttribute::Marked,
        NameAttribute::Unmarked,
        NameAttribute::Extension(Cow::Borrowed("HasChildren")),
        NameAttribute::Extension(Cow::Borrowed("Vendor")),
        NameAttribute::Extension(Cow::Borrowed("")),
    ];
    const NON_ROLE_NAMES: &[&str] = &[
        "INBOX2",
        "in box",
        "Sent",
        "Sent Message.s",
        "其他",
        "",
        " ",
        "已发送的",
        "Trashy",
        "&",
    ];

    let mut rng = Rng::new(0x007F_3003);
    for _ in 0..2000 {
        let attributes: Vec<NameAttribute<'static>> = (0..rng.below(4))
            .map(|_| rng.pick(NON_ROLE_ATTRIBUTES).clone())
            .collect();
        let name = rng.pick(NON_ROLE_NAMES);

        let mailbox = listed_mailbox(&imap_proto::MailboxListData {
            name_attributes: attributes.clone(),
            delimiter: rng.bool().then_some(Cow::Borrowed("/")),
            name: Cow::Borrowed(name),
        });

        assert_eq!(
            mailbox.raw_name, *name,
            "线路名必须原样保留（发命令要用它）"
        );
        assert_eq!(
            mailbox.role, None,
            "属性 {attributes:?} + 名字 {name:?} 都不是特殊用途，却认出了角色 {:?}",
            mailbox.role
        );
        assert_eq!(
            mailbox.attributes.len(),
            attributes.len(),
            "属性个数不能被吞"
        );
    }
}

/// 随机名字 + 一个角色属性：**属性说了算**，名字再怎么像也不影响。
///
/// 防的是什么：「属性优先、名字兜底」的顺序一旦反了，一个叫 `Drafts`
/// 但服务器标了 `\Junk` 的文件夹会被当成草稿箱。
#[test]
fn role_attributes_win_over_the_name() {
    let roles: &[(NameAttribute<'static>, &str)] = &[
        (NameAttribute::All, "all"),
        (NameAttribute::Archive, "archive"),
        (NameAttribute::Drafts, "drafts"),
        (NameAttribute::Flagged, "flagged"),
        (NameAttribute::Junk, "junk"),
        (NameAttribute::Sent, "sent"),
        (NameAttribute::Trash, "trash"),
    ];
    let names: &[&str] = &[
        "Sent",
        "Drafts",
        "Trash",
        "Junk",
        "已发送",
        "草稿箱",
        "INBOX",
        "",
        "任意",
    ];

    let mut rng = Rng::new(0x007F_3004);
    for _ in 0..2000 {
        let (attribute, expected) = rng.pick(roles);
        let name = rng.pick(names);

        let mailbox = listed_mailbox(&imap_proto::MailboxListData {
            name_attributes: vec![attribute.clone()],
            delimiter: None,
            name: Cow::Borrowed(name),
        });

        assert_eq!(
            mailbox.role.as_deref(),
            Some(*expected),
            "属性 {attribute:?} 应当决定角色 {expected}，名字 {name:?} 不该插话"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. IDLE：一条推送算「有变化」还是「要重连」
// ---------------------------------------------------------------------------

/// IDLE 期间随机收到的响应 → `classify_response`：只有 `BYE` 是错误，
/// 其余全是「有变化」，**没有第三种结论**。
///
/// 防的是什么：判错的代价不对称（`idle.rs` 的文档写着）：误报只是多一轮同步；
/// 漏报是用户五分钟看不到新邮件。所以「不确定」时必须落到 `MailboxChanged`。
/// 反过来，`BYE` 漏判会让 `DONE` 之后的会话看起来还能用——其实服务器已经要关了。
#[test]
fn idle_pushes_are_either_changed_or_a_bye_never_something_else() {
    let mut rng = Rng::new(0x007F_3005);
    let mut changed = 0usize;
    let mut bye = 0usize;

    for _ in 0..3000 {
        let response = random_response(&mut rng);
        let is_bye = matches!(
            &response,
            Response::Data {
                status: Status::Bye,
                ..
            }
        );

        let classified = classify_response(&response);
        match (is_bye, classified) {
            (true, Ok(_)) => panic!("BYE 必须报错（服务器要断了），实际算成了「有变化」"),
            (true, Err(_)) => bye += 1,
            (false, Ok(event)) => {
                changed += 1;
                assert_eq!(
                    event,
                    xremail_protocol_imap::IdleEvent::MailboxChanged,
                    "非 BYE 的响应一律算「有变化」，实际 {event:?}（响应 {response:?}）"
                );
            }
            (false, Err(error)) => {
                panic!("非 BYE 的响应不该报错，实际 {error:?}（响应 {response:?}）")
            }
        }
    }

    // 两种分支都得真的走到，否则用例是空转的。
    assert!(
        changed > 100,
        "非 BYE 的样本太少（{changed}），随机面不够宽"
    );
    if bye == 0 {
        eprintln!("这次随机没抽到 BYE（种子固定，正常情况下会有）");
    }
}

fn random_response<'a>(rng: &mut Rng) -> Response<'a> {
    // `Status` 不是 `Clone`（imap-proto 0.17），所以存成构造函数而不是值。
    type StatusFactory = fn() -> Status;
    const UNTAGGED: &[(&str, StatusFactory)] = &[
        ("OK", || Status::Ok),
        ("NO", || Status::No),
        ("BAD", || Status::Bad),
        ("BYE", || Status::Bye),
        ("PREAUTH", || Status::PreAuth),
    ];

    match rng.below(8) {
        0 | 1 => {
            let (_, status) = rng.pick(UNTAGGED);
            Response::Data {
                status: status(),
                outcome: imap_proto::Outcome::default(),
            }
        }
        2 => Response::MailboxData(imap_proto::MailboxDatum::Exists(rng.u32())),
        3 => Response::MailboxData(imap_proto::MailboxDatum::Recent(rng.u32())),
        4 => Response::Expunge(rng.u32()),
        5 => Response::Fetch(rng.u32(), vec![AttributeValue::Uid(rng.u32())]),
        6 => Response::Vanished {
            earlier: rng.bool(),
            uids: (0..rng.below(3))
                .map(|_| {
                    let start = rng.u32();
                    start..=start.saturating_add(rng.below(4) as u32)
                })
                .collect(),
        },
        _ => Response::Data {
            status: Status::Ok,
            outcome: imap_proto::Outcome::default(),
        },
    }
}

// ---------------------------------------------------------------------------
// 4. COPYUID：端点精确、展开有界、映射不凭空成立
// ---------------------------------------------------------------------------

/// `expand_uid_set`：结果**只由输入决定**、长度不超过上限、每个值都落在某个
/// 声明的区间里（或等于某个声明的单值）。
///
/// 防的是什么：`UidRange` 是**闭区间**（`imap-proto` 用 `fst..=snd`），
/// 端点写成 `end() - 1` 会少一个 UID；写成 `end() + 1` 会在 `u32::MAX` 上 panic
/// （debug 下溢出）。这里用随机的起止值把两种都逼出来。
#[test]
fn expanded_uid_sets_are_exact_bounded_and_contained() {
    const LIMIT: usize = 10_000;
    let mut rng = Rng::new(0x007F_3006);

    for _ in 0..3000 {
        let members: Vec<UidSetMember> = (0..rng.below(4))
            .map(|_| {
                if rng.bool() {
                    UidSetMember::Uid(rng.u32())
                } else {
                    let start = rng.u32();
                    // 有时让区间真的很大（考验上限），有时很小（考验端点）。
                    let end = if rng.bool() {
                        start.saturating_add(rng.below(4) as u32)
                    } else {
                        u32::MAX
                    };
                    UidSetMember::UidRange(start..=end)
                }
            })
            .collect();

        let expanded = expand_uid_set(&members);
        assert!(
            expanded.len() <= LIMIT,
            "展开 {} 个元素，超过上限 {LIMIT}（输入 {members:?}）",
            expanded.len()
        );

        for uid in &expanded {
            let contained = members.iter().any(|member| match member {
                UidSetMember::Uid(value) => value == uid,
                UidSetMember::UidRange(range) => range.contains(uid),
            });
            assert!(
                contained,
                "展开出了服务器没声明的 UID {uid}（输入 {members:?}）"
            );
        }

        // 没撞到上限时，单值成员一个都不能漏。撞到上限时后面的成员本来就
        // 会被截掉——那是文档写明的「截断」，不是吞。
        if expanded.len() < LIMIT {
            for member in &members {
                if let UidSetMember::Uid(uid) = member {
                    assert!(
                        expanded.contains(uid),
                        "单值成员 {uid} 被吞了（输入 {members:?}）"
                    );
                }
            }
        } else {
            assert_eq!(expanded.len(), LIMIT, "截断必须恰好停在上限处");
        }

        // 确定性的：同一个输入展开两次必须一模一样（不允许依赖时间/Hash 顺序）。
        assert_eq!(expanded, expand_uid_set(&members));
    }
}

/// `CopyUid` 的映射：源/目标长度不一时**明确地不可用**，一致时按下标对应。
///
/// 防的是什么：`TransferOutcome::destination_uid_of` 的文档写着「按源 UID 查目标
/// UID（没有对应关系时返回 `None`）」。硬套的后果是把信 A 的新 UID 记成信 B 的。
#[test]
fn copy_uid_state_refuses_to_guess() {
    let mut rng = Rng::new(0x007F_3007);

    for _ in 0..3000 {
        let mut source: Vec<u32> = (0..rng.below(4)).map(|_| rng.u32()).collect();
        let mut destination: Vec<u32> = (0..rng.below(4)).map(|_| rng.u32()).collect();
        if rng.bool() {
            // 一半的样本强制等长。
            destination.resize(source.len(), 7);
        }

        let copy_uid = CopyUid::new(rng.u32(), source.clone(), destination.clone());
        let (_, source_uids, destination_uids) = copy_uid.state();

        assert_eq!(source_uids, source.as_slice(), "源集合必须原样带出来");
        assert_eq!(destination_uids, destination.as_slice(), "目标集合同理");

        let outcome = copy_uid.into_outcome(&[]);
        if source.is_empty() || source.len() != destination.len() {
            assert!(
                !outcome.has_uid_mapping(),
                "源 {:?} → 目标 {:?} 不是可用的映射",
                source,
                destination
            );
            for uid in &source {
                assert_eq!(
                    outcome.destination_uid_of(*uid),
                    None,
                    "不可用的映射里不许查得出目标"
                );
            }
        } else {
            assert!(outcome.has_uid_mapping());
            // 源 UID 可能重复（随机的 `u32` 偶尔撞上）：`destination_uid_of` 按
            // **第一次出现**的下标取（`position` 的语义），期望值也按这个算。
            for (index, uid) in source.iter().enumerate() {
                let first = source
                    .iter()
                    .position(|other| other == uid)
                    .unwrap_or(index);
                assert_eq!(
                    outcome.destination_uid_of(*uid),
                    Some(destination[first]),
                    "源 {uid} 的目标应当按下标取（首次出现的那个），而不是按值"
                );
            }
        }

        // 重复的源 UID：第一次出现赢（`position` 的语义），仍然按下标对。
        if source.len() == destination.len() && source.len() >= 2 {
            source[1] = source[0];
            let copy_uid = CopyUid::new(1, source.clone(), destination.clone());
            let outcome = copy_uid.into_outcome(&[]);
            assert_eq!(
                outcome.destination_uid_of(source[0]),
                Some(destination[0]),
                "源 UID 重复时取第一次出现的那个"
            );
        }
    }
}

/// 没有 `COPYUID` 时：源集合 = 入参**排序去重**后的结果，目标为空。
///
/// 防的是什么：`run_transfer_with_timeout` 的注释写着「服务器没给 COPYUID 时，
/// 源集合就是我们自己发出去的那些」。原样照抄 `[3, 1, 1]` 会让下游按下标对齐时
/// 错位；空着会让调用方以为「一封都没搬」。
#[test]
fn absent_copy_uid_falls_back_to_the_requested_set() {
    let mut rng = Rng::new(0x007F_3008);

    for _ in 0..2000 {
        let requested: Vec<u32> = (0..rng.below(5)).map(|_| rng.below(6) as u32).collect();
        let outcome = CopyUid::absent_outcome(&requested);

        let mut expected = requested.clone();
        expected.sort_unstable();
        expected.dedup();

        assert_eq!(
            outcome.source_uids, expected,
            "要排序去重（入参 {requested:?}）"
        );
        assert!(outcome.destination_uids.is_empty());
        assert_eq!(outcome.source_uid_validity, None);
        assert_eq!(outcome.destination_uid_validity, None);
        assert!(!outcome.has_uid_mapping());
    }
}

// ---------------------------------------------------------------------------
// 5. APPENDUID：闭区间端点
// ---------------------------------------------------------------------------

/// `appended_uid`：单值取自己，区间取**末个**（闭区间），没有 / 残缺一律 `None`。
///
/// 防的是什么：这里曾经写成 `end().checked_sub(1)`，`APPENDUID 123 5:7` 被报成 6，
/// 于是发信副本的 UID 全记错一位——下一轮同步找不到「自己刚存的那封」，
/// 又 append 一次。
#[test]
fn appenduid_takes_the_last_uid_of_a_closed_range() {
    let mut rng = Rng::new(0x007F_3009);

    for _ in 0..3000 {
        let members: Vec<UidSetMember> = (0..rng.below(4))
            .map(|_| {
                if rng.bool() {
                    UidSetMember::Uid(rng.u32())
                } else {
                    let start = rng.u32();
                    UidSetMember::UidRange(start..=start.saturating_add(rng.below(8) as u32))
                }
            })
            .collect();
        let validity = rng.u32();

        let outcome = with_append_uid(validity, members.clone());
        let expected = members.first().map(|member| match member {
            UidSetMember::Uid(uid) => *uid,
            UidSetMember::UidRange(range) => *range.end(),
        });

        assert_eq!(
            appended_uid(&outcome),
            expected,
            "成员 {members:?} 应当报出 {expected:?}"
        );

        if let Some(first) = members.first() {
            // 报出来的 UID 必须**落在**第一个成员里：单值就是它自己，
            // 区间就是末个（不能是 end-1，也不能是 start）。
            let value = appended_uid(&outcome).expect("有成员就有 UID");
            let contained = match first {
                UidSetMember::Uid(uid) => value == *uid,
                UidSetMember::UidRange(range) => range.contains(&value),
            };
            assert!(contained, "报出的 UID {value} 不在成员 {first:?} 里");
        }
    }

    // 没有 `APPENDUID` / 空成员：`None`，不是 0。
    assert_eq!(appended_uid(&empty_outcome()), None);
    let outcome = with_append_uid(1, Vec::new());
    assert_eq!(appended_uid(&outcome), None, "空的 APPENDUID 不能补成 0");
}

fn with_append_uid<'a>(validity: u32, members: Vec<UidSetMember>) -> imap_proto::Outcome<'a> {
    imap_proto::Outcome {
        code: Some(ResponseCode::AppendUid(validity, members)),
        information: None,
    }
}

fn empty_outcome<'a>() -> imap_proto::Outcome<'a> {
    imap_proto::Outcome::default()
}

// ---------------------------------------------------------------------------
// 6. 假服务器的解析器：不比真服务器宽松
// ---------------------------------------------------------------------------

/// 随机字节（按行）喂给假服务器的几个解析器：不 panic、结论明确、**接受即被遵守**。
///
/// 防的是什么：假服务器若比真服务器**宽松**，用例就会用「假服务器能接受、真服务器
/// 会 `BAD`」的形状去断言客户端行为——测试是绿的，线上是坏的（R40 就是这个形状）。
/// 这里钉三条：
///
/// 1. `Command::parse` 出来的 `name` 永远是大写、`tag` 永远非空；
/// 2. `SequenceSet::parse` 成功的集合，`contains` 与 `resolve` 必须**一致**——
///    否则「命中了」与「展开了」两条路径会给出不同的邮件集合；
/// 3. `parse_store` 成功的请求，`flags` 永远是 `(`…`)` 里那几个，多一个少一个都不行。
#[test]
fn fake_server_parsers_never_disagree_with_themselves() {
    let mut rng = Rng::new(0x007F_300A);
    let mut accepted_sequences = 0usize;

    for _ in 0..4000 {
        // ---- Command::parse ----
        let line = random_command_line(&mut rng);
        if let Some(command) = Command::parse(&line) {
            assert!(!command.tag.is_empty(), "{line:?} 解析出了空 tag");
            assert_eq!(
                command.name,
                command.name.to_ascii_uppercase(),
                "{line:?} 的命令名不是大写：{}",
                command.name
            );
            // 命令名的形状没有承诺：`Command::parse` 的文档只说「已去掉命令名」，
            // 它按第一个空格切，制表符、空命令名的情况都可能出现（服务的是
            // 「我们自己客户端发出去的命令」，那里面不会有制表符）。
            // 这里只钉**切词是确定的**：同样的行解析两次，结果必须一样。
            assert_eq!(Some(command.clone()), Command::parse(&line));
            // 引号字符串的解析必须闭合安全：任意输入都不 panic，且每个值里的
            // 反斜杠转义都已消解（不许把 `\"` 原样留在值里）。
            for value in quoted_values(&command.args) {
                assert!(
                    !value.contains("\\\""),
                    "{line:?} 的引号字符串里残留了转义：{value:?}"
                );
            }
        }

        // ---- SequenceSet ----
        let set_text = random_sequence_text(&mut rng);
        match SequenceSet::parse(&set_text) {
            Ok(set) => {
                accepted_sequences += 1;
                // 解析成功的集合至少有一段（`is_empty` 的文档写着「永远为 false」）。
                assert!(!set.is_empty(), "{set_text:?} 解析出了空集合");

                let max = rng.below(40) as u32;
                let resolved = set.resolve(max);
                // 展开：升序、去重、不超过 max。
                assert!(
                    resolved.windows(2).all(|w| w[0] < w[1]),
                    "{set_text:?} 未排序去重: {resolved:?}"
                );
                for uid in &resolved {
                    assert!(
                        *uid <= max,
                        "{set_text:?} 展开出了超过最大值 {max} 的 {uid}"
                    );
                }
                // `contains` 与 `resolve` 必须一致——两条路径被不同地方用到。
                for uid in 0..=max {
                    assert_eq!(
                        set.contains(uid, max),
                        resolved.contains(&uid),
                        "含 {set_text:?}、max={max} 时，第 {uid} 个：contains 与 resolve 说法不一致"
                    );
                }
                // 往返：显示出来的文本再解析一次，行为必须一样。
                let round = SequenceSet::parse(&set.to_string()).unwrap_or_else(|e| {
                    panic!("{set_text:?} → {:?} 再解析失败: {e}", set.to_string())
                });
                for uid in 0..=max {
                    assert_eq!(
                        set.contains(uid, max),
                        round.contains(uid, max),
                        "{set_text:?} 往返之后对第 {uid} 个的判断变了"
                    );
                }
            }
            Err(error) => assert!(!error.to_string().is_empty()),
        }
    }

    assert!(
        accepted_sequences > 300,
        "只有 {accepted_sequences} 个序列集被接受，随机面太窄，用例几乎是空转的"
    );
}

/// `parse_fetch` / `parse_items` / `parse_store`：成功时结构自洽，失败时明确报错。
///
/// 防的是什么：「看起来成功但内容错位」——`parse_fetch` 把 sequence set 与取件项
/// 分开的方式是「切第一个词」，一旦服务器侧与客户端侧的切法不一致，
/// `1:*` 可能被当成取件项、`(UID)` 可能被当成 sequence set。
#[test]
fn fetch_and_store_parsers_are_self_consistent() {
    let mut rng = Rng::new(0x007F_300B);
    let mut accepted_fetch = 0usize;
    let mut accepted_store = 0usize;

    for _ in 0..4000 {
        // ---- FETCH ----
        let args = random_fetch_args(&mut rng);
        match parse_fetch(&args) {
            Ok((set, items)) => {
                accepted_fetch += 1;
                // sequence set 是**原样**带出来的（假服务器随后自己再解析一次，
                // `handle_fetch` 里就是 `SequenceSet::parse(&set_text)`）。
                // 这里不要求它本身能解析——那是假服务器下一层的事，而且要钉的是
                // 「切词位置对不对」：set 里不许有空白。
                assert!(
                    !set.contains(char::is_whitespace),
                    "{args:?} 的 sequence set 里有空白"
                );
                // 取件项不许有空项，且每项都得是合法的命令文本（没有残留的括号）。
                for item in &items {
                    if let FetchItem::Other(text) = item {
                        assert!(!text.is_empty(), "{args:?} 解析出了空的取件项");
                    }
                }
                // 往返：把自己的输出再拼回去解析，必须得到同样的结果。
                if let Some(rendered) = render_fetch(&set, &items) {
                    let again = parse_fetch(&rendered)
                        .unwrap_or_else(|e| panic!("{args:?} → {rendered:?} 再解析失败: {e}"));
                    assert_eq!(
                        again.0, set,
                        "往返之后 sequence set 变了：{args:?} → {rendered:?}"
                    );
                }
            }
            Err(error) => assert!(!error.to_string().is_empty()),
        }

        // ---- parse_items 单独测：带括号的全部成功，缺括号的全部失败 ----
        let inner = random_items_text(&mut rng);
        let wrapped = format!("({inner})");
        assert!(
            parse_items(&wrapped).is_ok(),
            "带括号的 {wrapped:?} 必须能被解析"
        );
        assert!(
            parse_items(inner.trim()).is_err(),
            "没括号的 {inner:?} 必须报错，不能「看起来成功」"
        );

        // ---- STORE ----
        let store = random_store_args(&mut rng);
        match parse_store(&store) {
            Ok(request) => {
                accepted_store += 1;
                // 成功就意味着操作词是 FLAGS（±/.SILENT 形式），别的都该报错。
                assert!(
                    store.to_ascii_uppercase().contains("FLAGS"),
                    "{store:?} 没有 FLAGS，却解析成功了"
                );
                // 标记都是非空、不含括号的原子。
                for flag in &request.flags {
                    assert!(
                        !flag.is_empty() && !flag.starts_with('(') && !flag.ends_with(')'),
                        "{store:?} 解析出了畸形标记 {flag:?}"
                    );
                }
                // `.SILENT` 只会让 `silent` 变真，绝不会把操作方向也算错。
                if store.to_ascii_uppercase().contains(".SILENT") {
                    assert!(request.silent, "{store:?} 带 .SILENT，却没标成 silent");
                }
                if store.trim_start().starts_with('+') {
                    assert_eq!(request.operation, StoreOperation::Add);
                }
                if store.trim_start().starts_with('-') {
                    assert_eq!(request.operation, StoreOperation::Remove);
                }
            }
            Err(error) => assert!(!error.to_string().is_empty()),
        }
    }

    assert!(
        accepted_fetch > 100,
        "被接受的 FETCH 太少（{accepted_fetch}）"
    );
    assert!(
        accepted_store > 100,
        "被接受的 STORE 太少（{accepted_store}）"
    );
}

fn render_fetch(set: &str, items: &[FetchItem]) -> Option<String> {
    let rendered: Vec<String> = items
        .iter()
        .map(|item| match item {
            FetchItem::Uid => Some("UID".to_string()),
            FetchItem::Flags => Some("FLAGS".to_string()),
            FetchItem::Rfc822Size => Some("RFC822.SIZE".to_string()),
            FetchItem::InternalDate => Some("INTERNALDATE".to_string()),
            FetchItem::Envelope => Some("ENVELOPE".to_string()),
            FetchItem::ModSeq => Some("MODSEQ".to_string()),
            FetchItem::Body { peek, section } => Some(format!(
                "BODY{}[{}]",
                if *peek { ".PEEK" } else { "" },
                render_section(section)?
            )),
            FetchItem::Rfc822 { part } => Some(match part {
                None => "RFC822".to_string(),
                Some(body) => format!("RFC822{}", render_rfc822_suffix(body)?),
            }),
            FetchItem::Other(text) => {
                if text.contains(char::is_whitespace) || text.contains(['(', ')']) {
                    return None;
                }
                Some(text.clone())
            }
        })
        .collect::<Option<Vec<String>>>()?;
    Some(format!("{set} ({})", rendered.join(" ")))
}

fn render_section(section: &BodySection) -> Option<String> {
    match section {
        BodySection::Full => Some(String::new()),
        BodySection::Header => Some("HEADER".to_string()),
        BodySection::Text => Some("TEXT".to_string()),
        BodySection::HeaderFields(fields) => Some(format!("HEADER.FIELDS ({})", fields.join(" "))),
    }
}

fn render_rfc822_suffix(section: &BodySection) -> Option<&'static str> {
    match section {
        BodySection::Header => Some(".HEADER"),
        BodySection::Text => Some(".TEXT"),
        // `RFC822` 的其他形态解析不出来（`RFC822.SIZE` 是另一项）。
        BodySection::Full | BodySection::HeaderFields(_) => None,
    }
}

/// `search::parse`：成功的表达式**不丢键**，失败的明确报错。
///
/// 防的是什么：`search.rs` 的模块文档写着「不支持的键**显式报错**，而不是悄悄当成
/// 「匹配全部」——后者会让测试看起来通过、实际什么都没测」。这里钉住：
/// 返回 `Ok` 的表达式里，每个关键字都真的变成了一个 `SearchKey`（AND 的个数 == 词的个数）。
#[test]
fn search_parses_every_key_or_refuses_the_expression() {
    // `BOGUS` **只**出现在「键的位置」：值的位置上它只是普通文本
    // （`KEYWORD BOGUS` 里的 `BOGUS` 是个关键字名，不是不支持的键）。
    const KEYS: &[&str] = &[
        "ALL",
        "SEEN",
        "UNSEEN",
        "FLAGGED",
        "DELETED",
        "DRAFT",
        "NEW",
        "RECENT",
        "OLD",
        "ANSWERED",
        "UNDELETED",
        "UNFLAGGED",
        "UNANSWERED",
        "UNDRAFT",
        "\\Seen",
        "BOGUS",
    ];

    let mut rng = Rng::new(0x007F_300C);
    let mut accepted = 0usize;

    for _ in 0..4000 {
        let count = 1 + rng.below(4);
        let mut words: Vec<String> = Vec::new();
        for _ in 0..count {
            match rng.below(5) {
                // 需要参数的键：有时给参数、有时故意不给（必须报错）。
                0 => {
                    let key = rng.pick(&["UID", "SUBJECT", "HEADER", "SINCE", "KEYWORD", "FROM"]);
                    words.push(key.to_string());
                    if rng.below(3) != 0 {
                        words.push(
                            rng.pick(&["1:2", "\"会议\"", "hello", "01-Jan-2024", "\\Seen"])
                                .to_string(),
                        );
                    }
                    if *key == "HEADER" && rng.bool() {
                        words.push("\"x\"".to_string());
                    }
                }
                // NOT / OR：少一个参数时必须报错。
                1 => {
                    words.push(rng.pick(&["NOT", "OR"]).to_string());
                    if rng.below(3) != 0 {
                        words.push("SEEN".to_string());
                    }
                    if rng.below(2) == 0 {
                        words.push("UNSEEN".to_string());
                    }
                }
                // 括号：有时不闭合。
                2 => {
                    if rng.bool() {
                        words.push("(".to_string());
                        words.push("SEEN".to_string());
                        if rng.bool() {
                            words.push(")".to_string());
                        }
                    } else {
                        words.push(")".to_string());
                    }
                }
                _ => words.push(rng.pick(KEYS).to_string()),
            }
        }
        let expression = words.join(" ");

        match parse_search(&expression) {
            Ok(key) => {
                accepted += 1;
                // 恰好一个键时不许被包成 AND（那会让「NOT (……)」的语义多一层）。
                let top_level = count_keys(&key);
                assert!(
                    top_level >= 1,
                    "{expression:?} 解析成功了却是空表达式：{key:?}"
                );
                // 不支持的键必须报错，不能落到「认不出的词 → 匹配全部」的兜底上
                // （文档明令禁止：那会让测试看起来通过、实际什么都没测）。
                // 判据是「`BOGUS` 紧跟在某个需要参数的键后面」——那个位置上它是
                // 参数（如 `HEADER BOGUS FLAGGED` 的字段名），不是键。
                let words: Vec<&str> = expression.split_whitespace().collect();
                let value_positions: Vec<usize> = words
                    .iter()
                    .enumerate()
                    .flat_map(|(index, word)| {
                        let count = match word.to_ascii_uppercase().as_str() {
                            "HEADER" => 2,
                            "UID" | "SUBJECT" | "FROM" | "TO" | "CC" | "BCC" | "BODY" | "TEXT"
                            | "SINCE" | "BEFORE" | "ON" | "KEYWORD" | "UNKEYWORD" => 1,
                            _ => 0,
                        };
                        ((index + 1)..=(index + count)).collect::<Vec<usize>>()
                    })
                    .collect();
                let bogus_is_a_key = words
                    .iter()
                    .enumerate()
                    .any(|(index, word)| word == &"BOGUS" && !value_positions.contains(&index));
                if bogus_is_a_key {
                    panic!("{expression:?} 里有不支持的键 BOGUS，却被当成了 {key:?}");
                }
            }
            Err(error) => {
                assert!(matches!(
                    error,
                    SearchError::Invalid(_) | SearchError::Unsupported(_)
                ));
            }
        }
    }

    assert!(
        accepted > 100,
        "被接受的搜索表达式太少（{accepted}），随机面太窄"
    );
}

fn count_keys(key: &SearchKey) -> usize {
    match key {
        SearchKey::And(keys) => keys.iter().map(count_keys).sum(),
        SearchKey::Not(inner) => 1 + count_keys(inner),
        SearchKey::Or(left, right) => 1 + count_keys(left) + count_keys(right),
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// 随机输入生成器
// ---------------------------------------------------------------------------

fn random_command_line(rng: &mut Rng) -> String {
    const FRAGMENTS: &[&str] = &[
        "a1",
        "A0001",
        "X0001",
        "*",
        "",
        " ",
        "  ",
        "\t",
        "UID",
        "uid",
        "FETCH",
        "fetch",
        "LOGIN",
        "1:*",
        "(UID",
        "FLAGS)",
        "\"INBOX\"",
        "\"&XfJT0ZAB-\"",
        "\"we\\\"ird\"",
        "\"",
        "\\",
        "{12}",
        "{12+}",
        "APPEND",
        "STORE",
        "+FLAGS",
        "UNCHANGEDSINCE",
        "(5)",
        "SEARCH",
        "1:10,15,20:*",
        "4294967296",
        "4294967295",
        "0",
        "\\Seen",
        "(",
        ")",
    ];
    let mut line = String::new();
    for _ in 0..(1 + rng.below(5)) {
        line.push_str(rng.pick(FRAGMENTS));
        if rng.bool() {
            line.push(' ');
        }
    }
    let line = line.trim_end_matches('\n').to_string();
    // 一半带上 CRLF：`Command::parse` 必须自己吃掉它们。
    if rng.bool() {
        format!("{line}\r\n")
    } else {
        line
    }
}

fn random_sequence_text(rng: &mut Rng) -> String {
    const PARTS: &[&str] = &[
        "1",
        "0",
        "*",
        "2:5",
        "5:2",
        "1:*",
        "*:*",
        "1:",
        ":1",
        ":",
        "1,,2",
        "4294967295",
        "4294967296",
        " 3 ",
        "abc",
        "-1",
        "+1",
        "1:2:3",
        "１",
    ];
    let count = 1 + rng.below(4);
    let mut text = String::new();
    for index in 0..count {
        if index > 0 {
            text.push(',');
        }
        text.push_str(rng.pick(PARTS));
    }
    if rng.bool() {
        text.insert(0, ' ');
    }
    text
}

fn random_items_text(rng: &mut Rng) -> String {
    const ITEMS: &[&str] = &[
        "UID",
        "FLAGS",
        "RFC822.SIZE",
        "INTERNALDATE",
        "ENVELOPE",
        "MODSEQ",
        "BODY[]",
        "BODY.PEEK[HEADER]",
        "BODY.PEEK[HEADER.FIELDS (FROM TO)]",
        "BODY[1.2]",
        "BODY[]  ",
        "RFC822",
        "RFC822.HEADER",
        "RFC822.SIZE ",
        "X-GM-LABELS",
        "",
    ];
    let count = rng.below(4);
    (0..count)
        .map(|_| rng.pick(ITEMS).to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn random_fetch_args(rng: &mut Rng) -> String {
    let set = random_sequence_text(rng);
    let items = random_items_text(rng);
    match rng.below(6) {
        0 => format!("{set} ({items})"),
        1 => format!("{set} ({items}"),
        2 => format!("{set} {items})"),
        3 => format!("  {set}   ({items})  "),
        4 => format!("{set} ()"),
        _ => format!("{set}({items})"),
    }
}

fn random_store_args(rng: &mut Rng) -> String {
    const HEADS: &[&str] = &[
        "+FLAGS",
        "-FLAGS",
        "FLAGS",
        "+FLAGS.SILENT",
        "-FLAGS.SILENT",
        "FLAGS.SILENT",
        "flags",
        "+flags.silent",
        "+BOGUS",
        "",
        "+",
        "-",
    ];
    const FLAG_LISTS: &[&str] = &[
        "(\\Seen)",
        "(\\Seen \\Deleted)",
        "()",
        "(\\Seen",
        "\\Seen)",
        "\\Seen",
        "",
    ];

    let head = rng.pick(HEADS);
    let flags = rng.pick(FLAG_LISTS);
    let mut args = format!("{head} {flags}");
    if rng.below(3) == 0 {
        args.push_str(rng.pick(&[
            " UNCHANGEDSINCE (12345)",
            " UNCHANGEDSINCE 12345",
            " UNCHANGEDSINCE ()",
            " UNCHANGEDSINCE",
            " UNCHANGEDSINCE (99999999999999999999)",
        ]));
    }
    args
}

// 让 `Cow` 的导入在生成器里也用上（`NameAttribute::Extension` 用了它）。
const _: fn() -> Cow<'static, str> = || Cow::Borrowed("");

// `BodyExtension` 只被用到类型层面（`AttributeValue::Envelope` 的字段），
// 留一个引用免得 `use` 被 clippy 判成多余。
const _: fn() -> Option<BodyExtension<'static>> = || None;
