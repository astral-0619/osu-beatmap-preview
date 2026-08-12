#!/usr/bin/env python3
"""CI 助手：给 flutter create 生成的 android 工程注入本项目的依赖和权限。

- build.gradle.kts: media3-exoplayer（音频主时钟）+ okhttp（下载 .osz）
- AndroidManifest.xml: INTERNET 权限（release 包下载谱面需要）
"""
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parent.parent
gradle = root / "app" / "android" / "app" / "build.gradle.kts"
manifest = root / "app" / "android" / "app" / "src" / "main" / "AndroidManifest.xml"

DEPS = '''dependencies {
    implementation("androidx.media3:media3-exoplayer:1.4.1")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
}
'''

g = gradle.read_text()
if "media3-exoplayer" not in g:
    gradle.write_text(g.rstrip() + "\n\n" + DEPS)
    print("injected gradle dependencies")
else:
    print("gradle dependencies already present")

m = manifest.read_text()
if "android.permission.INTERNET" not in m:
    m = re.sub(
        r"(<manifest\b[^>]*>)",
        r'\1\n    <uses-permission android:name="android.permission.INTERNET" />',
        m,
        count=1,
    )
    manifest.write_text(m)
    print("injected INTERNET permission")
else:
    print("INTERNET permission already present")
