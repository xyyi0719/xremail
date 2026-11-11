#!/usr/bin/env python3
"""把 release 签名配置注入到 Tauri 生成的 Android gradle 文件里。

## 为什么需要这一步

`apps/shell/src-tauri/gen/` 整个在 `.gitignore` 里——CI 每次由
`tauri android build` **重新生成**这份 Gradle 工程。所以签名配置不能写在
仓库里的 `build.gradle.kts`（改了也不进版本库、CI 看不到），只能在生成之后注入。

## 注入什么

1. 在 `android {` 块里加 `signingConfigs { create("release") { … } }`，
   取值全来自**环境变量**（由 CI 的 secrets 注入，不落盘成文件常量）；
2. 把 `release` buildType 绑上这个签名配置。

## 幂等

已经在里面就跳过——重复执行不会插两遍。
"""
import io
import sys

MARK = "xremail-release-signing"


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "apps/shell/src-tauri/gen/android/app/build.gradle.kts"
    s = io.open(path, encoding="utf-8").read()

    if MARK in s:
        print("签名配置已存在，跳过")
        return 0

    block = f'''
    // {MARK}：由 scripts/inject_android_signing.py 在构建期注入。
    // 所有取值来自环境变量（CI secrets），不写进版本库。
    signingConfigs {{
        create("release") {{
            storeFile = file(System.getenv("ANDROID_KEYSTORE_PATH"))
            storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")
            keyAlias = System.getenv("ANDROID_KEY_ALIAS")
            keyPassword = System.getenv("ANDROID_KEY_PASSWORD")
        }}
    }}
'''
    if "android {" not in s:
        print("找不到 `android {`，文件结构变了？", file=sys.stderr)
        return 1
    s = s.replace("android {", "android {" + block, 1)

    # release buildType 绑签名
    if 'getByName("release") {' in s:
        s = s.replace(
            'getByName("release") {',
            'getByName("release") {\n            signingConfig = signingConfigs.getByName("release")',
            1,
        )
    else:
        print("找不到 release buildType", file=sys.stderr)
        return 1

    io.open(path, "w", encoding="utf-8").write(s)
    print(f"已注入签名配置到 {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
