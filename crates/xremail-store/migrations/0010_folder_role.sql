-- 文件夹的**特殊用途角色**（RFC 6154：`\Inbox` / `\Sent` / `\Drafts` / `\Trash` …），
-- 以及由此支撑的「删除 = 进垃圾箱」（R04 的残余）。
--
-- ## 为什么要有这一列
--
-- 协议层早就能认出角色了：`xremail-sync` 拉 `LIST` 时，
-- `xremail-protocol-imap::folders::detect_role` **属性优先、名字兜底**，
-- 服务器宣告了 `\Trash` 就用宣告的。但那个结论此前**止步于协议层**——
-- `folder` 表没有地方放它，于是回写删除时只能拿文件夹名去比
-- （`role_by_name("Deleted Messages")` 之类）。名字猜的代价是实打实的：
-- 服务器把垃圾箱叫 `Rubbish` 或「回收站」时猜不出来，删除就退化成**就地标删**；
-- 反过来某个普通文件夹名字碰巧像垃圾箱，信就会被挪进一个用户没指望的地方。
--
-- ## 为什么可空、为什么不给默认值
--
-- `NULL` 的含义是**「这个文件夹的角色还不知道」**，不是「没有角色」，
-- 更不是「没有垃圾箱」。下面三种情况都会是 `NULL`：
--
--   * 升级上来的老库：0001 之后、本迁移之前落库的文件夹，从来没存过角色；
--   * 服务器既没宣告 RFC 6154 属性、名字也认不出的文件夹（QQ 只给
--     `\NoSelect` / `\HasChildren`，很多中文名也不在兜底表里）；
--   * 本轮同步还没来得及重写这一行的文件夹。
--
-- 所以**绝不能**给一个 `'inbox'` 之类的默认值去「填补」它：那等于把
-- 「不知道」写成一个具体结论，从这以后没有任何迹象能把它和真结论区分开，
-- 数据库里也就再也补不回来了。读取方必须把 `NULL` 当成「这条线索缺失」，
-- 转去用别的线索（名字兜底），而不是当成一个否定的答案。
--
-- ## 取值
--
-- 约束里的取值集合**就是** `folders.rs` 里 `detect_role` / `role_from_attributes`
-- / `role_by_name` 实际会产出的全部值（`inbox` / `sent` / `drafts` / `trash` /
-- `junk` / `archive` / `flagged` / `all`），不多不少。
--
-- 为什么要 `CHECK`：这一列是**删除往哪儿走**的输入。一个手滑写进去的
-- `'TRASH'`（大小写不同）不会报错，只会在比较时静默不命中，删除于是
-- 悄悄退化成「就地标删」——用户以为能撤销，其实删没了。把错值挡在写入处，
-- 比在下游每一处都记得「比较要忽略大小写」可靠。协议层新增角色时
-- 要同步改这里（迁移只能加列，改约束得走一次表重建，所以这件事必须显式）。
--
-- `NULL` 通过 `role IS NULL OR role IN (...)` 显式放行：
-- SQL 的三值逻辑里 `NULL IN (...)` 求值为 `NULL`，而 `CHECK` 只在
-- 结果为**假**时拒绝——写成 `role IN (...)` 恰好也能过，但那是靠运气，
-- 换个数据库或换个写法就变成「NULL 一律被拒」，所以这里写明白。
--
-- ## 重复应用是安全的
--
-- 这是升级路径，会作用在**已有数据**的库上：
--
--   * `ADD COLUMN` 可空、不设默认值：SQLite 只改表结构元数据，不重写任何行，
--     老行的 `role` 自然是 `NULL`（语义正确：就是还不知道）；已有数据一个字节不变。
--   * 没有表重建、没有 `DROP`、没有回填。**回填尤其不能做**：拿名字去猜一个
--     role 写进去，就等于把我们自己的猜测伪装成「服务器说过」，从此分不清。
--     「名字兜底」是读取方的事（`transfer_push::trash_candidates`），
--     只在那一次决策里生效，不落库。
--
-- 但 SQLite 的 `ALTER TABLE … ADD COLUMN` **不可重放**：第二次执行会报
-- `duplicate column name: role`。sqlx 的迁移器本身不把一条迁移包进事务
-- （它把整段 SQL 逐条预处理执行），所以一句裸 `ALTER` 会让升级半途失败、
-- 应用起不来。「表里已经有这一列、记录表里却没有这一版」是真实存在过的状态：
-- 早期客户端的手工修复、排障时补数据、或者是把库从别处拷回来。
--
-- 结论先说清楚：**在 SQL 层做不到「列已存在就跳过 ALTER」**。SQLite 没有语句级
-- `IF`，也没有 `ALTER … ADD COLUMN IF NOT EXISTS`；`CASE` 只能决定表达式算不算，
-- 决定不了语句跑不跑（实测：`CASE` 里套 `ALTER` 是语法错误，`probe()` 也只能让
-- 参数不被求值，跳不过语句本身）；`BEGIN` / `COMMIT` 也只是让失败**原子地**回滚，
-- 不能让失败的语句变成「没发生」。
--
-- 所以本迁移**不声称**可重复应用。实际保证是：
--
--   * **正常升级路径**只跑一次——`_sqlx_migrations` 按版本记账，这是 sqlx 的事；
--   * 谁要是**手动**把这段 SQL 再跑一遍，会得到一行明确的
--     `duplicate column name: role`，而已有数据**一个字节都不动**
--     （`crates/xremail-store/tests/folder_role.rs` 有一条用例专门钉住这一点）：
--     这是「**响亮地失败**」，不是「说了成功、数据却坏了」。
--
-- 下面这张 `_migration_gate` 表只承担一件事：说明这一版是否已经加过列，
-- 方便排障时用一句 `SELECT` 看出来（升级本身不依赖它，也不删它）。
--
-- 另一条踩过的坑，比上面这条更容易复发：**迁移 SQL 的注释里不要出现分号**。
-- sqlx 在把迁移交给 SQLite 之前按分号切语句，注释里多一个分号就会让后面的语句
-- 整体错位——轻则语法错误，重则**迁移记成「成功」、`role` 列却没加上**
-- （本文件第一版正是如此）。下面所有 SQL 语句都只在行尾用分号，注释里不出现字面分号。
CREATE TABLE IF NOT EXISTS _migration_gate (
    alive   INTEGER,
    role_ok BOOLEAN NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO _migration_gate (alive, role_ok)
SELECT strftime('%s', 'now'), 0
WHERE NOT EXISTS (SELECT 1 FROM _migration_gate)
  AND NOT EXISTS (SELECT 1 FROM pragma_table_info('folder') WHERE name = 'role');

ALTER TABLE folder ADD COLUMN role TEXT
    CHECK (role IS NULL OR role IN (
        'inbox', 'sent', 'drafts', 'trash', 'junk', 'archive', 'flagged', 'all'
    ));

-- 回写删除时要在一轮里问一次「这个账户的垃圾箱是谁」，按账户过滤即可。
-- 这一列很稀疏（大多数文件夹是 NULL），部分索引只给认得角色的行建条目。
CREATE INDEX IF NOT EXISTS idx_folder_account_role
    ON folder (account_id, role)
    WHERE role IS NOT NULL;
