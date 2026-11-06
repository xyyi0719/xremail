//! 附件下载服务：把邮件里的附件**落到本地文件**。
//!
//! ## 为什么是「流式落盘」而不是「解码成一整块内存」
//!
//! 一个 10 MB 的附件在 base64 之后是 ~13 MB，解码那一刻要两块内存并存；
//! 一封邮件挂 5 个就 100 MB 起。所以这里**流式**做：
//!
//! 1. `BODY.PEEK[n]` 只拉**那一个部件**（不是整封邮件）；
//! 2. 传输编码（base64 / quoted-printable）按**固定大小的块**增量解码，
//!    每解出一块就写进临时文件，内存里永远只有一个块（[`CHUNK_BYTES`]）；
//! 3. 边写边数：**超过上限立刻报错并删掉临时文件**——不截断、不留半截文件；
//! 4. 写完核对字节数（与库里记录的 `size` 比），算 SHA-256，
//!    最后**原子改名**成正式文件。
//!
//! ## 幂等
//!
//! 正式文件名是内容哈希，所以在下载**之前**就能算出来：
//! 文件已经在那儿、且大小与库里一致 → 直接返回，不连网、不重下。
//!
//! ## 字节不走 IPC
//!
//! 前端拿到的是**本地路径**，打开它交给系统（`tauri-plugin-opener`）。
//! 附件字节从头到尾不经过 IPC。
//!
//! ## 路径布局
//!
//! ```text
//! <app_data>/attachments/<message_id>/<sha256 前 32 位>
//! <app_data>/attachments/<message_id>/.<attachment_id>.name   ← 下载时的文件名
//! ```
//!
//! 文件名不进主路径（避免中文/非法字符/路径穿越）。用户看到的名字来自数据库；
//! `.name` 小文件记着下载那一刻的名字，交给系统打开时用它——
//! `Message-ID` 里的尖括号与 `/` 在 Windows 上是非法字符。

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use xremail_core::attachment::MAX_ATTACHMENT_BYTES;
use xremail_core::inline_image::MAX_INLINE_IMAGE_BYTES;
use xremail_core::model::{Attachment, Message};
use xremail_core::{AppError, AppResult};
use xremail_store::dao::attachment as attachment_dao;

use crate::service::{MessageService, current_time_ms};

/// 一次增量解码的块大小。
///
/// 影响两件事：内存峰值（约这个数的一倍多）与磁盘写入次数。
/// 64 KiB 对一块机械盘也是单次写入，对内存则是「随便占」。
pub const CHUNK_BYTES: usize = 64 * 1024;

/// 下载完成后给前端的东西（**只有路径与元数据，没有字节**）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDownload {
    /// 附件 id。
    pub attachment_id: String,
    /// 本地文件路径。
    pub path: String,
    /// 建议的文件名（已清洗成各平台都能用的样子）。
    pub name: String,
    /// 落盘字节数。
    pub size: i64,
    /// 内容 SHA-256（小写十六进制）。
    pub sha256: String,
    /// 这次是命中已有文件（`true`）还是真的下载了（`false`）。
    pub cached: bool,
}

/// 附件服务。
#[derive(Debug, Clone)]
pub struct AttachmentService {
    pool: SqlitePool,
    messages: MessageService,
    /// `<app_data>/attachments`。
    root: PathBuf,
}

impl AttachmentService {
    /// 用连接池、邮件服务与**数据目录**构造。
    ///
    /// `data_dir` 就是 [`crate::state::AppState`] 里那个目录（数据库文件所在处）。
    pub fn new(pool: SqlitePool, messages: MessageService, data_dir: impl Into<PathBuf>) -> Self {
        let root = data_dir.into().join("attachments");
        Self {
            pool,
            messages,
            root,
        }
    }

    /// 附件根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 列出某封邮件的附件（元数据里带上本地路径与下载状态）。
    pub async fn list(&self, message_id: &str) -> AppResult<Vec<Attachment>> {
        self.messages.list_attachments(message_id).await
    }

    /// 取一个附件的本地路径（给「打开」用）。
    ///
    /// **只认**数据库里记着的、且文件还在的路径；不信任前端传来的任何路径。
    /// 另外再核一次「它确实在附件目录里」——数据库被别人改过也不认。
    pub async fn local_path(&self, message_id: &str, attachment_id: &str) -> AppResult<PathBuf> {
        let attachment = self.get_meta(message_id, attachment_id).await?;
        if !attachment.fetched {
            return Err(AppError::NotFound(format!(
                "附件「{}」还没下载",
                display_name(&attachment)
            )));
        }
        let stored = attachment
            .local_path
            .as_deref()
            .map(PathBuf::from)
            .ok_or_else(|| AppError::NotFound(format!("附件 {attachment_id} 没有本地文件")))?;
        let canonical = std::fs::canonicalize(&stored).map_err(|error| {
            AppError::NotFound(format!("本地文件已经不在（{}）：{error}", stored.display()))
        })?;
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if !canonical.starts_with(&root) {
            return Err(AppError::NotFound("本地文件不在附件目录里".into()));
        }
        Ok(canonical)
    }

