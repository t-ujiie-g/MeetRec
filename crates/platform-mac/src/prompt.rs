//! 確認ポップアップ：画面右上隅の小さな非アクティブ化パネル。
//! 会議アプリからフォーカスを奪わず、答えるまで（または会議が終わるまで）表示し続ける。

use meetrec_core::platform::PromptModel;
use meetrec_core::text;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSButton, NSControlStateValueOn, NSPanel, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::panel::{MARGIN, WIDTH, floating_panel, show_top_right};

pub const HEIGHT: f64 = 118.0;

pub struct Prompt {
    panel: Retained<NSPanel>,
    always: Retained<NSButton>,
}

impl Prompt {
    pub fn show(mtm: MainThreadMarker, model: &PromptModel, target: &AnyObject) -> Prompt {
        let panel = floating_panel(mtm, text::APP_NAME, HEIGHT);

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
        skip.setFrame(NSRect::new(
            NSPoint::new(WIDTH - 250.0, 8.0),
            NSSize::new(120.0, 30.0),
        ));
        record.setFrame(NSRect::new(
            NSPoint::new(WIDTH - 130.0, 8.0),
            NSSize::new(118.0, 30.0),
        ));
        // Return キーで「録画する」。
        record.setKeyEquivalent(&NSString::from_str("\r"));
        if let Some(content) = panel.contentView() {
            content.addSubview(&label);
            content.addSubview(&always);
            content.addSubview(&skip);
            content.addSubview(&record);
        }

        show_top_right(&panel, mtm, 0.0);
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
