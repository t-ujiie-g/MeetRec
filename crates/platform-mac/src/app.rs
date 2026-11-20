//! アプリ骨格：NSApplication・デリゲート・イベント配送と `Platform` 実装。
//!
//! すべての UI イベントはメインスレッドの `dispatch` を通り、再入しないように
//! キューで直列化してから `App::on_event` に渡す。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use meetrec_core::platform::{
    App, LocalTime, MicUser, Notice, Permissions, Platform, PromptAnswer, PromptModel, RawUsage,
    RecordRequest, RecorderEvent, RecordingSummary, TrayModel, UiEvent, WindowInfo,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSMenuItem, NSWorkspace,
    NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{
    NSActivityOptions, NSNotification, NSProcessInfo, NSRunLoop, NSRunLoopCommonModes, NSString,
    NSTimer, NSURL,
};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::capture::Recorder;
use crate::notify::Notifier;
use crate::probe::Probe;
use crate::prompt::Prompt;
use crate::sys;
use crate::tray::{action_for_tag, Tray};

/// メインスレッドで順に処理する項目。
pub enum Item {
    Ui(UiEvent),
    /// SCStream がエラーで停止した（録画層で復旧を試みる）。
    StreamStopped(String),
}

struct State {
    app: Box<dyn App>,
    platform: MacPlatform,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static PENDING_APP: RefCell<Option<Box<dyn App>>> = const { RefCell::new(None) };
    static QUEUE: RefCell<VecDeque<Item>> = const { RefCell::new(VecDeque::new()) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    // ボタンのアクションから（イベント処理中でも）読めるよう、State とは別に持つ。
    static PROMPT: RefCell<Option<Prompt>> = const { RefCell::new(None) };
}

/// 任意のスレッドから：メインキュー経由でイベントを届ける。
pub fn post(item: Item) {
    DispatchQueue::main().exec_async(move || dispatch(item));
}

pub fn post_recorder(event: RecorderEvent) {
    post(Item::Ui(UiEvent::Recorder(event)));
}

/// メインスレッド専用。処理中に呼ばれた場合はキューに積むだけ（再入しない）。
fn dispatch(item: Item) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        // モーダル表示中などに溜まったティックは 1 つにまとめる。
        let is_tick = |i: &Item| matches!(i, Item::Ui(UiEvent::Tick));
        if !(is_tick(&item) && q.iter().any(is_tick)) {
            q.push_back(item);
        }
    });
    if BUSY.replace(true) {
        return;
    }
    while let Some(item) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else { return };
            let event = match item {
                Item::Ui(event) => Some(event),
                Item::StreamStopped(msg) => st.platform.stream_stopped(&msg),
            };
            if let Some(event) = event {
                st.app.on_event(&mut st.platform, event);
            }
        });
    }
    BUSY.set(false);
}

pub fn open_path(path: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    if !NSWorkspace::sharedWorkspace().openURL(&url) {
        meetrec_core::warn!("開けません: {}", path.display());
    }
}