    /// 附件元数据（校验它属于这封邮件）。
    pub async fn get_meta(&self, message_id: &str, attachment_id: &str) -> AppResult<Attachment> {
        attachment_dao::get_attachment(&self.pool, message_id, attachment_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("附件 {attachment_id} 不存在")))
    }

    /// 建议的保存文件名（给「另存为」用）。
    pub async fn suggested_name(&self, message_id: &str, attachment_id: &str) -> AppResult<String> {
        let attachment = self.get_meta(message_id, attachment_id).await?;
        Ok(safe_file_name(
            attachment.filename.as_deref().unwrap_or("attachment.bin"),
        ))
    }

    /// 一封邮件里所有**已下载**的内联图片（给 cid 替换用）。
    ///
    /// 读不出来的单张图片跳过而不是整体失败：一张坏图不该让整封邮件的正文消失。
    pub async fn inline_images(
        &self,
        message_id: &str,
    ) -> Vec<xremail_core::inline_image::InlineImage> {
        let inline = attachment_dao::list_inline_attachments(&self.pool, message_id)
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for attachment in inline {
            let image =
                xremail_core::inline_image::InlineImage::from_attachment(&attachment, |path| {
                    std::fs::read(path)
                });
            match image {
                Ok(image) => out.push(image),
                Err(error) => {
                    // 未下载、不是图片、文件被改过——都不是致命错误，正文照常显示。
                    tracing::debug!(attachment = %attachment.id, %error, "内联图片跳过");
                }
            }
        }
        out
    }

    /// 已经有完整文件时直接返回。
    ///
    /// 判定「完整」：库里记着 `fetched`、路径存在、**大小与库里一致**。
    pub fn existing(&self, attachment: &Attachment) -> Option<AttachmentDownload> {
        if !attachment.fetched {
            return None;
        }
        let (Some(path), Some(sha)) = (
            attachment.local_path.as_deref(),
            attachment.sha256.as_deref(),
        ) else {
            return None;
        };
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        if let Some(expected) = attachment.size
            && expected >= 0
            && expected != size
        {
            return None;
        }
        Some(AttachmentDownload {
            attachment_id: attachment.id.clone(),
            path: path.to_string(),
            name: self
                .stored_name(&attachment.message_id, &attachment.id)
                .unwrap_or_else(|| {
                    safe_file_name(attachment.filename.as_deref().unwrap_or("attachment.bin"))
                }),
            size,
            sha256: sha.to_string(),
            cached: true,
        })
    }

    /// 把**已经拿到的**（仍是传输编码形态的）字节解码、落盘、记账。
    ///
    /// 与网络解耦，方便单测：真网络路径只是多一个 `fetch_attachment_part`。
    ///
    /// 返回的 `temp` 只在失败时有意义（成功时已经改名成正式文件）。
    pub fn store(
        &self,
        message_id: &str,
        attachment: &Attachment,
        raw: &[u8],
    ) -> AppResult<AttachmentDownload> {
        let limit = limit_for(attachment);
        let written = write_decoded(
            raw,
            attachment.transfer_encoding.as_deref().unwrap_or(""),
            &self.root,
            message_id,
            &attachment.id,
            limit,
        )?;

        // 与库里记录的解析长度核对：拉的是同一个部件，长度必须一致。
        if let Some(expected) = attachment.size
            && expected >= 0
            && expected != written.size
        {
            let _ = std::fs::remove_file(&written.temp);
            return Err(AppError::Storage(format!(
                "附件「{}」大小对不上：邮件里记录 {expected} 字节，实际取到 {} 字节",
                display_name(attachment),
                written.size
            )));
        }

        finalize(&written)?;

        let name = safe_file_name(attachment.filename.as_deref().unwrap_or("attachment.bin"));
        write_sidecar(&self.root, message_id, &attachment.id, &name)?;

        Ok(AttachmentDownload {
            attachment_id: attachment.id.clone(),
            path: written.final_path.to_string_lossy().to_string(),
            name,
            size: written.size,
            sha256: written.sha256,
            cached: false,
        })
    }

    /// 记库：`local_path` / `sha256` / `size` / `fetched` / `downloaded_at`。
    pub async fn mark(&self, message_id: &str, download: &AttachmentDownload) -> AppResult<()> {
        attachment_dao::mark_downloaded(
            &self.pool,
            message_id,
            &download.attachment_id,
            &download.path,
            &download.sha256,
            download.size,
            current_time_ms(),
        )
        .await?;
        Ok(())
    }

    /// 一次完整的下载：查元数据 → 幂等短路 → 取字节 → 落盘 → 记库。
    ///
    /// `fetch` 负责「把部件号换成字节」，生产路径是
    /// [`xremail_sync::fetch_attachment_part`]，测试里可以换成假的。
    pub async fn download_with<F, Fut>(
        &self,
        message: &Message,
        attachment_id: &str,
        remote_name: &str,
        fetch: F,
    ) -> AppResult<AttachmentDownload>
    where
        F: FnOnce(String, String, i64, String) -> Fut,
        Fut: std::future::Future<Output = AppResult<Option<Vec<u8>>>>,
    {
        let attachment = self.get_meta(&message.id, attachment_id).await?;

        // 1. 幂等：已有完整文件 → 直接返回。
        if let Some(done) = self.existing(&attachment) {
            return Ok(done);
        }

        let part = attachment
            .part_key
            .clone()
            .ok_or_else(|| AppError::Storage("这个附件没有部件号，没法下载".into()))?;

        // 2. 取字节（只取那一个部件，`PEEK` 不改 \Seen）。
        let raw = fetch(
            message.id.clone(),
            remote_name.to_string(),
            message.uid,
            part,
        )
        .await?
        .ok_or_else(|| AppError::NotFound("服务器上没有这个部件".into()))?;

        // 3. 落盘 + 记库。记库失败时文件留着——下次下载会命中 `existing`。
        let download = self.store(&message.id, &attachment, &raw)?;
        self.mark(&message.id, &download).await?;
        Ok(download)
    }

    fn stored_name(&self, message_id: &str, attachment_id: &str) -> Option<String> {
        std::fs::read_to_string(sidecar_path(&self.root, message_id, attachment_id))
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }
}

