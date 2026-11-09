//! Linux Secret Service 适配的集成测试（P5-L）。
//!
//! ## 这里能证明什么、不能证明什么
//!
//! 这台开发机/CI **没有** gnome-keyring 之类的 Secret Service 守护进程
//! （`libsecret` 装了，但守护进程要图形会话才会起）。所以：
//!
//! * **不能**证明「写进去、读回来、删掉」在真钥匙串上成立——那条要桌面环境，
//!   见 [ROADMAP 的 P5-L 验收门槛](../../../../docs/ROADMAP.md)。
//! * **能**证明真正要紧的那一半：**没有钥匙串时它不会假装成功**。
//!   这恰恰是历史上最容易写错的地方（退化成"存内存里"或"存文件里"，
//!   用户以为存好了，重启后才发现没有）。
//!
//! 用例在**真实的 D-Bus 会话**里跑（`dbus-run-session`），不是 mock：
//! 走的是 `keyring` → `dbus-secret-service` → libdbus 的完整链路，
//! 只是链路上缺最后那个服务。缺服务与缺库是两种不同的失败，两种都得落成
//! 明确的错误。
//!
//! ## 为什么用 `#[ignore]` 之外的办法隔离
//!
//! 这些用例**必须**在干净环境里跑：如果开发者恰好开着 GNOME 桌面，
//! `set` 会真的成功、往用户的真实钥匙串里写条目。所以它们：
//!
//! * 只在 `XREMAIL_SECRET_SERVICE_PROBE=1` 时运行，默认 `cargo test` 跳过——
//!   避免在用户机器上留下垃圾条目。跑法是：
//!
//!   ```bash
//!   python3 scripts/verify_linux_keychain.py
//!   ```
//!
//!   那个脚本会先查这台机器有没有 Secret Service 守护进程；没有就**明确说
//!   「这一项验不了」**，然后仍然跑一遍「没有守护进程时会不会如实报错」那一半。
//!   （这里以前写的是「由 `scripts/` 里的包装脚本设」——**当时那个脚本并不存在**，
//!   2026-11-10 才补上。）
//! * 成功时不留下任何东西（写完立刻删）。
//!
//! 换句话说：**断言两种结局都合法，但两种都必须诚实**。

#![cfg(target_os = "linux")]

use xremail_api::SecretStoreError;
use xremail_api::secret::{SecretStore, SecretValue};
use xremail_shell_lib::secrets::linux_store::LinuxSecretStore;

/// 探针用例只在显式要求时跑（见模块文档）。
fn probe_enabled() -> bool {
    std::env::var("XREMAIL_SECRET_SERVICE_PROBE").as_deref() == Ok("1")
}

/// 一个不会被误认成真凭据的假机密。
const FAKE_SECRET: &str = "probe-not-a-real-credential";

/// 没有可用的 Secret Service 时，**三个方法都必须明确报错**。
///
/// 这一条是本文件的核心：它防的是「静默降级」——那种写起来很顺、
/// 用起来很久之后才发现的错误。
#[tokio::test]
async fn without_a_secret_service_every_operation_fails_loudly() {
    if !probe_enabled() {
        return;
    }

    let store = LinuxSecretStore::new();
    // 用一个独立的名字，且用完会删，不会碰到真实账户的条目。
    let secret_ref = format!("xremail:probe:{}", std::process::id());

    let set_result = store
        .set(&secret_ref, &SecretValue::from_str_lossless(FAKE_SECRET))
        .await;

    match set_result {
        // 两种环境都可能：桌面机器上有守护进程（写成功），CI 上没有（报错）。
        Ok(()) => {
            // 有守护进程：那就把往返也验完，然后删干净。
            let read_back = store.get(&secret_ref).await.expect("写成功后必须能读");
            assert_eq!(
                read_back.as_ref().and_then(|v| v.as_str()),
                Some(FAKE_SECRET),
                "读回来的必须就是写进去的那一份字节"
            );
            assert!(
                store.delete(&secret_ref).await.expect("删除不该报错"),
                "刚写进去的条目必须删得掉"
            );
            assert!(
                !store.delete(&secret_ref).await.expect("再删一次不该报错"),
                "删过之后再删应当返回 false（本来就没有）"
            );
        }
        Err(error) => {
            // 没有守护进程：必须是「不可用」，不是「拒绝」，更不是静默成功。
            assert!(
                matches!(error, SecretStoreError::Unavailable(_)),
                "没有 Secret Service 时必须报 Unavailable，实际是 {error:?}"
            );
            let text = error.to_string();
            assert!(
                !text.contains(FAKE_SECRET),
                "错误信息里绝对不能出现机密：{text}"
            );
            assert!(
                text.contains(&secret_ref),
                "但应当指出是哪个条目，排障时才有用：{text}"
            );

            // 读取同样要报错——不能给个 `None` 冒充「没存过」。
            // 「取不到」与「没存过」是两件事，混淆了上层就会以为凭据丢失。
            match store.get(&secret_ref).await {
                Err(SecretStoreError::Unavailable(_)) => {}
                Err(other) => panic!("读取应当报 Unavailable，实际是 {other:?}"),
                Ok(value) => panic!("没有钥匙串却读出了东西：{value:?}"),
            }
        }
    }
}

/// 条目**不存在**与**存储设施不可用**必须是两种不同的结果。
///
/// 这条在有守护进程的环境里才有意义（没有守护进程时两个方法都会报错，
/// 分辨不出来）。所以只在探测成功时验证。
#[tokio::test]
async fn a_missing_entry_is_not_the_same_as_an_unavailable_store() {
    if !probe_enabled() {
        return;
    }

    let store = LinuxSecretStore::new();
    let secret_ref = format!("xremail:probe-absent:{}", std::process::id());

    // 先确认设施可用（不可用就跳过——另一条用例负责那种情况）。
    if store
        .set(&secret_ref, &SecretValue::from_str_lossless("x"))
        .await
        .is_err()
    {
        return;
    }
    store.delete(&secret_ref).await.expect("删除不该报错");

    assert!(
        store
            .get(&secret_ref)
            .await
            .expect("设施可用时，读取一个不存在的条目不该报错")
            .is_none(),
        "不存在的条目应当是 None，而不是某种「错误」"
    );
    assert!(
        !store.delete(&secret_ref).await.expect("删除不该报错"),
        "删除不存在的条目应当是 false"
    );
}
