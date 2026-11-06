//! 内联图片替换的纯函数测试。
//!
//! 这些用例只碰 [`xremail_core::inline_image`]，不依赖数据库或网络——
//! 它们要钉死的是几条**安全断言**：
//! * 只有 `cid:` 且能对上的 `<img>` 才会被换成 `data:`；
//! * 远程图片、`file:`、`javascript:` 一律原样留给消毒器（等于删掉）；
//! * 不是图片的 MIME、SVG、超限的字节都不会变成 `data:`；
//! * 替换后的片段里不会出现除 `src`/`alt` 之外的属性。

use std::path::Path;

use xremail_core::html::sanitize_html;
use xremail_core::inline_error::InlineImageError;
use xremail_core::inline_image::{
    InlineImage, MAX_INLINE_IMAGE_BYTES, SAFE_IMAGE_CLOSE, SAFE_IMAGE_OPEN, rewrite_inline_images,
};
use xremail_core::model::Attachment;

/// 1×1 PNG 的字节足够当“一张图”。
fn png_bytes() -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]
}

fn image(cid: &str, mime: &str, bytes: Vec<u8>) -> InlineImage {
    InlineImage::new(cid, mime, bytes).expect("应能构造")
}

/// 按标记切分：把标记外的部分交给消毒器，标记内的保留。
///
/// 这就是服务层真正要做的事，测试里照着实现一份，保证「拼接后仍然是安全 HTML」。
fn render(html: &str, images: &[InlineImage]) -> String {
    let rewritten = rewrite_inline_images(html, images);
    let mut out = String::new();
    let mut rest = rewritten.html.as_str();
    while let Some(start) = rest.find(SAFE_IMAGE_OPEN) {
        let before = &rest[..start];
        out.push_str(&sanitize_html(before));
        let after_open = &rest[start + SAFE_IMAGE_OPEN.len()..];
        let end = after_open
            .find(SAFE_IMAGE_CLOSE)
            .unwrap_or(after_open.len());
        out.push_str(&after_open[..end]);
        rest = &after_open[(end + SAFE_IMAGE_CLOSE.len()).min(after_open.len())..];
    }
    out.push_str(&sanitize_html(rest));
    out
}

#[test]
fn a_matching_cid_is_replaced_with_a_data_uri() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    let out = render(
        r#"<p>你好</p><img src="cid:logo@example.com"><p>再见</p>"#,
        &[logo],
    );

    assert!(out.contains("<img src=\"data:image/png;base64,"), "{out}");
    assert!(out.contains("你好"));
    assert!(out.contains("再见"));
    assert!(!out.contains("cid:"), "cid 引用应已被换掉：{out}");
}

#[test]
fn the_original_html_is_untouched_when_nothing_matches() {
    let logo = image("logo@example.com", "image/png", png_bytes());
    let html = r#"<p>没有图片</p>"#;

    let rewritten = rewrite_inline_images(html, &[logo]);

    assert_eq!(rewritten.html, html, "无匹配时必须逐字节相同");
    assert_eq!(rewritten.replaced, 0);
}

#[test]
fn an_unknown_cid_is_left_for_the_sanitizer_to_drop() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    let out = render(r#"<p>a</p><img src="cid:nope@example.com">"#, &[logo]);

    assert!(!out.contains("<img"), "对不上的 cid 必须被删：{out}");
    assert!(out.contains('a'));
}

#[test]
fn remote_images_are_still_removed_even_when_inline_images_exist() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    let out = render(
        r#"<img src="https://tracker.example/p.gif"><img src="cid:logo@example.com"><img src="http://x/y.png">"#,
        &[logo],
    );

    assert!(!out.contains("tracker.example"), "{out}");
    assert!(!out.contains("http://x/y.png"), "{out}");
    assert_eq!(out.matches("<img").count(), 1, "只应留下内联图：{out}");
}

#[test]
fn dangerous_schemes_are_not_treated_as_cid() {
    let logo = image("x", "image/png", png_bytes());

    for src in [
        "javascript:alert(1)",
        "file:///C:/Windows/win.ini",
        "http://example.com/a.png",
        "data:image/png;base64,AAAA",
        "cidx:foo",
    ] {
        let html = format!(r#"<img src="{src}">"#);
        let rewritten = rewrite_inline_images(&html, std::slice::from_ref(&logo));
        assert_eq!(rewritten.replaced, 0, "{src} 不该被当成 cid");
        assert_eq!(rewritten.html, html);
    }
}

#[test]
fn cid_variants_are_all_matched() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    for src in [
        "cid:logo@example.com",
        "CID:logo@example.com",
        "cid:<logo@example.com>",
        "cid:%3Clogo@example.com%3E",
        "  cid:logo@example.com  ",
        "cid:LOGO@EXAMPLE.COM",
    ] {
        let html = format!(r#"<img src="{src}">"#);
        let rewritten = rewrite_inline_images(&html, std::slice::from_ref(&logo));
        assert_eq!(rewritten.replaced, 1, "{src} 应被替换");
    }
}

#[test]
fn entities_in_the_attribute_are_decoded_before_matching() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    // `&#99;` 是 'c'。
    let rewritten = rewrite_inline_images(r#"<IMG SRC=&#99;id:logo@example.com>"#, &[logo]);

    assert_eq!(rewritten.replaced, 1);
}

#[test]
fn attributes_on_the_original_img_are_not_carried_over() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    let out = render(
        r#"<img src="cid:logo@example.com" onerror="alert(1)" style="x" width="9999" alt="公司 logo">"#,
        &[logo],
    );

    assert!(out.contains("alt=\"公司 logo\""));
    assert!(!out.contains("onerror"), "{out}");
    assert!(!out.contains("style"), "{out}");
    assert!(!out.contains("width"), "{out}");
}