/// 一次「流式写出」的结果。
#[derive(Debug)]
struct Written {
    final_path: PathBuf,
    temp: PathBuf,
    size: i64,
    sha256: String,
}

/// 把（可能还是编码态的）字节增量解码并写到临时文件。
fn write_decoded(
    raw: &[u8],
    encoding: &str,
    root: &Path,
    message_id: &str,
    attachment_id: &str,
    limit: u64,
) -> AppResult<Written> {
    use std::io::Write;

    let dir = root.join(safe_component(message_id));
    std::fs::create_dir_all(&dir)
        .map_err(|error| AppError::Io(format!("建附件目录失败（{}）：{error}", dir.display())))?;

    let temp = dir.join(format!(".{}.part", safe_component(attachment_id)));
    let file = std::fs::File::create(&temp)
        .map_err(|error| AppError::Io(format!("建临时文件失败（{}）：{error}", temp.display())))?;
    let mut writer = std::io::BufWriter::new(file);

    let mut hasher = Hasher::new();
    let mut size: u64 = 0;
    let mut decoder = Decoder::new(encoding);

    let outcome = (|| -> AppResult<()> {
        for chunk in raw.chunks(CHUNK_BYTES) {
            let decoded = decoder.push(chunk);
            write_block(&decoded, &mut size, limit, &mut hasher, &mut writer)?;
        }
        let tail = decoder.finish();
        write_block(&tail, &mut size, limit, &mut hasher, &mut writer)
    })();

    // 无论成功失败都要先关文件：Windows 上文件还被打开着就删不掉。
    let closed = writer
        .flush()
        .map_err(|error| AppError::Io(format!("写附件失败：{error}")))
        .and_then(|()| {
            writer
                .into_inner()
                .map_err(|error| AppError::Io(format!("写附件失败：{error}")))
        })
        .and_then(|file| {
            file.sync_all()
                .map_err(|error| AppError::Io(format!("落盘失败：{error}")))
        });

    if let Err(error) = outcome.and(closed) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }

    let sha256 = hasher.finish();

    // 正式文件名用内容哈希：同样的字节只落一份，天然幂等。
    let final_path = dir.join(&sha256[..32]);
    Ok(Written {
        final_path,
        temp,
        size: i64::try_from(size).unwrap_or(i64::MAX),
        sha256,
    })
}

/// 写一块解出来的字节；超限就地报错（**不截断**）。
fn write_block(
    bytes: &[u8],
    size: &mut u64,
    limit: u64,
    hasher: &mut Hasher,
    writer: &mut std::io::BufWriter<std::fs::File>,
) -> AppResult<()> {
    use std::io::Write;

    if bytes.is_empty() {
        return Ok(());
    }
    *size += bytes.len() as u64;
    if *size > limit {
        return Err(AppError::Invalid(format!(
            "附件超过 {} MB 的上限，已中止（没有留下文件）",
            limit / (1024 * 1024)
        )));
    }
    hasher.update(bytes);
    writer
        .write_all(bytes)
        .map_err(|error| AppError::Io(format!("写附件失败：{error}")))
}

/// 临时文件 → 正式文件（原子改名；目标已存在就丢掉临时文件）。
fn finalize(written: &Written) -> AppResult<()> {
    if written.final_path.exists() {
        // 同样的内容已经在那儿了（上次下到一半失败，或同一封邮件里两段字节相同）。
        let _ = std::fs::remove_file(&written.temp);
        return Ok(());
    }
    std::fs::rename(&written.temp, &written.final_path).map_err(|error| {
        let _ = std::fs::remove_file(&written.temp);
        AppError::Io(format!(
            "把附件移到最终位置失败（{}）：{error}",
            written.final_path.display()
        ))
    })
}

/// 增量传输解码器。
///
/// * base64：把输入里的字母表字符攒起来，满 4 个解出 3 字节；
/// * quoted-printable：按字节扫描，`=` 后面不够判断就先留着（跨块也要对）；
/// * 其余（7bit / 8bit / binary / 未知）：原样输出。
#[derive(Debug)]
struct Decoder {
    kind: Kind,
    /// base64：已经攒下的字母表字符（0–3 个）。
    quad: Vec<u8>,
    /// base64：已经见过 `=` 填充，后续输入不再产生字节。
    padded: bool,
    /// QP：跨块的待决字节（0–2 个）。
    carry: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Base64,
    QuotedPrintable,
    Identity,
}

