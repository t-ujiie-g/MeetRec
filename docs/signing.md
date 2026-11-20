# 署名・公証の手順

CI が作る成果物は macOS が ad-hoc 署名、Windows が未署名です。配布用に正式な署名を付ける手順をまとめます。

## macOS：Developer ID 署名と公証（notarization）

前提：Apple Developer Program に加入し、「Developer ID Application」証明書をキーチェーンに入れておく。

```sh
# 1. universal バイナリをビルド
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release -p meetrec --target aarch64-apple-darwin
cargo build --release -p meetrec --target x86_64-apple-darwin
mkdir -p dist
lipo -create -output dist/meetrec \
  target/aarch64-apple-darwin/release/meetrec target/x86_64-apple-darwin/release/meetrec

# 2. .app 化して Developer ID + Hardened Runtime で署名
#    （CODESIGN_IDENTITY を指定すると bundle.sh が --options runtime と entitlements を付ける）
CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  packaging/macos/bundle.sh dist/meetrec dist

# 3. 公証（初回だけ認証情報をキーチェーンに保存）
xcrun notarytool store-credentials meetrec-notary \
  --apple-id "you@example.com" --team-id TEAMID --password "app-specific-password"
xcrun notarytool submit dist/MeetRec-macos-*.zip --keychain-profile meetrec-notary --wait

# 4. チケットを .app に貼り付けて zip を作り直す
xcrun stapler staple dist/MeetRec.app
(cd dist && ditto -c -k --keepParent MeetRec.app MeetRec-macos-notarized.zip)

# 5. 確認
spctl --assess --type execute --verbose dist/MeetRec.app
codesign --verify --deep --strict --verbose=2 dist/MeetRec.app
```

メモ：
- Hardened Runtime ではマイク利用に `com.apple.security.device.audio-input` が必要です（`packaging/macos/entitlements.plist` に設定済み）。
  画面収録は entitlement 不要（TCC の許可のみ）。App Sandbox は使いません（他アプリのウィンドウを録るため）。
- バンドル ID は `packaging/macos/Info.plist` の `CFBundleIdentifier`（既定 `io.github.meetrec.MeetRec`）。自分の ID に変えてください。
  変えると TCC の権限・ログイン項目は別アプリ扱いになります。
- Developer ID で署名すると、アップデートしても画面収録・マイクの権限が引き継がれます（ad-hoc 署名では引き継がれません）。
- CI で行う場合は、証明書（.p12）と公証用の認証情報を GitHub Secrets に入れ、`security import` でキーチェーンに取り込んでから
  上記 2〜4 を実行するステップを `.github/workflows/ci.yml` の macOS ビルドの後に追加します。

## Windows：コード署名

前提：コード署名証明書（OV / EV）または Azure Trusted Signing のアカウント。

```powershell
# 証明書がストアにある場合（Windows SDK の signtool）
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
  /n "Your Company Name" dist\MeetRec.exe
signtool verify /pa /v dist\MeetRec.exe
```

Azure Trusted Signing を使う場合は `azure/trusted-signing-action` を CI の Windows ビルド後に追加します。

メモ：
- 未署名の exe は SmartScreen の警告が出ます。OV 証明書でも評判が蓄積するまでは警告が出ることがあります。
- インストーラは用意していません（単体 exe）。必要になった場合は WiX / Inno Setup で
  `%LOCALAPPDATA%\Programs\MeetRec` へのユーザー単位インストール（管理者権限不要）にするのが適しています。
- ログイン時起動は `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` に exe のパスを登録する方式です。
  exe を移動した場合は、メニューの「ログイン時に起動」を一度オフ→オンにしてください。
