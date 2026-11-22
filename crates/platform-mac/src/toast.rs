//! アプリ内の通知パネル：OS の通知が使えないとき（未許可、または ad-hoc 署名のビルドで
//! 通知センターに登録できないとき）の代わりに、画面右上隅へ数秒だけ表示する。
//! 確認ポップアップと同じく、会議アプリからフォーカスを奪わない。

use std::path::PathBuf;

use meetrec_core::platform::Notice;
use meetrec_core::text;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSButton, NSPanel, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::panel::{MARGIN, WIDTH, floating_panel, show_top_right};
use crate::prompt;

const HEIGHT: f64 = 104.0;
/// 自動で閉じるまでの秒数。
const SHOW_SECONDS: f64 = 8.0;
/// 確認ポップアップと重ならないように下げる量（ポップアップの高さ＋タイトルバーと余白のぶん）。
const BELOW_PROMPT: f64 = prompt::HEIGHT + 32.0;

pub struct Toast {
    panel: Retained<NSPanel>,
    timer: Retained<NSTimer>,
    /// 「フォルダを開く」で開くパス。
    pub open: Option<PathBuf>,
}

impl Toast {
    /// `target` は `toastOpen:` と `toastClose:` を実装するデリゲート。
    pub fn show(
        mtm: MainThreadMarker,
        notice: &Notice,
        target: &AnyObject,
        below_prompt: bool,
    ) -> Toast {
        let panel = floating_panel(mtm, &notice.title, HEIGHT);

        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(&notice.body), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(MARGIN, 44.0),
            NSSize::new(WIDTH - MARGIN * 2.0, 52.0),
        ));
        // SAFETY: target はアプリ終了まで生きるデリゲート。
        let (close, open) = unsafe {
            (
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(text::TOAST_CLOSE),
                    Some(target),
                    Some(sel!(toastClose:)),
                    mtm,
                ),
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(text::TOAST_OPEN),
                    Some(target),
                    Some(sel!(toastOpen:)),
                    mtm,
                ),
            )
        };
        close.setFrame(NSRect::new(
            NSPoint::new(WIDTH - 250.0, 8.0),
            NSSize::new(120.0, 30.0),
        ));
        open.setFrame(NSRect::new(
            NSPoint::new(WIDTH - 130.0, 8.0),
            NSSize::new(118.0, 30.0),
        ));
        if notice.open.is_none() {
            // 開く先がない通知は「閉じる」だけを右端に置く。
            close.setFrame(open.frame());
        }
        if let Some(content) = panel.contentView() {
            content.addSubview(&label);
            content.addSubview(&close);
            if notice.open.is_some() {
                content.addSubview(&open);
            }
        }

        // 確認ポップアップが出ているときは、重ならないようにその下へ出す。
        let drop = if below_prompt { BELOW_PROMPT } else { 0.0 };
        show_top_right(&panel, mtm, drop);
        // SAFETY: タイマーは target のセレクタを呼ぶ。target はアプリ終了まで生きる。
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                SHOW_SECONDS,
                target,
                sel!(toastClose:),
                None,
                false,
            )
        };
        Toast {
            panel,
            timer,
            open: notice.open.clone(),
        }
    }

    pub fn close(&self) {
        // 次のパネルを古いタイマーが閉じてしまわないよう、必ず止める。
        self.timer.invalidate();
        self.panel.orderOut(None);
        self.panel.close();
    }
}