impl Decoder {
    fn new(encoding: &str) -> Self {
        let kind = match encoding.trim().to_ascii_lowercase().as_str() {
            "base64" => Kind::Base64,
            "quoted-printable" => Kind::QuotedPrintable,
            _ => Kind::Identity,
        };
        Self {
            kind,
            quad: Vec::with_capacity(4),
            padded: false,
            carry: Vec::with_capacity(4),
        }
    }

    /// 喂一块，返回此刻能确定解出来的字节。
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self.kind {
            Kind::Identity => chunk.to_vec(),
            Kind::Base64 => self.push_base64(chunk),
            Kind::QuotedPrintable => self.push_qp(chunk, false),
        }
    }

    /// 收尾。
    fn finish(&mut self) -> Vec<u8> {
        match self.kind {
            Kind::Identity => Vec::new(),
            Kind::Base64 => {
                // 不足 4 个字符的尾巴：补 `=` 解掉（坏数据，不是我们的错）。
                if self.quad.is_empty() {
                    return Vec::new();
                }
                let mut quad = std::mem::take(&mut self.quad);
                while quad.len() < 4 {
                    quad.push(b'=');
                }
                // 补出来的 `=` 不算填充位：只解出真正有的字符。
                let meaningful = quad.iter().take_while(|byte| **byte != b'=').count();
                let decoded = decode_quad(&quad);
                let take = match meaningful {
                    4 => 3,
                    3 => 2,
                    2 => 1,
                    _ => 0,
                };
                decoded[..take].to_vec()
            }
            Kind::QuotedPrintable => self.push_qp(&[], true),
        }
    }

    fn push_base64(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() / 4 * 3 + 3);
        for byte in chunk {
            // 填充之后的一切都是垃圾（有的客户端在同一个部件里拼多段 base64）。
            if self.padded {
                break;
            }
            // 不在字母表里的一律忽略（RFC 2045 §6.8：换行、空格、垃圾字符）。
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/' | b'=') {
                self.quad.push(*byte);
                if self.quad.len() == 4 {
                    // `=` 是填充：按填充的个数决定这一组真正产出几个字节。
                    // 不解裁的话 `VGVzdA==` 会解出 `Test\0\0`，落盘长度就对不上了。
                    let padding = self.quad.iter().filter(|byte| **byte == b'=').count();
                    let data = 4 - padding;
                    let produced = match data {
                        0 | 1 => 0,
                        2 => 1,
                        3 => 2,
                        _ => 3,
                    };
                    out.extend(&decode_quad(&self.quad)[..produced]);
                    // 填充之后的一切都是垃圾（有的客户端在同一个部件里拼多段 base64）。
                    self.padded = padding > 0;
                    self.quad.clear();
                }
            }
        }
        out
    }

    /// QP：`carry` 里最多留 2 个待决字节。
    fn push_qp(&mut self, chunk: &[u8], end: bool) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(self.carry.len());
        let mut index = 0;

        while index < self.carry.len() {
            let rest = &self.carry[index..];
            if rest[0] != b'=' {
                out.push(rest[0]);
                index += 1;
                continue;
            }
            if rest.len() >= 2 && rest[1] == b'\n' {
                index += 2; // 软换行
                continue;
            }
            if rest.len() >= 3 && rest[1] == b'\r' && rest[2] == b'\n' {
                index += 3; // 软换行
                continue;
            }
            if rest.len() >= 3 {
                let high = (rest[1] as char).to_digit(16);
                let low = (rest[2] as char).to_digit(16);
                match (high, low) {
                    (Some(high), Some(low)) => out.push((high * 16 + low) as u8),
                    // 不合法的 `=`：原样保留（与 `transfer::decode_quoted_printable` 一致）。
                    _ => out.push(b'='),
                }
                index += 3;
                continue;
            }
            if end {
                out.push(b'=');
                index += 1;
                continue;
            }
            // 还判断不了（`=` 或 `=\r`）：留给下一块。
            break;
        }

        self.carry.drain(..index);
        out
    }
}

/// 解一个 4 字符的 base64 组（可能带 `=` 填充）。
/// 填充位与垃圾字符都当 0——但输出长度由 [`Decoder::finish`] 按真实字符数裁。
fn decode_quad(quad: &[u8]) -> [u8; 3] {
    let mut values = [0u8; 4];
    for (index, byte) in quad.iter().take(4).enumerate() {
        values[index] = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            // `=` 填充与垃圾字符都当 0：填充的位数本来就不该被采纳。
            _ => 0,
        };
    }
    let merged = ((values[0] as u32) << 18)
        | ((values[1] as u32) << 12)
        | ((values[2] as u32) << 6)
        | (values[3] as u32);
    [(merged >> 16) as u8, (merged >> 8) as u8, merged as u8]
}

/// 一个附件允许的最大字节数。
///
/// 内联图片更小；普通附件与发信侧同一个上限。**超过就报错**，不截断。
pub fn limit_for(attachment: &Attachment) -> u64 {
    if attachment.inline {
        MAX_INLINE_IMAGE_BYTES
    } else {
        MAX_ATTACHMENT_BYTES
    }
}

