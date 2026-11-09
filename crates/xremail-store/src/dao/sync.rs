//! 跨表的同步提交。
//!
//! 为什么需要单独一个模块：一轮文件夹同步要同时动**三张表**
//! （`message` / `folder` / `message_body` 的级联），
//! 而它们必须**原子**——
//!
//! * epoch 切换时，旧世代的邮件要清掉、新世代的邮件要写入、游标要更新。
//!   这三件事**必须同时成功或同时失败**：只做一半会留下
//!   「新 epoch 的游标 + 旧 epoch 的邮件」这种不一致状态，
//!   下一轮差分会基于错的 epoch 算，结果是彻底错误
//!   （见 [ARCHITECTURE §4.3](../../../../docs/ARCHITECTURE.md)）。
//! * 同理，普通增量同步的「写入 + 删除 + 更新游标」也要原子：
//!   半截数据会让下一轮的 UID 差分把没写进去的邮件当成「服务器已删」。

use sqlx::SqlitePool;
use xremail_core::AppResult;
use xremail_core::model::{Folder, Message};

use crate::codec::{addresses_from_json, addresses_to_json};
use crate::error::map_sqlx;

/// 一轮文件夹同步要落库的内容。
#[derive(Debug, Clone, Default)]
pub struct FolderSyncWrite {
    /// 要写入（或覆盖）的邮件。
    pub upserts: Vec<Message>,
    /// 要删掉的 UID（服务器上已经没有的）。
    ///
    /// 只在**当前 epoch** 下有意义。
    pub delete_uids: Vec<i64>,
    /// 是否清掉**其他 epoch** 的邮件。
    ///
    /// epoch 切换时为 `true`：旧世代的 UID 映射全部作废。
    pub drop_other_epochs: bool,
}

