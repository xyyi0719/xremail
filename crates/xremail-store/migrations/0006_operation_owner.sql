-- 提交归属：区分「本进程正在提交的行」与「上次进程崩掉留下的残骸」。
--
-- 为什么需要它：`running` 只说明「有人认领了」，不说明「那个人还活着」。
-- 启动时若**无条件**把所有 `running` 判成中断，会把同一台机器上另一个
-- 仍然活着的进程正在提交的行误判成「结果未知」——用户白确认一次，
-- 还烧掉一次尝试配额，而那条操作本来一切正常。
-- 所以认领时记下进程归属，回收时只碰**别人的**行。
--
-- `owner_boot` 是进程启动时生成的一次性标识，只存在于内存与该列，
-- 不落 meta 表：它没有跨进程共享的价值，只有「这批行是不是我认领的」这一个用途。
--
-- `started_at` **只是诊断字段**（排查「这一轮为什么慢」），
-- **不参与超时判定**。一旦它参与判定，它就变成了半个心跳；而心跳需要
-- 后台任务、节流落库与进程身份三件套，收益为零、成本最高，
-- 还会和 30 秒命令超时（`xremail-sync/src/net.rs` 的 `COMMAND_TIMEOUT`）抢时序。
-- 若以后有人想拿它判超时，请先明确反对一次。
--
-- 两列都可空：老数据没有归属，按「不是我的」处理（会被回收成 `uncertain`）。
ALTER TABLE mail_operation ADD COLUMN started_at INTEGER;
ALTER TABLE mail_operation ADD COLUMN owner_boot TEXT;

-- 发信队列同样会卡在 `running`（`claim_for_sending` 置位，
-- 而回收函数 `reclaim_running` 此前从未被产品代码调用过）。
ALTER TABLE send_queue ADD COLUMN started_at INTEGER;
ALTER TABLE send_queue ADD COLUMN owner_boot TEXT;

-- 两张表都在每次启动扫一遍，按 (state, owner_boot) 过滤。
CREATE INDEX idx_mail_operation_owner ON mail_operation (state, owner_boot);
CREATE INDEX idx_send_queue_owner ON send_queue (state, owner_boot);