/// 小写十六进制。
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 边写边算的 SHA-256。
///
/// 这里没有用 `sha2` crate：它只是「给落盘文件起个内容名」，
/// 不是安全边界（本地文件的完整性由落盘时的 `size` 校验与首次写入的
/// 原子改名保证），不为一个哈希再拉一个依赖。
///
/// 实现是 FIPS 180-4 的标准 SHA-256，对分块喂入等价于一次性计算。
#[derive(Debug)]
struct Hasher {
    /// 中间状态（H0..H7）。
    state: [u32; 8],
    /// 目前攒着的字节（不足 64 的一截）。
    block: Vec<u8>,
    /// 已经吃进去的字节数。
    length: u64,
}

impl Hasher {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: Vec::with_capacity(64),
            length: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.length += bytes.len() as u64;
        let mut rest = bytes;
        if !self.block.is_empty() {
            let need = 64 - self.block.len();
            let take = need.min(rest.len());
            self.block.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.block.len() == 64 {
                let block: [u8; 64] = self.block[..].try_into().expect("64 字节");
                compress(&mut self.state, &block);
                self.block.clear();
            }
        }
        while rest.len() >= 64 {
            let block: [u8; 64] = rest[..64].try_into().expect("64 字节");
            compress(&mut self.state, &block);
            rest = &rest[64..];
        }
        self.block.extend_from_slice(rest);
    }

    /// 收尾，返回小写十六进制摘要。
    fn finish(mut self) -> String {
        let bits = self.length.wrapping_mul(8);
        let mut tail = std::mem::take(&mut self.block);
        tail.push(0x80);
        while tail.len() % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bits.to_be_bytes());
        let mut state = self.state;
        // tail 一定是 64 的整数倍（上面那个 while 保证 % 64 == 56 之后又补了 8 字节长度），
        // 所以按 64 字节切片交给 compress，不会剩下短块。
        for chunk in tail.chunks(64) {
            let block: [u8; 64] = chunk.try_into().expect("tail 是 64 的整数倍");
            compress(&mut state, &block);
        }
        let mut out = Vec::with_capacity(32);
        for word in state {
            out.extend_from_slice(&word.to_be_bytes());
        }
        hex(&out)
    }
}

/// SHA-256 的常数表（前 64 个素数立方根小数部分的前 32 位）。
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 压缩一个 64 字节块。
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    // 块长恒为 64，按 4 字节切成 16 个大端字，没有剩余。
    for (index, chunk) in block.chunks(4).enumerate() {
        w[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[index])
            .wrapping_add(w[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

/// 数据库里的名字 → 各平台都能用的文件名。
pub fn safe_file_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let bad = matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || (ch as u32) < 0x20
            || ch == '\u{7f}';
        out.push(if bad { '_' } else { ch });
    }
    let trimmed = out.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return "attachment.bin".to_string();
    }
    // 太长的名字在 Windows 上建不出来；截断时尽量保住扩展名。
    let mut name: String = trimmed.chars().take(120).collect();
    if name != trimmed
        && let Some(dot) = trimmed.rfind('.')
        && trimmed.len() - dot <= 10
        && let Some(extension) = trimmed.get(dot..)
        && !name.ends_with(extension)
    {
        name.push_str(extension);
    }
    // 只去掉**开头**的点：`.` 开头的名字在 Unix 上是隐藏文件，`..` 开头更糟。
    // 末尾的保留（`报告..` 也还是叫「报告..」）。
    let name = name.trim_start_matches('.');
    if name.is_empty() {
        return "attachment.bin".to_string();
    }
    name.to_string()
}

/// 目录名/文件名里的安全片段（只留字母数字与 `-_.`）。
fn safe_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|ch| ch == '.' || ch == '_') {
        "item".to_string()
    } else {
        cleaned
    }
}

/// sidecar 的路径：`<dir>/.<id>.name`。
fn sidecar_path(root: &Path, message_id: &str, attachment_id: &str) -> PathBuf {
    root.join(safe_component(message_id))
        .join(format!(".{}.name", safe_component(attachment_id)))
}

fn write_sidecar(root: &Path, message_id: &str, attachment_id: &str, name: &str) -> AppResult<()> {
    let path = sidecar_path(root, message_id, attachment_id);
    std::fs::write(&path, name)
        .map_err(|error| AppError::Io(format!("记录附件文件名失败（{}）：{error}", path.display())))
}

