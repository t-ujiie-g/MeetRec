//! 通知（UNUserNotificationCenter）。クリックで `Notice.open` のフォルダを開く。
//! .app バンドル外で実行された場合や未許可の場合は何もしない（ログには残る）。

use block2::RcBlock;
use meetrec_core::platform::Notice;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_foundation::{NSBundle, NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

const OPEN_KEY: &str = "open";

define_class!(
    // 通知センターのデリゲート。コールバックのスレッドは保証されないので、
    // 状態を持たず、UI 操作はメインキューへ回す。
    #[unsafe(super(NSObject))]
    #[name = "MeetRecNotifyDelegate"]
    struct NotifyDelegate;

    unsafe impl NSObjectProtocol for NotifyDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotifyDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // 常駐アプリは「前面」扱いになるため、明示しないとバナーが出ない。
            handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let info = response.notification().request().content().userInfo();
            let key = NSString::from_str(OPEN_KEY);
            let key: &AnyObject = &key;
            let path = info
                .objectForKey(key)
                .and_then(|v| v.downcast::<NSString>().ok())
                .map(|s| s.to_string());
            if let Some(path) = path {
                dispatch2::DispatchQueue::main().exec_async(move || {
                    crate::app::open_path(std::path::Path::new(&path));
                });
            }
            handler.call(());
        }
    }
);

pub struct Notifier {
    center: Option<Retained<UNUserNotificationCenter>>,
    // センターはデリゲートを弱参照するので保持しておく。
    _delegate: Option<Retained<NotifyDelegate>>,
    seq: u64,
}

impl Notifier {
    pub fn new() -> Notifier {
        // バンドル ID がないと currentNotificationCenter が例外を投げる。
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            meetrec_core::warn!("アプリバンドル外で実行されているため通知は無効です");
            return Notifier { center: None, _delegate: None, seq: 0 };
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate: Retained<NotifyDelegate> =
            unsafe { msg_send![NotifyDelegate::alloc(), init] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let handler = RcBlock::new(|granted: Bool, error: *mut NSError| {
            // SAFETY: error は nil か有効な NSError。
            let err = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
            meetrec_core::info!("通知の許可: {} {}", granted.as_bool(), err.unwrap_or_default());
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &handler,
        );
        Notifier { center: Some(center), _delegate: Some(delegate), seq: 0 }
    }

    pub fn notify(&mut self, notice: &Notice) {
        let Some(center) = &self.center else { return };
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&notice.title));
        content.setBody(&NSString::from_str(&notice.body));
        if let Some(path) = &notice.open {
            let key = NSString::from_str(OPEN_KEY);
            let value = NSString::from_str(&path.to_string_lossy());
            let info = NSDictionary::<NSString, NSString>::from_slices(&[&*key], &[&*value]);
            // SAFETY: 文字列キー・文字列値（plist 型）だけの辞書を型消去して渡す。
            unsafe {
                let erased = &*(Retained::as_ptr(&info) as *const NSDictionary);
                content.setUserInfo(erased);
            }
        }
        self.seq += 1;
        let id = NSString::from_str(&format!("meetrec-{}-{}", std::process::id(), self.seq));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        let handler = RcBlock::new(|error: *mut NSError| {
            // SAFETY: error は nil か有効な NSError。
            if let Some(e) = unsafe { error.as_ref() } {
                meetrec_core::warn!("通知を表示できません: {}", e.localizedDescription());
            }
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&handler));
    }
}
