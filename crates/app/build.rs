fn main() {
    // Windows: DPI 対応・共通コントロール v6・UTF-8 のマニフェストを埋め込む（rc.exe 不要）。
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::manifest::{ActiveCodePage, DpiAwareness};
        use embed_manifest::{embed_manifest, new_manifest};
        let manifest = new_manifest("MeetRec")
            .active_code_page(ActiveCodePage::Utf8)
            .dpi_awareness(DpiAwareness::PerMonitorV2);
        embed_manifest(manifest).expect("embed manifest");
        // 単体 exe で配る（ダウンロードフォルダなどから起動される）ため、依存 DLL は System32 からだけ読む。
        // 既定では exe と同じフォルダが先に探され、そこに置かれた偽の DLL が読み込まれてしまう。
        // 0x800 = LOAD_LIBRARY_SEARCH_SYSTEM32。
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