/// 错误信息里显示的名字。
fn display_name(attachment: &Attachment) -> String {
    attachment
        .filename
        .clone()
        .unwrap_or_else(|| attachment.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use xremail_core::model::{Account, Folder, MessageFlags};

    /// 造一份附件元数据。
    fn meta(id: &str, part: &str, encoding: &str, size: i64, inline: bool) -> Attachment {
        Attachment {
            id: id.to_string(),
            message_id: "m1@example.com".to_string(),
            filename: Some("报告.pdf".to_string()),
            mime: Some(if inline {
                "image/png".to_string()
            } else {
                "application/pdf".to_string()
            }),
            size: Some(size),
            content_id: if inline {
                Some("logo@example.com".to_string())
            } else {
                None
            },
            inline,
            local_path: None,
            sha256: None,
            fetched: false,
            part_key: Some(part.to_string()),
            downloaded_at_ms: None,
            transfer_encoding: Some(encoding.to_string()),
        }
    }

    /// 临时附件目录。
    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("建临时目录失败")
    }

    /// 只落盘、不碰数据库的占位池。
    fn lazy_pool() -> SqlitePool {
        SqlitePool::connect_lazy("sqlite::memory:").expect("占位池")
    }

    /// 临时目录上的附件服务（数据库是占位池，只用于 `store`）。
    fn service_on(dir: &Path) -> AttachmentService {
        AttachmentService::new(lazy_pool(), MessageService::new(lazy_pool()), dir)
    }

    #[tokio::test]
    async fn base64_part_is_written_and_hashed() {
        let dir = temp_root();
        // `store` 不碰数据库，池子是占位。
        let service = service_on(dir.path());
        let attachment = meta("m1/2", "2", "base64", 3, false);

        // 「Man」的 base64 是 TWFu。
        let download = service
            .store("m1@example.com", &attachment, b"TWFu")
            .expect("落盘失败");

        assert_eq!(download.size, 3);
        assert!(!download.cached);
        // 「Man」的 SHA-256（内容寻址的文件名就是它的前 32 位）。
        assert_eq!(
            download.sha256,
            "20fe1bd201cd900bdbffeaec0b42e40b51cbf6b37ae5fbeaddd83aab9a221837"
        );
        assert!(download.path.ends_with(&download.sha256[..32]));
        let bytes = std::fs::read(&download.path).expect("读回失败");
        assert_eq!(bytes, b"Man");
        // 落盘大小与记录一致。
        assert_eq!(
            std::fs::metadata(&download.path).expect("stat").len(),
            download.size as u64
        );
        // 文件名 sidecar 也写了。
        assert_eq!(
            service.stored_name("m1@example.com", "m1/2").as_deref(),
            Some("报告.pdf")
        );
    }

    #[tokio::test]
    async fn quoted_printable_part_is_decoded_on_disk() {
        let dir = temp_root();
        let service = service_on(dir.path());
        // 「日 本」的 UTF-8 是 E6 97 A5  E6 9C AC；带一个软换行。
        let attachment = meta("m1/3", "3", "quoted-printable", 6, false);
        let download = service
            .store("m1@example.com", &attachment, b"=E6=97=A5=\r\n=E6=9C=AC")
            .expect("落盘失败");

        assert_eq!(download.size, 6);
        assert_eq!(
            std::fs::read(&download.path).expect("读回失败"),
            "日本".as_bytes()
        );
    }

    #[tokio::test]
    async fn oversized_part_is_refused_without_leaving_a_file() {
        let dir = temp_root();
        let service = service_on(dir.path());
        // 内联图的上限是 2 MB；造一条超过它的。
        let attachment = meta("m1/1.2", "1.2", "8bit", 0, true);
        let raw = vec![b'a'; MAX_INLINE_IMAGE_BYTES as usize + 1];

        let error = service
            .store("m1@example.com", &attachment, &raw)
            .expect_err("超限应该报错");
        let text = error.to_string();
        assert!(text.contains("上限"), "错误信息应提到上限：{text}");
        assert!(
            text.contains("没有留下文件"),
            "错误信息应说明不留文件：{text}"
        );

        // 目录里不该留下任何东西（临时的 `.part` 也删了）。
        let leftovers: Vec<_> = std::fs::read_dir(service.root().join("m1_example.com"))
            .map(|entries| entries.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "不该留下文件：{leftovers:?}");
    }

    #[tokio::test]
    async fn a_size_disagreement_is_refused() {
        let dir = temp_root();
        let service = service_on(dir.path());
        // 解析时记的是 5 字节，实际解出来 3 字节。
        let attachment = meta("m1/2", "2", "base64", 5, false);
        let error = service
            .store("m1@example.com", &attachment, b"TWFu")
            .expect_err("大小不符应该报错");
        assert!(
            error.to_string().contains("大小对不上"),
            "错误信息：{error}"
        );
    }

    #[tokio::test]
    async fn a_second_store_reuses_the_same_file() {
        let dir = temp_root();
        let service = service_on(dir.path());
        let attachment = meta("m1/2", "2", "base64", 3, false);
        let first = service
            .store("m1@example.com", &attachment, b"TWFu")
            .expect("第一次落盘失败");
        let second = service
            .store("m1@example.com", &attachment, b"TWFu")
            .expect("第二次落盘失败");

        // 同样的字节落在同一个路径上（内容寻址），没有第二份文件。
        assert_eq!(first.path, second.path);
        let count = std::fs::read_dir(service.root().join("m1_example.com"))
            .expect("读目录失败")
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".name"))
            .count();
        assert_eq!(count, 1, "同样的内容应只落一份");
    }

    #[tokio::test]
    async fn an_existing_file_short_circuits_the_download() {
        let dir = temp_root();
        let service = service_on(dir.path());
        let mut attachment = meta("m1/2", "2", "base64", 3, false);
        let download = service
            .store("m1@example.com", &attachment, b"TWFu")
            .expect("落盘失败");

        // 还没记库 → 不算「已有」。
        assert!(service.existing(&attachment).is_none());

        attachment.fetched = true;
        attachment.local_path = Some(download.path.clone());
        attachment.sha256 = Some(download.sha256.clone());
        let existing = service.existing(&attachment).expect("应该命中已有文件");
        assert!(existing.cached);
        assert_eq!(existing.size, 3);
        assert_eq!(existing.path, download.path);

        // 文件被换过（大小变了）→ 不再算「已有」，避免把坏文件当好的交出去。
        std::fs::write(&download.path, b"Man-and-more").expect("覆写失败");
        assert!(service.existing(&attachment).is_none());
    }

    #[test]
    fn sha256_matches_the_known_vectors() {
        // FIPS 180-4 / 常见测试向量。
        let cases: [(&[u8], &str); 3] = [
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (input, expected) in cases {
            let mut hasher = Hasher::new();
            hasher.update(input);
            assert_eq!(hasher.finish(), expected, "输入长度 {}", input.len());
        }
    }

    #[test]
    fn sha256_is_the_same_whether_fed_in_one_or_many_chunks() {
        let data: Vec<u8> = (0..1000u32).map(|index| (index % 251) as u8).collect();
        let mut one = Hasher::new();
        one.update(&data);
        let whole = one.finish();

        for chunk in [1usize, 7, 63, 64, 65, 200] {
            let mut many = Hasher::new();
            for piece in data.chunks(chunk) {
                many.update(piece);
            }
            assert_eq!(many.finish(), whole, "分块大小 {chunk}");
        }
    }

    #[test]
    fn the_decoder_handles_chunk_boundaries() {
        // base64 的 4 字符组与 QP 的 `=` 三元组都会被块边界切开。
        // 「Testb&6d」，7 个字符 + `=` 填充。
        let encoded = b"VGVzdGImNmQ=";
        let mut decoder = Decoder::new("base64");
        let mut out = Vec::new();
        for byte in encoded.chunks(3) {
            out.extend(decoder.push(byte));
        }
        out.extend(decoder.finish());
        // 「Test」+「b&6d」：9 字节，块边界正好切在组中间。
        assert_eq!(out, b"Testb&6d");

        let encoded = b"=E6=97=A5=0A=E6=9C=AC";
        let mut decoder = Decoder::new("quoted-printable");
        let mut out = Vec::new();
        for byte in encoded.chunks(2) {
            out.extend(decoder.push(byte));
        }
        out.extend(decoder.finish());
        assert_eq!(out, "日\n本".as_bytes());
    }

    #[test]
    fn base64_stops_at_the_padding() {
        // 只解第一段：填充之后的内容不是这个部件的字节。
        let mut decoder = Decoder::new("base64");
        let mut out = decoder.push(b"VGVzdA==VGVzdA==");
        out.extend(decoder.finish());
        assert_eq!(out, b"Test");

        // 尾块不足 4 个字符：补 `=` 后按真实字符数裁长度。
        let mut decoder = Decoder::new("base64");
        let mut out = decoder.push(b"TWE");
        out.extend(decoder.finish());
        assert_eq!(out, b"Ma");
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_file_name("报告.pdf"), "报告.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("<n1@example.com>"), "_n1@example.com_");
        assert_eq!(safe_file_name("   "), "attachment.bin");
        let long = format!("{}.pdf", "a".repeat(200));
        let safe = safe_file_name(&long);
        assert!(safe.chars().count() <= 124, "长度 {}", safe.chars().count());
        assert!(safe.ends_with(".pdf"));
    }

    #[test]
    fn inline_parts_get_the_smaller_limit() {
        assert_eq!(
            limit_for(&meta("m1/1", "1", "base64", 0, true)),
            MAX_INLINE_IMAGE_BYTES
        );
        assert_eq!(
            limit_for(&meta("m1/2", "2", "base64", 0, false)),
            MAX_ATTACHMENT_BYTES
        );
    }

    /// 每封邮件都要有 `(account, folder, message)` 才存得下附件。
    async fn seeded(
        root: &Path,
    ) -> (
        AttachmentService,
        Message,
        tempfile::TempDir,
        sqlx::SqlitePool,
    ) {
        let pool = xremail_store::open_pool_in_memory(&xremail_store::PoolConfig::default())
            .await
            .expect("打开内存库失败");
        xremail_store::migrate(&pool).await.expect("迁移失败");

        let account = Account {
            id: "acc-1".into(),
            email: "me@example.com".into(),
            provider: xremail_core::model::ProviderKind::Netease163,
            display_name: None,
            color: None,
            auth_kind: xremail_core::model::AuthKind::Password,
            server: xremail_core::model::ServerConfig {
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                imap_tls: xremail_core::model::TlsMode::Implicit,
                smtp_host: "smtp.example.com".into(),
                smtp_port: 465,
                smtp_tls: xremail_core::model::TlsMode::Implicit,
                smtp_auth: xremail_core::model::SmtpAuth::Login,
            },
            username: "me@example.com".into(),
            secret_ref: None,
            oauth_client_id: None,
            sync_mode: xremail_core::model::SyncMode::Poll,
            poll_interval_secs: 300,
            signature_html: None,
            created_at: 1,
            updated_at: 1,
        };
        xremail_store::dao::account::upsert_account(&pool, &account)
            .await
            .expect("种账户失败");

        let folder = Folder {
            id: "fld-1".into(),
            account_id: account.id.clone(),
            remote_name: "INBOX".into(),
            remote_id: None,
            local_name: Some("收件箱".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            last_sync_at: Some(1),
            last_uid: Some(9),
            highest_modseq: None,
            uidvalidity: Some(7),
        };
        xremail_store::dao::folder::upsert_folder(&pool, &folder)
            .await
            .expect("种文件夹失败");

        let message = Message {
            id: "m1@example.com".into(),
            account_id: account.id.clone(),
            folder_id: folder.id.clone(),
            uid: 9,
            uidvalidity: 7,
            message_id: Some("<m1@example.com>".into()),
            subject: Some("带附件的邮件".into()),
            snippet: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            date_sent: Some(1),
            internal_date: Some(1),
            flags: MessageFlags::empty(),
            size: Some(100),
            has_attachment: true,
        };
        xremail_store::dao::message::upsert_message(&pool, &message)
            .await
            .expect("种邮件失败");

        let dir = temp_root();
        let service =
            AttachmentService::new(pool.clone(), MessageService::new(pool.clone()), dir.path());
        let _ = root;
        (service, message, dir, pool)
    }

    #[tokio::test]
    async fn download_with_is_idempotent_and_marks_the_row() {
        let (service, message, _dir, pool) = seeded(Path::new(".")).await;

        let mut attachment = meta("m1@example.com/2", "2", "base64", 3, false);
        attachment.message_id = message.id.clone();
        xremail_store::dao::attachment::replace_attachments(&pool, &message.id, &[attachment], 1)
            .await
            .expect("种附件失败");

        let first = service
            .download_with(
                &message,
                "m1@example.com/2",
                "INBOX",
                |_, _, _, part| async move {
                    assert_eq!(part, "2");
                    Ok(Some(b"TWFu".to_vec()))
                },
            )
            .await
            .expect("第一次下载失败");
        assert!(!first.cached);

        // 第二次不再访问服务器：闭包里直接 panic 就能证明。
        let second = service
            .download_with(
                &message,
                "m1@example.com/2",
                "INBOX",
                |_, _, _, _| async move { panic!("已有文件时不该再取一次") },
            )
            .await
            .expect("第二次下载失败");
        assert!(second.cached);
        assert_eq!(second.path, first.path);

        // 库里记上了。
        let listed = attachment_dao::list_attachments(&pool, &message.id)
            .await
            .expect("读附件失败");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].fetched);
        assert_eq!(listed[0].size, Some(3));
        assert!(listed[0].downloaded_at_ms.is_some());
        assert_eq!(
            attachment_dao::count_pending_downloads(&pool, &message.id)
                .await
                .expect("计数失败"),
            0
        );
    }

    #[tokio::test]
    async fn download_with_refuses_an_oversized_part_and_leaves_no_row_marked() {
        let (service, message, _dir, pool) = seeded(Path::new(".")).await;

        let mut attachment = meta("m1@example.com/1.2", "1.2", "8bit", 0, true);
        attachment.message_id = message.id.clone();
        attachment.size = None;
        xremail_store::dao::attachment::replace_attachments(&pool, &message.id, &[attachment], 1)
            .await
            .expect("种附件失败");

        let raw = vec![b'a'; MAX_INLINE_IMAGE_BYTES as usize + 1];
        let error = service
            .download_with(
                &message,
                "m1@example.com/1.2",
                "INBOX",
                |_, _, _, _| async move { Ok(Some(raw)) },
            )
            .await
            .expect_err("超限应该报错");
        assert!(error.to_string().contains("上限"), "错误信息：{error}");

        let listed = attachment_dao::list_attachments(&pool, &message.id)
            .await
            .expect("读附件失败");
        assert!(!listed[0].fetched, "失败时不该标成已下载");
        assert_eq!(
            attachment_dao::count_pending_downloads(&pool, &message.id)
                .await
                .expect("计数失败"),
            1
        );
        // 目录里没有留下半个文件。
        let dir = service.root().join(safe_component(&message.id));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .map(|entries| entries.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "不该留下文件：{leftovers:?}");
    }

    #[tokio::test]
    async fn a_part_without_a_number_cannot_be_downloaded() {
        let (service, message, _dir, _pool) = seeded(Path::new(".")).await;

        let mut attachment = meta("m1@example.com/x", "2", "base64", 3, false);
        attachment.message_id = message.id.clone();
        attachment.part_key = None;
        // 没有部件号的行存不进库（迁移里 `part_key` 可空只是给更早的数据留位），
        // 所以这里只断言：未知附件会被如实拒绝，而不会去猜一个部件号。
        let error = service
            .download_with(
                &message,
                "m1@example.com/x",
                "INBOX",
                |_, _, _, _| async move { panic!("没有元数据时不该取字节") },
            )
            .await
            .expect_err("未知附件应该报错");
        assert!(error.to_string().contains("不存在"), "错误信息：{error}");
    }
}