#[test]
fn alt_text_is_escaped() {
    let logo = image("logo@example.com", "image/png", png_bytes());

    let out = render(
        r#"<img src="cid:logo@example.com" alt="a&quot;b<c>">"#,
        &[logo],
    );

    assert!(out.contains("alt=\"a&quot;b&lt;c&gt;\""), "{out}");
}

#[test]
fn non_image_and_svg_parts_are_refused() {
    assert_eq!(
        InlineImage::new("x", "application/pdf", png_bytes()).unwrap_err(),
        InlineImageError::NotAnImage
    );
    assert_eq!(
        InlineImage::new("x", "image/svg+xml", b"<svg/>".to_vec()).unwrap_err(),
        InlineImageError::NotAnImage
    );
    assert_eq!(
        InlineImage::new("x", "text/html", b"<b/>".to_vec()).unwrap_err(),
        InlineImageError::NotAnImage
    );
    assert_eq!(
        InlineImage::new("", "image/png", png_bytes()).unwrap_err(),
        InlineImageError::EmptyContentId
    );
    assert_eq!(
        InlineImage::new("x", "image/png", Vec::new()).unwrap_err(),
        InlineImageError::Empty
    );
}

#[test]
fn oversized_images_are_refused() {
    let too_big = vec![0u8; MAX_INLINE_IMAGE_BYTES as usize + 1];

    let err = InlineImage::new("x", "image/png", too_big).unwrap_err();

    assert!(matches!(err, InlineImageError::TooLarge { .. }));
}

#[test]
fn mime_is_normalized_and_parameters_dropped() {
    let logo = image("x", "IMAGE/PNG; name=a.png", png_bytes());

    assert_eq!(logo.mime(), "image/png");
    assert!(logo.data_uri().starts_with("data:image/png;base64,"));
}

#[test]
fn an_unterminated_img_tag_does_not_swallow_the_rest() {
    let logo = image("logo@example.com", "image/png", png_bytes());
    let html = r#"<p>a</p><img src="cid:logo@example.com""#;

    let rewritten = rewrite_inline_images(html, &[logo]);

    assert_eq!(rewritten.replaced, 0);
    assert!(rewritten.html.contains("<p>a</p>"));
}

#[test]
fn many_images_stop_at_the_total_budget() {
    // 每张 1 MiB，base64 后约 1.37 MB；8 MB 总量只够 5 张多。
    let big = vec![7u8; 1024 * 1024];
    let images: Vec<InlineImage> = (0..10)
        .map(|i| image(&format!("img{i}@example.com"), "image/jpeg", big.clone()))
        .collect();
    let html: String = (0..10)
        .map(|i| format!(r#"<img src="cid:img{i}@example.com">"#))
        .collect();

    let rewritten = rewrite_inline_images(&html, &images);

    assert!(
        rewritten.replaced < 10,
        "总量上限应拦住一部分：replaced={}",
        rewritten.replaced
    );
    assert!(rewritten.replaced >= 5, "但又不应拦得太早");
}

#[test]
fn from_attachment_checks_download_state_and_size() {
    let mut attachment = Attachment {
        id: "at1".into(),
        message_id: "m1".into(),
        filename: Some("logo.png".into()),
        mime: Some("image/png".into()),
        size: Some(png_bytes().len() as i64),
        content_id: Some("<logo@example.com>".into()),
        inline: true,
        local_path: Some("/data/attachments/a/b/c".into()),
        sha256: Some("deadbeef".into()),
        fetched: true,
        part_key: Some("1.1".into()),
        downloaded_at_ms: Some(1),
        transfer_encoding: Some("base64".into()),
    };

    let ok = InlineImage::from_attachment(&attachment, |path: &Path| {
        assert_eq!(path, Path::new("/data/attachments/a/b/c"));
        Ok(png_bytes())
    })
    .expect("应能构造");
    assert_eq!(ok.content_id(), "logo@example.com", "尖括号要去掉");

    // 还没下载。
    attachment.fetched = false;
    assert_eq!(
        InlineImage::from_attachment(&attachment, |_| Ok(png_bytes())).unwrap_err(),
        InlineImageError::NotDownloaded
    );

    // 不是内联部件。
    attachment.fetched = true;
    attachment.inline = false;
    assert_eq!(
        InlineImage::from_attachment(&attachment, |_| Ok(png_bytes())).unwrap_err(),
        InlineImageError::NotInline
    );

    // 文件被外部改过（大小对不上）。
    attachment.inline = true;
    let err = InlineImage::from_attachment(&attachment, |_| Ok(vec![0u8; 3])).unwrap_err();
    assert!(matches!(err, InlineImageError::SizeMismatch { .. }));

    // 文件不在了。
    let err = InlineImage::from_attachment(&attachment, |_| {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "丢了"))
    })
    .unwrap_err();
    assert!(matches!(err, InlineImageError::Io(_)));
}

#[test]
fn base64_of_known_bytes_is_correct() {
    // 手算基准：`Man` → `TWFu`，`Ma` → `TWE=`，`M` → `TQ==`。
    let one = InlineImage::new("a", "image/gif", b"Man".to_vec()).unwrap();
    assert!(one.data_uri().ends_with(";base64,TWFu"));

    let two = InlineImage::new("b", "image/gif", b"Ma".to_vec()).unwrap();
    assert!(two.data_uri().ends_with(";base64,TWE="));

    let three = InlineImage::new("c", "image/gif", b"M".to_vec()).unwrap();
    assert!(three.data_uri().ends_with(";base64,TQ=="));
}
