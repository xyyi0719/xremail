-- 附件的 MIME 结构、下载状态与正文里引用的内容地址（B2：附件下载 + 内联图片 cid）。
--
-- ## 为什么复用 `attachment` 表而不是新开一张
--
-- 0001 里已经有 `attachment` 表（id / message_id / filename / mime / size /
-- content_id / inline / local_path / sha256 / fetched），**但一直没有代码用过**。
-- 它缺的只是「这个附件来自 MIME 树的哪一个 part」与两个时间戳：
--
--   * `part_key`：本封邮件内唯一且稳定的部件路径（如 `1.2`），
--     用来把「正文里的 `cid:`」与「哪一个内联部件」对上，也用来按需回服务器取那一段；
--   * `transfer_encoding`：这个部件的 `Content-Transfer-Encoding`（`base64` /
--     `quoted-printable` / `8bit` …）。**下载时必须知道它**：一段纯 ASCII 的文本附件
--     按 base64 去解会解成垃圾，而「按内容猜」是猜不准的；
--   * `downloaded_at` / `created_at`：离线缓存状态（ROADMAP「附件下载、离线缓存状态」）。
--
-- 另开一张 `message_attachment` 只会让「附件」有两份真相。所以这里**加列**。
--
-- ## 列的可空性与默认值
--
-- `part_key` / `transfer_encoding` 可空：老行（如果有）没有这些信息；
-- 新的解析侧一定会给。
-- `inline` 是 SQLite 关键字，写 SQL 时要加双引号——本迁移与 DAO 都这么写。
-- `fetched` 与 `local_path` 必须**同时**成立才算下载过；DAO 一起维护这两列，
-- 不留半截状态。
ALTER TABLE attachment ADD COLUMN part_key TEXT;
ALTER TABLE attachment ADD COLUMN downloaded_at INTEGER;
ALTER TABLE attachment ADD COLUMN created_at INTEGER;
ALTER TABLE attachment ADD COLUMN transfer_encoding TEXT;

-- 同一封邮件里同一个部件只会有一条记录：重拉正文时按它做 upsert，
-- 不会因为「同一封邮件被打开两次」攒出重复的附件行。
--
-- 注意这个唯一约束**依赖 SQL 标准里「NULL 互不相等」**：在唯一索引里，
-- 两行的 `part_key` 都是 NULL 时不算冲突（SQLite 与 Postgres 都如此）。
-- 这正是我们要的——`part_key` 为空的老行（0008 之前写下的、或解析侧认不出
-- 部件路径的）可以有多条而不互相顶掉；只有**有部件路径**的行才去重。
-- 所以 DAO 在 `part_key IS NULL` 时走纯 INSERT，在写入时把它作为「同一部件」的
-- 唯一依据；反过来如果有人把这里的唯一列改成 `COALESCE(part_key, '')` 之类，
-- 这个语义就变了。
CREATE UNIQUE INDEX idx_attachment_message_part
    ON attachment (message_id, part_key);

-- 正文渲染时要按 `cid:` 找内联部件；只给内联且有 content-id 的行建索引。
CREATE INDEX idx_attachment_message_inline
    ON attachment (message_id, content_id)
    WHERE "inline" = 1 AND content_id IS NOT NULL;
