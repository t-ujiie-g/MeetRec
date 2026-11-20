//! メニューバーの常駐アイコンとメニュー。

use block2::RcBlock;
use meetrec_core::config::Mode;
use meetrec_core::platform::{MenuAction, TrayModel};
use meetrec_core::text;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBezierPath, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

// メニュー項目のタグ（デリゲートの menuAction: で MenuAction に戻す）。
const TAG_RECORD: isize = 1;
const TAG_STOP: isize = 2;
const TAG_OPEN: isize = 3;
const TAG_AUTOSTART: isize = 4;
const TAG_DIAG: isize = 5;
const TAG_QUIT: isize = 6;
const TAG_MODE_ASK: isize = 10;
const TAG_MODE_AUTO: isize = 11;
const TAG_MODE_OFF: isize = 12;

pub fn action_for_tag(tag: isize) -> Option<MenuAction> {
    Some(match tag {
        TAG_RECORD => MenuAction::RecordNow,
        TAG_STOP => MenuAction::StopNow,
        TAG_OPEN => MenuAction::OpenFolder,
        TAG_AUTOSTART => MenuAction::ToggleAutostart,
        TAG_DIAG => MenuAction::SaveDiagnostics,
        TAG_QUIT => MenuAction::Quit,
        TAG_MODE_ASK => MenuAction::SetMode(Mode::Ask),
        TAG_MODE_AUTO => MenuAction::SetMode(Mode::Auto),
        TAG_MODE_OFF => MenuAction::SetMode(Mode::Off),
        _ => return None,
    })
}

/// 18pt のアイコンを実行時に描く。待機中は輪＋点（テンプレート画像＝メニューバーの色に従う）、
/// 録画中は赤い塗りつぶしの丸。
fn icon(recording: bool) -> Retained<NSImage> {
    let size = NSSize::new(18.0, 18.0);
    let handler = RcBlock::new(move |_rect: NSRect| -> Bool {
        if recording {
            NSColor::systemRedColor().set();
            let r = NSRect::new(NSPoint::new(2.5, 2.5), NSSize::new(13.0, 13.0));
            NSBezierPath::bezierPathWithOvalInRect(r).fill();
        } else {
            NSColor::blackColor().set();
            let ring = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
                NSPoint::new(3.0, 3.0),
                NSSize::new(12.0, 12.0),
            ));
            ring.setLineWidth(1.5);
            ring.stroke();
            let dot = NSRect::new(NSPoint::new(6.5, 6.5), NSSize::new(5.0, 5.0));
            NSBezierPath::bezierPathWithOvalInRect(dot).fill();
        }
        Bool::YES
    });
    let image = NSImage::imageWithSize_flipped_drawingHandler(size, false, &handler);
    image.setTemplate(!recording);
    image
}

pub struct Tray {
    item: Retained<NSStatusItem>,
    // NSStatusItem はメニューを保持するが、項目は更新のため個別に持つ。
    _menu: Retained<NSMenu>,
    status: Retained<NSMenuItem>,
    record: Retained<NSMenuItem>,
    modes: [(Mode, Retained<NSMenuItem>); 3],
    autostart: Retained<NSMenuItem>,
    idle_icon: Retained<NSImage>,
    rec_icon: Retained<NSImage>,
    mtm: MainThreadMarker,
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    tag: isize,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSMenuItem> {
    // SAFETY: target はアプリ終了まで生きるデリゲート。action はそのセレクタ。
    unsafe {
        let item = NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        );
        item.setTarget(Some(target));
        item.setTag(tag);
        item
    }
}

impl Tray {
    pub fn new(mtm: MainThreadMarker, target: &AnyObject) -> Tray {
        let action = sel!(menuAction:);
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let status = menu_item(mtm, text::STATUS_IDLE, 0, target, action);
        status.setEnabled(false);
        menu.addItem(&status);
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let record = menu_item(mtm, text::MENU_RECORD_NOW, TAG_RECORD, target, action);
        menu.addItem(&record);
        menu.addItem(&menu_item(
            mtm,
            text::MENU_OPEN_FOLDER,
            TAG_OPEN,
            target,
            action,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let mode_parent = menu_item(mtm, text::MENU_MODE, 0, target, action);
        let sub = NSMenu::new(mtm);
        sub.setAutoenablesItems(false);
        let modes = [
            (Mode::Ask, TAG_MODE_ASK),
            (Mode::Auto, TAG_MODE_AUTO),
            (Mode::Off, TAG_MODE_OFF),
        ]
        .map(|(mode, tag)| {
            let it = menu_item(mtm, text::mode_label(mode), tag, target, action);
            sub.addItem(&it);
            (mode, it)
        });
        mode_parent.setSubmenu(Some(&sub));
        menu.addItem(&mode_parent);

        let autostart = menu_item(mtm, text::MENU_AUTOSTART, TAG_AUTOSTART, target, action);
        menu.addItem(&autostart);
        menu.addItem(&menu_item(
            mtm,
            text::MENU_DIAGNOSTICS,
            TAG_DIAG,
            target,
            action,
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&menu_item(mtm, text::MENU_QUIT, TAG_QUIT, target, action));

        item.setMenu(Some(&menu));
        let tray = Tray {
            item,
            _menu: menu,
            status,
            record,
            modes,
            autostart,
            idle_icon: icon(false),
            rec_icon: icon(true),
            mtm,
        };
        tray.set_icon(false);
        tray
    }

    fn set_icon(&self, recording: bool) {
        if let Some(button) = self.item.button(self.mtm) {
            button.setImage(Some(if recording {
                &self.rec_icon
            } else {
                &self.idle_icon
            }));
            button.setToolTip(Some(&NSString::from_str(text::APP_NAME)));
        }
    }

    pub fn set(&self, model: &TrayModel) {
        let on_off = |on: bool| {
            if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            }
        };
        self.status.setTitle(&NSString::from_str(&model.status));
        let (title, tag) = if model.recording {
            (text::MENU_STOP, TAG_STOP)
        } else {
            (text::MENU_RECORD_NOW, TAG_RECORD)
        };
        self.record.setTitle(&NSString::from_str(title));
        self.record.setTag(tag);
        for (mode, item) in &self.modes {
            item.setState(on_off(*mode == model.mode));
        }
        self.autostart.setState(on_off(model.autostart));
        self.set_icon(model.recording);
    }
}
