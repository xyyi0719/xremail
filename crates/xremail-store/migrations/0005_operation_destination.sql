-- 移动 / 删除操作要记「挪到哪个文件夹」。
--
-- 可空：flags 类操作没有目标文件夹。
--
-- 存的是**本地** `folder.id`，不是线路上的邮箱名。理由：邮箱名会被服务器改写
-- （改名、分隔符变化、modified UTF-7 归一），本地映射变一次不该让整条队列作废；
-- 提交时按 `folder.remote_name` 现取，取不到就报错让人看见。
ALTER TABLE mail_operation ADD COLUMN destination_folder_id TEXT;