fn open_url(url: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeetRecAppDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.launch();
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _sender: &NSApplication) -> NSApplicationTerminateReply {
            if QUITTING.get() {
                // メニューの「終了」：録画は controller が確定済み。
                return NSApplicationTerminateReply::TerminateNow;
            }
            // ログアウト・シャットダウン等：録画を確定してから終了する。
            if BUSY.get() {
                DispatchQueue::main().exec_async(|| {
                    dispatch(Item::Ui(UiEvent::Shutdown));
                    if let Some(mtm) = MainThreadMarker::new() {
                        NSApplication::sharedApplication(mtm).replyToApplicationShouldTerminate(true);
                    }
                });
                return NSApplicationTerminateReply::TerminateLater;
            }
            dispatch(Item::Ui(UiEvent::Shutdown));
            NSApplicationTerminateReply::TerminateNow
        }
    }

    impl Delegate {
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            if let Some(action) = action_for_tag(sender.tag()) {
                dispatch(Item::Ui(UiEvent::Menu(action)));
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            dispatch(Item::Ui(UiEvent::Tick));
        }

        #[unsafe(method(promptRecord:))]
        fn prompt_record(&self, _sender: &AnyObject) {
            let always = PROMPT.with(|p| p.borrow().as_ref().is_some_and(|p| p.always_checked()));
            dispatch(Item::Ui(UiEvent::Prompt(PromptAnswer::Record { always })));
        }

        #[unsafe(method(promptSkip:))]
        fn prompt_skip(&self, _sender: &AnyObject) {
            dispatch(Item::Ui(UiEvent::Prompt(PromptAnswer::Skip)));
        }

        #[unsafe(method(willSleep:))]
        fn will_sleep(&self, _notification: &NSNotification) {
            dispatch(Item::Ui(UiEvent::WillSleep));
        }

        #[unsafe(method(didWake:))]
        fn did_wake(&self, _notification: &NSNotification) {
            dispatch(Item::Ui(UiEvent::DidWake));
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Delegate> {
        // SAFETY: NSObject の init を呼ぶだけ。
        unsafe { msg_send![Delegate::alloc(mtm), init] }
    }

    fn launch(&self) {
        let mtm = self.mtm();
        let Some(app) = PENDING_APP.with(|a| a.borrow_mut().take()) else {
            return;
        };
        let interval = app.tick_seconds().max(1) as f64;
        let target: &AnyObject = self;
        let platform = MacPlatform {
            mtm,
            delegate: self.retain(),
            tray: Tray::new(mtm, target),
            notifier: Notifier::new(),
            probe: Probe::default(),
            recorder: None,
        };
        STATE.with(|s| *s.borrow_mut() = Some(State { app, platform }));
        dispatch(Item::Ui(UiEvent::Launched));

        // SAFETY: タイマーと通知センターは self を対象にセレクタを呼ぶ。self はアプリ終了まで生きる。
        unsafe {
            let timer = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                interval,
                target,
                sel!(tick:),
                None,
                true,
            );
            // 起床をまとめられるよう許容誤差を付ける（省電力）。
            timer.setTolerance(interval * 0.25);
            // メニュー表示中やモーダル中も止まらないよう common modes に登録する。
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
            let center = NSWorkspace::sharedWorkspace().notificationCenter();
            center.addObserver_selector_name_object(
                target,
                sel!(willSleep:),
                Some(NSWorkspaceWillSleepNotification),
                None,
            );
            center.addObserver_selector_name_object(
                target,
                sel!(didWake:),
                Some(NSWorkspaceDidWakeNotification),
                None,
            );
        }
    }
}

pub struct MacPlatform {
    mtm: MainThreadMarker,
    delegate: Retained<Delegate>,
    tray: Tray,
    notifier: Notifier,
    probe: Probe,
    recorder: Option<Recorder>,
}

impl MacPlatform {
    /// SCStream のエラー停止を処理し、必要なら app に渡すイベントを返す。
    fn stream_stopped(&mut self, msg: &str) -> Option<UiEvent> {
        let recorder = self.recorder.as_mut()?;
        let probe = &mut self.probe;
        meetrec_core::warn!("キャプチャが停止しました: {msg}");
        match recorder.stream_stopped(|id| probe.windows().iter().any(|w| w.id == id)) {
            None => None,
            // 対象ウィンドウが消えた：すぐ再判定させて張り替える。
            Some(true) => Some(UiEvent::Tick),
            Some(false) => Some(UiEvent::Recorder(RecorderEvent::Failed(format!(
                "キャプチャが停止しました: {msg}"
            )))),
        }
    }

    fn mic_status() -> AVAuthorizationStatus {
        // SAFETY: 静的な定数とクラスメソッド。
        unsafe {
            match AVMediaTypeAudio {
                Some(audio) => AVCaptureDevice::authorizationStatusForMediaType(audio),
                None => AVAuthorizationStatus::NotDetermined,
            }
        }
    }
}

impl Platform for MacPlatform {
    fn mic_users(&mut self) -> Vec<MicUser> {
        self.probe.mic_users()
    }

    fn windows(&mut self) -> Vec<WindowInfo> {
        self.probe.windows()
    }

    fn local_time(&self) -> LocalTime {
        sys::local_time()
    }

    fn now_ns(&self) -> u64 {
        sys::now_ns()
    }

    fn free_space(&self, dir: &Path) -> Option<u64> {
        sys::free_space(dir)
    }

    fn resource_usage(&self) -> Option<RawUsage> {
        sys::resource_usage()
    }

    fn diagnostics(&mut self) -> String {
        self.probe.diagnostics()
    }

    fn set_tray(&mut self, model: &TrayModel) {
        self.tray.set(model);
    }

    fn show_prompt(&mut self, prompt: &PromptModel) {
        self.hide_prompt();
        let target: &AnyObject = &self.delegate;
        let shown = Prompt::show(self.mtm, prompt, target);
        PROMPT.with(|p| *p.borrow_mut() = Some(shown));
    }

    fn hide_prompt(&mut self) {
        if let Some(prompt) = PROMPT.with(|p| p.borrow_mut().take()) {
            prompt.close();
        }
    }

    fn notify(&mut self, notice: &Notice) {
        self.notifier.notify(notice);
    }

    fn open_path(&mut self, path: &Path) {
        open_path(path);
    }

