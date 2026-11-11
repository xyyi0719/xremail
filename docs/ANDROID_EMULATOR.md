# 用模拟器做 Android 运行时验收

> 状态：**环境已全部备好，但本沙盒跑不完一次完整验收**。
> 原因见 §4。最后更新 2026-11-11。

---

## 1. 已经准备好的东西（都在 `.devcache/` 下，可复用）

| 组件 | 位置 | 说明 |
|---|---|---|
| emulator 37.2.13 | `.devcache/emu/` | 从 `dl.google.com` 直接下 zip 解压（`sdkmanager` 拉 manifest 会失败） |
| 系统镜像 android-30 | `.devcache/sdk-local/system-images/android-30/google_apis/x86_64/` | 1.44 GB，SQLite/system.img 等齐全 |
| AVD | `.devcache/android-avd/xremail-api30.{ini,avd}` | **手写**的 config（`avdmanager` 认不出镜像，见 §3） |
| APK | `apps/shell/src-tauri/gen/android/app/build/outputs/apk/universal/debug/` | 254 MB，universal debug |

> SDK 根目录 `/home/xl/Android/Sdk` 是**只读**的（容器限制），所以建了
> `.devcache/sdk-local/`，把可写部分放这里、其余用软链指回系统 SDK。

## 2. 启动命令（已验证能 `BOOT_COMPLETED`）

```bash
cd .devcache
export ANDROID_HOME=$PWD/sdk-local ANDROID_SDK_ROOT=$PWD/sdk-local
export ANDROID_AVD_HOME=$PWD/android-avd ANDROID_SDK_HOME=$PWD
export XDG_RUNTIME_DIR=$PWD/xdg-run        # 默认 /run/user/1000 是只读的
./emu/emulator/emulator -avd xremail-api30 \
  -accel off -gpu swiftshader -no-window -no-audio -no-boot-anim \
  -no-snapshot -read-only
```

**实测**：`sys.boot_completed=1` 用时 **230 秒**，`adb devices` 显示
`emulator-5554 device`。

## 3. 踩过的坑（下次直接用本文，不要重走）

1. **没有 `/dev/kvm`**：容器里 `CapEff=0`，`mknod /dev/kvm` 被拒（Operation not
   permitted）。所以只能用 `-accel off`（TCG 纯软件模拟），慢 10 倍以上。
2. **`sdkmanager` 下不了包**：它拉的 manifest 端点不通；`dl.google.com` 的
   归档下载是通的 → 直接从 `repository2-3.xml` / `sys-img2-1.xml` 找 zip URL 下。
3. **`avdmanager` 建不了 AVD**：解压的镜像缺 `package.xml`，手写一个它仍
   报「Package path is not valid」。**绕过**：手写 `config.ini` + `<name>.ini`，
   `emulator -list-avds` 能认。
4. **`/tmp` 在两次命令之间被清空**：大文件必须下到 `.devcache/`（工作区）。
5. **`/run/user/1000` 只读**：设 `XDG_RUNTIME_DIR` 到工作区，否则
   `Failed to create jwk directory` 直接 FATAL。
6. **`-read-only` 是必需的**：不加会报「multiple emulators with the same AVD」
   并 `PANIC: Broken AVD system path`。原因就是上面那条：只读 SDK。

## 4. 为什么本沙盒跑不完（已知限制，不是配置错误）

模拟器需要**连续运行 4 分钟以上**才能到 adb 在线，完整验收（安装 254MB APK
+ 启动应用 + 存授权码 + 冷启动重开）还要再加几分钟。但这个执行环境：

* 单次命令有约 5 分钟硬上限；
* 后台 job 也在 4 分钟左右被回收（实测两次都是 exit 143）；
* 子进程一旦存活，shell 就不返回，无法「启动后离开、稍后回来操作」。

**结论**：需要有更长生命周期的机器（或能跑 `docker`/`qemu` 的宿主）来执行。
代码侧的准备（见 §1）已经做完。

## 5. 换到合适环境后要验什么

对应 `docs/ROADMAP.md` P4 的第 2、3 项：

1. `adb install -r <apk>` → `adb shell am start -n com.xremail.app/.MainActivity`
2. 在应用里**添加账户**（选服务商 → 邮箱 + 授权码）。
   **注意**：这一步会真正写入 Android Keystore（`secrets/android_store.rs`），
   是本轮代码**从未被真实执行过**的地方。
3. **杀掉进程再重开**：授权码不该再被要求（证明 Keystore 的 set→get 往返成立）。
4. 收一封邮件、发一封邮件、看本地缓存在重启后还在。
5. 后台限制如实展示（息屏 / 切后台后同步行为）。
