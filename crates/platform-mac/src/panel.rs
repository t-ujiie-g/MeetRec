//! 画面右上隅に出す小さな非アクティブ化パネルの共通部分（確認ポップアップと、通知の代わりのパネル）。
//! 会議アプリからフォーカスを奪わず、全デスクトップとフルスクリーンの上に出す。

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSPanel, NSScreen, NSStatusWindowLevel, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

pub const WIDTH: f64 = 360.0;
pub const MARGIN: f64 = 16.0;

/// タイトルバーだけ（閉じるボタンなし）のパネルを作る。まだ表示はしない。
pub fn floating_panel(mtm: MainThreadMarker, title: &str, height: f64) -> Retained<NSPanel> {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height));
    // NonactivatingPanel でフォーカスを奪わない。
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: Retained で保持するので、close 時の自動解放は無効にする。
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(&NSString::from_str(title));
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setHidesOnDeactivate(false);
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary,
    );
    panel
}

/// メイン画面の右上隅（メニューバーの下）に置いて表示する。`drop` だけ下にずらす。
pub fn show_top_right(panel: &NSPanel, mtm: MainThreadMarker, drop: f64) {
    if let Some(screen) = NSScreen::mainScreen(mtm) {
        let visible = screen.visibleFrame();
        let frame = panel.frame();
        panel.setFrameOrigin(NSPoint::new(
            visible.origin.x + visible.size.width - frame.size.width - MARGIN,
            visible.origin.y + visible.size.height - frame.size.height - MARGIN - drop,
        ));
    }
    panel.orderFrontRegardless();
}