    fn set_autostart(&mut self, enabled: bool) -> Result<(), String> {
        // SAFETY: メインアプリ自身をログイン項目として登録/解除する。
        let result = unsafe {
            let service = SMAppService::mainAppService();
            if enabled {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        result.map_err(|e| e.localizedDescription().to_string())
    }

    fn autostart_enabled(&self) -> bool {
        // SAFETY: 状態の読み取りのみ。
        unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled }
    }

    fn permissions(&mut self) -> Permissions {
        Permissions {
            screen: CGPreflightScreenCaptureAccess(),
            microphone: Self::mic_status() == AVAuthorizationStatus::Authorized,
        }
    }

    fn request_permissions(&mut self, missing: Permissions) {
        let mut mic_denied = false;
        if missing.microphone {
            if Self::mic_status() == AVAuthorizationStatus::NotDetermined {
                // 初回：システムの許可ダイアログを出す。
                let handler = RcBlock::new(|granted: Bool| {
                    meetrec_core::info!("マイクの許可: {}", granted.as_bool());
                });
                // SAFETY: 完了ハンドラは任意のキューで呼ばれる。
                unsafe {
                    if let Some(audio) = AVMediaTypeAudio {
                        AVCaptureDevice::requestAccessForMediaType_completionHandler(
                            audio, &handler,
                        );
                    }
                }
            } else {
                mic_denied = true;
            }
        }
        if missing.screen {
            // 初回はシステムのダイアログが出て、設定の一覧に MeetRec が追加される。
            CGRequestScreenCaptureAccess();
        }
        if !missing.screen && !mic_denied {
            return;
        }
        let mut what = Vec::new();
        if missing.screen {
            what.push(
                "「画面収録とシステムオーディオ録音」（会議ウィンドウと会議の音声を録るため）",
            );
        }
        if mic_denied {
            what.push("「マイク」（自分の声を録るため）");
        }
        let alert = NSAlert::new(self.mtm);
        alert.setMessageText(&NSString::from_str("MeetRec に権限が必要です"));
        alert.setInformativeText(&NSString::from_str(&format!(
            "システム設定の「プライバシーとセキュリティ」で、次の項目の MeetRec をオンにしてください。\n\n{}\n\n\
             画面収録を許可した後は MeetRec の再起動が必要です。",
            what.join("\n")
        )));
        alert.addButtonWithTitle(&NSString::from_str("設定を開く"));
        alert.addButtonWithTitle(&NSString::from_str("あとで"));
        NSApplication::sharedApplication(self.mtm).activate();
        if alert.runModal() == NSAlertFirstButtonReturn {
            let pane = if missing.screen {
                "Privacy_ScreenCapture"
            } else {
                "Privacy_Microphone"
            };
            open_url(&format!(
                "x-apple.systempreferences:com.apple.preference.security?{pane}"
            ));
        }
    }

    fn start_recording(&mut self, req: &RecordRequest) -> Result<(), String> {
        if self.recorder.is_some() {
            return Err("既に録画中です".into());
        }
        self.recorder = Some(Recorder::start(req)?);
        Ok(())
    }

    fn retarget(&mut self, window: &WindowInfo) {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.retarget(window);
        }
    }

    fn stop_recording(&mut self) -> Option<RecordingSummary> {
        self.recorder.take().and_then(|mut r| r.stop())
    }

    fn quit(&mut self) {
        QUITTING.set(true);
        // on_event の外で終了させる。
        DispatchQueue::main().exec_async(|| {
            if let Some(mtm) = MainThreadMarker::new() {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        });
    }
}

/// マイクの使用が許可済みか（未許可のままマイクキャプチャを要求しないため）。
pub fn mic_authorized() -> bool {
    MacPlatform::mic_status() == AVAuthorizationStatus::Authorized
}

/// OS のイベントループを開始する（戻らない）。
pub fn run(app: Box<dyn App>) {
    let Some(mtm) = MainThreadMarker::new() else {
        meetrec_core::warn!("メインスレッド以外から起動されました");
        return;
    };
    let nsapp = NSApplication::sharedApplication(mtm);
    // Dock に出さず、メニューバーだけに常駐する。
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    PENDING_APP.with(|a| *a.borrow_mut() = Some(app));
    // App Nap によるタイマーの間引き（検出の遅れ）と、録画中の突然終了を防ぐ。
    // アイドル時のシステムスリープは妨げない。戻り値はアプリ終了まで保持する。
    let _activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str("会議の検出と録画"),
    );
    // NSApplication はデリゲートを弱参照する。run() は戻らないのでここで保持し続ける。
    let delegate = Delegate::new(mtm);
    nsapp.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    nsapp.run();
}
