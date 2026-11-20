//! 確認ポップアップ：画面右上隅の小さな非アクティブ化パネル。
//! 会議アプリからフォーカスを奪わず、答えるまで（または会議が終わるまで）表示し続ける。

use meetrec_core::platform::PromptModel;
use meetrec_core::text;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSControlStateValueOn, NSPanel, NSScreen, NSStatusWindowLevel,
    NSTextField, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

const WIDTH: f64 = 360.0;
const HEIGHT: f64 = 118.0;
const MARGIN: f64 = 16.0;

pub struct Prompt {
    panel: Retained<NSPanel>,
    always: Retained<NSButton>,
}

impl Prompt {
    pub fn show(mtm: MainThreadMarker, model: &PromptModel, target: &AnyObject) -> Prompt {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
        // タイトルバーのみ（閉じるボタンなし）。NonactivatingPanel でフォーカスを奪わない。
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::NonactivatingPanel;
        // SAFETY: メインスレッドで通常の初期化子を呼ぶ。
        let panel = unsafe {
            NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: Retained で保持するので、close 時の自動解放は無効にする。
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str(text::APP_NAME));
        panel.setFloatingPanel(true);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary,
        );

        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(&model.message), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(MARGIN, 66.0),
            NSSize::new(WIDTH - MARGIN * 2.0, 40.0),
        ));
        // SAFETY: target はアプリ終了まで生きるデリゲート。
        let (always, skip, record) = unsafe {
            (
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str(&model.always_label),
                    None,
                    None,
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(&model.skip_label),
                    Some(target),
                    Some(sel!(promptSkip:)),
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(&model.record_label),
                    Some(target),
                    Some(sel!(promptRecord:)),
                    mtm,
                ),
            )
        };
        always.setFrame(NSRect::new(
            NSPoint::new(MARGIN, 42.0),
            NSSize::new(WIDTH - MARGIN * 2.0, 20.0),
        ));
        skip.setFrame(NSRect::new(NSPoint::new(WIDTH - 250.0, 8.0), NSSize::new(120.0, 30.0)));
        record.setFrame(NSRect::new(NSPoint::new(WIDTH - 130.0, 8.0), NSSize::new(118.0, 30.0)));
        // Return キーで「録画する」。
        record.setKeyEquivalent(&NSString::from_str("\r"));
        if let Some(content) = panel.contentView() {
            content.addSubview(&label);
            content.addSubview(&always);
            content.addSubview(&skip);
            content.addSubview(&record);
        }

        // メイン画面の右上隅（メニューバーの下）。
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let visible = screen.visibleFrame();
            let frame = panel.frame();
            panel.setFrameOrigin(NSPoint::new(
                visible.origin.x + visible.size.width - frame.size.width - MARGIN,
                visible.origin.y + visible.size.height - frame.size.height - MARGIN,
            ));
        }
        panel.orderFrontRegardless();
        Prompt { panel, always }
    }

    /// 「次回から確認せず録画」がチェックされているか。
    pub fn always_checked(&self) -> bool {
        self.always.state() == NSControlStateValueOn
    }

    pub fn close(&self) {
        self.panel.orderOut(None);
        self.panel.close();
    }
}