/// 在**一个事务**里提交一轮文件夹同步。
///
/// 步骤：
/// 1. （可选）清掉其他 epoch 的邮件——epoch 切换时旧世代全部作废；
/// 2. 写入新邮件（按 `(account_id, folder_id, uidvalidity, uid)` upsert）；
/// 3. 删掉服务器已删的 UID；
/// 4. 更新文件夹游标（`uidvalidity` / `last_uid` / `last_sync_at`）。
///
/// 返回被删掉的邮件数。
///
/// ## 提前 `return` 也走一次**确定的**回滚（R54）
///
/// 第 1 步「epoch 切换却没给新 `uidvalidity`」是参数校验失败，会在
/// `drop_other_epochs` 那一段就返回 `Invalid`。此时事务里还没有任何写语句
/// （清旧世代是第一句写，它排在这次校验之后），所以**即使** sqlx 的异步 drop
/// 没能及时回滚，库里的数据也不会不一致——这是本次复核的结论，不是猜测：
/// 把这里的 `rollback` 去掉，用例照样绿。
///
/// 之所以仍然显式 `rollback`：不把「数据没被改过」这件事押在
/// 「`Transaction` 在异步 drop 里补做 ROLLBACK」上。那种保证依赖
/// 运行时线程、进程退出时机与连接关闭这三件事，而这里只要一句话就能不依赖它们；
/// 顺手让「失败路径一定回滚」这条不变量在本函数里看得见。
pub async fn commit_folder_sync(
    pool: &SqlitePool,
    folder: &Folder,
    write: &FolderSyncWrite,
) -> AppResult<usize> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| map_sqlx(e, "commit_folder_sync: begin"))?;

    // 1. 旧世代作废。
    if write.drop_other_epochs {
        let Some(current) = folder.uidvalidity else {
            // 显式回滚：见函数文档的「提前 return 也算回滚」。
            tx.rollback()
                .await
                .map_err(|e| map_sqlx(e, "commit_folder_sync: rollback"))?;
            return Err(xremail_core::AppError::Invalid(
                "epoch 切换时必须给出新的 uidvalidity".into(),
            ));
        };
        sqlx::query!(
            r#"
            DELETE FROM message
            WHERE account_id = ? AND folder_id = ? AND uidvalidity != ?
            "#,
            folder.account_id,
            folder.id,
            current
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "commit_folder_sync: drop other epochs"))?;
    }

    // 2. 写入新邮件。
    for message in &write.upserts {
        let from_addr = addresses_to_json(&message.from, "message.from")?;
        let to_addr = addresses_to_json(&message.to, "message.to")?;
        let cc_addr = addresses_to_json(&message.cc, "message.cc")?;
        let bcc_addr = addresses_to_json(&message.bcc, "message.bcc")?;

        sqlx::query!(
            r#"
            INSERT INTO message (
                id, account_id, folder_id, uid, uidvalidity,
                message_id, subject, snippet,
                from_addr, to_addr, cc_addr, bcc_addr,
                date_sent, internal_date, flags, size, has_attachment
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (account_id, folder_id, uidvalidity, uid) DO UPDATE SET
                message_id     = excluded.message_id,
                subject        = excluded.subject,
                snippet        = excluded.snippet,
                from_addr      = excluded.from_addr,
                to_addr        = excluded.to_addr,
                cc_addr        = excluded.cc_addr,
                bcc_addr       = excluded.bcc_addr,
                date_sent      = excluded.date_sent,
                internal_date  = excluded.internal_date,
                flags          = excluded.flags,
                size           = excluded.size,
                has_attachment = excluded.has_attachment
            "#,
            message.id,
            message.account_id,
            message.folder_id,
            message.uid,
            message.uidvalidity,
            message.message_id,
            message.subject,
            message.snippet,
            from_addr,
            to_addr,
            cc_addr,
            bcc_addr,
            message.date_sent,
            message.internal_date,
            i64::from(message.flags.bits()),
            message.size,
            message.has_attachment,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx(e, "commit_folder_sync: upsert"))?;
    }

    // 3. 删掉服务器已删的 UID。
    let mut deleted: u64 = 0;
    if let Some(uidvalidity) = folder.uidvalidity {
        for uid in &write.delete_uids {
            let result = sqlx::query!(
                r#"
                DELETE FROM message
                WHERE account_id = ? AND folder_id = ? AND uidvalidity = ? AND uid = ?
                "#,
                folder.account_id,
                folder.id,
                uidvalidity,
                uid
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx(e, "commit_folder_sync: delete"))?;
            deleted += result.rows_affected();
        }
    }

    // 4. 更新游标。
    sqlx::query!(
        r#"
        UPDATE folder
        SET uidvalidity = ?, last_uid = ?, highest_modseq = ?, last_sync_at = ?
        WHERE id = ?
        "#,
        folder.uidvalidity,
        folder.last_uid,
        folder.highest_modseq,
        folder.last_sync_at,
        folder.id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| map_sqlx(e, "commit_folder_sync: update folder"))?;

    tx.commit()
        .await
        .map_err(|e| map_sqlx(e, "commit_folder_sync: commit"))?;

    Ok(deleted as usize)
}

/// 读一封邮件的地址列。
///
/// ## 当前**没有调用方**（2026-11-10 用 `scripts/find_unused_pub_fns.py` 查出）
///
/// 注释原本写「测试辅助」——**但测试里也没有用它**（`crates/xremail-store/tests/`
/// 与 `apps/` 都搜不到）。`COMMIT_MESSAGE` 里的地址列由 `commit_folder_sync`
/// 直接写入，读取方是 `message` 表的常规查询，不走这里。
///
/// 保留的理由：它是覆盖 `from_addr` / `to_addr` 两列的唯一读取入口，
/// 将来若要核对「写进去的地址与读出来的一致」会用到。**别把它当活代码依赖。**
pub async fn read_addresses(pool: &SqlitePool, id: &str) -> AppResult<(Vec<String>, Vec<String>)> {
    let row = sqlx::query!(
        r#"
        SELECT from_addr, to_addr FROM message WHERE id = ?
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| map_sqlx(e, "read_addresses"))?;

    let Some(row) = row else {
        return Ok((Vec::new(), Vec::new()));
    };
    let from = addresses_from_json::<Vec<xremail_core::model::Address>>(&row.from_addr, "from")?;
    let to = addresses_from_json::<Vec<xremail_core::model::Address>>(&row.to_addr, "to")?;

    Ok((
        from.into_iter().map(|a| a.email).collect(),
        to.into_iter().map(|a| a.email).collect(),
    ))
}
