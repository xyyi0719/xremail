-- 状态机守卫的两处schema级收敛（R50 / R55）。
--
-- ## 为什么需要这一版
--
-- 0006 / 0007 之后，`mail_operation` 与 `send_queue` 的状态机**在应用层**是收敛的：
-- 认领写 `owner_boot`，收尾函数在 `WHERE` 里比对它。但那句话只写在 DAO 里——
-- 任何人打开 sqlite3 或写一条新的 SQL，都能把 `state` 写成任意字符串，
-- 读的一方（`codec::enum_from_text`）直到**读**的时候才会报「未知枚举值」，
-- 而那时这条永远读不出来的行已经躺在库里，还会让整个队列的列表查询失败。
--
-- `folder.role`（0010）就是这么收的：取值集合用 `CHECK` 挡在写入处。这里同样。
--
-- ## 取值集合
--
-- 与 `xremail_core::model` 逐字对应，不多不少：
--   * `mail_operation.state`：`OperationState` 的六个变体；
--   * `send_queue.state`：`SendState` 的七个变体（比 0001 注释多一个 `draft`，
--     那个是 0003 加进来的，0001 的注释**没有**跟着改，见 R56）。
--
-- 改枚举就意味着改这里——迁移只能加列/建表，改 CHECK 得走一次表重建，
-- 所以这件事必须显式，而不是靠「反正 DAO 不会写错」。
--
-- ## 为什么用触发器而不是 ALTER TABLE ADD CONSTRAINT
--
-- SQLite **没有** `ALTER TABLE ... ADD CONSTRAINT`；加 `CHECK` 只能重建整张表
-- （`CREATE TABLE new … `、`INSERT INTO new SELECT …`、`DROP`、`RENAME`），
-- 而这两张表带着 `ON DELETE` 外键（`mail_operation.account_id`、
-- `send_queue.account_id` 都被别的行引用）。重建期间外键指向的是旧表名，
-- 失败的代价是用户库里的队列数据丢失或悬空。
--
-- 触发器等价地把约束挡在写入处：`BEFORE INSERT` 与 `BEFORE UPDATE OF state`
-- 各一条，非法取值时 `RAISE(ABORT, ...)`。代价是每次状态迁移多一次触发器调用，
-- 相对于一次网络往返可以忽略。
--
-- 注释里不出现分号（0010 踩过的坑：sqlx 按分号切语句）。

CREATE TRIGGER mail_operation_state_insert_guard
BEFORE INSERT ON mail_operation
WHEN NEW.state NOT IN ('queued', 'running', 'succeeded', 'retryable', 'failed', 'uncertain')
BEGIN
    SELECT RAISE(ABORT, 'mail_operation.state 只能是 queued / running / succeeded / retryable / failed / uncertain');
END;

CREATE TRIGGER mail_operation_state_update_guard
BEFORE UPDATE OF state ON mail_operation
WHEN NEW.state NOT IN ('queued', 'running', 'succeeded', 'retryable', 'failed', 'uncertain')
BEGIN
    SELECT RAISE(ABORT, 'mail_operation.state 只能是 queued / running / succeeded / retryable / failed / uncertain');
END;

CREATE TRIGGER send_queue_state_insert_guard
BEFORE INSERT ON send_queue
WHEN NEW.state NOT IN ('draft', 'queued', 'running', 'succeeded', 'retryable', 'failed', 'uncertain')
BEGIN
    SELECT RAISE(ABORT, 'send_queue.state 只能是 draft / queued / running / succeeded / retryable / failed / uncertain');
END;

CREATE TRIGGER send_queue_state_update_guard
BEFORE UPDATE OF state ON send_queue
WHEN NEW.state NOT IN ('draft', 'queued', 'running', 'succeeded', 'retryable', 'failed', 'uncertain')
BEGIN
    SELECT RAISE(ABORT, 'send_queue.state 只能是 draft / queued / running / succeeded / retryable / failed / uncertain');
END;
