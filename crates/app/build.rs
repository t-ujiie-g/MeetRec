fn main() {
    // Windows: DPI 対応・共通コントロール v6・UTF-8 のマニフェストを埋め込む（rc.exe 不要）。
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::manifest::{ActiveCodePage, DpiAwareness};
        use embed_manifest::{embed_manifest, new_manifest};
        let manifest = new_manifest("MeetRec")
            .active_code_page(ActiveCodePage::Utf8)
            .dpi_awareness(DpiAwareness::PerMonitorV2);
        embed_manifest(manifest).expect("embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
