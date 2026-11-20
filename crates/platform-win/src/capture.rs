//! Windows.Graphics.Capture による単一ウィンドウのキャプチャ。
//! フレームは内容が変化したときだけ届く。fps 制限後、上限付きキューで映像スレッドへ渡す。

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use meetrec_core::session::FrameGate;
use windows::core::{factory, Interface, Result as WinResult};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// キューに溜めるフレーム数の上限（あふれたら古い方を捨てる）。
const QUEUE_LIMIT: usize = 2;
/// フレームプールのバッファ数。
const POOL_BUFFERS: i32 = 2;

pub struct CapFrame {
    pub texture: ID3D11Texture2D,
    /// テクスチャの寿命を保つために保持する（処理後に Close してプールへ返す）。
    frame: Direct3D11CaptureFrame,
    pub pts_ns: u64,
    /// テクスチャ内の有効領域（ウィンドウの内容サイズ）。
    pub content: (u32, u32),
}

// D3D11 デバイスはマルチスレッド保護を有効にしており、フレームはスレッド間で受け渡せる。
unsafe impl Send for CapFrame {}

impl Drop for CapFrame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

/// WinRT の IDirect3DDevice はアジャイル（どのスレッドからでも使える）だが、
/// インターフェイス型は Send ではないので包んで渡す。
struct AgileDevice(IDirect3DDevice);

unsafe impl Send for AgileDevice {}

impl AgileDevice {
    fn get(&self) -> &IDirect3DDevice {
        &self.0
    }
}

#[derive(Default)]
pub struct FrameQueue {
    frames: Mutex<VecDeque<CapFrame>>,
    pub ready: Condvar,
}

impl FrameQueue {
    fn push(&self, frame: CapFrame) {
        let mut q = self.frames.lock().unwrap();
        while q.len() >= QUEUE_LIMIT {
            q.pop_front();
        }
        q.push_back(frame);
        drop(q);
        self.ready.notify_all();
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<CapFrame>> {
        self.frames.lock().unwrap()
    }

    pub fn clear(&self) {
        self.frames.lock().unwrap().clear();
    }
}

pub struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    token: i64,
    _item: GraphicsCaptureItem,
}

fn clamp_size(size: SizeInt32) -> SizeInt32 {
    SizeInt32 {
        Width: size.Width.max(16),
        Height: size.Height.max(16),
    }
}

impl Capture {
    pub fn start(
        hwnd: isize,
        device: &IDirect3DDevice,
        show_cursor: bool,
        fps: u32,
        queue: Arc<FrameQueue>,
    ) -> WinResult<Capture> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(E_FAIL.into());
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
        let size = clamp_size(item.Size()?);
        let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
        let pool =
            Direct3D11CaptureFramePool::CreateFreeThreaded(device, format, POOL_BUFFERS, size)?;

        let state = Mutex::new((FrameGate::new(fps), size));
        let dev = AgileDevice(device.clone());
        let token = pool.FrameArrived(&TypedEventHandler::new(
            move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                let Some(pool) = pool.as_ref() else {
                    return Ok(());
                };
                let frame = pool.TryGetNextFrame()?;
                let content = frame.ContentSize()?;
                let mut st = state.lock().unwrap();
                if content.Width != st.1.Width || content.Height != st.1.Height {
                    // ウィンドウサイズが変わった：次のフレームから新しい大きさで受け取る。
                    st.1 = content;
                    let _ = pool.Recreate(dev.get(), format, POOL_BUFFERS, clamp_size(content));
                }
                // SystemRelativeTime は QPC と同じ時計（100ns 単位）。
                let pts_ns = frame.SystemRelativeTime()?.Duration.max(0) as u64 * 100;
                if !st.0.accept(pts_ns) {
                    return frame.Close();
                }
                drop(st);
                let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                queue.push(CapFrame {
                    texture,
                    frame,
                    pts_ns,
                    content: (content.Width.max(1) as u32, content.Height.max(1) as u32),
                });
                Ok(())
            },
        ))?;

        let session = pool.CreateCaptureSession(&item)?;
        // 古い Windows 10 ではこれらのプロパティがない。失敗は無視する。
        let _ = session.SetIsCursorCaptureEnabled(show_cursor);
        // Windows 11：キャプチャ中の黄色い枠を消す。枠なしの許可を（結果を待たずに）要求してから設定する。
        // 未検証: パッケージ化していないデスクトップアプリでは許可ダイアログなしで通る想定。
        let _ = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless);
        let _ = session.SetIsBorderRequired(false);
        // ウィンドウが閉じられた場合は core が次のティックで張り替えを指示する。
        let _ = item.Closed(&TypedEventHandler::new(|_, _| {
            meetrec_core::info!("キャプチャ対象のウィンドウが閉じられました");
            Ok(())
        }));
        session.StartCapture()?;
        Ok(Capture {
            pool,
            session,
            token,
            _item: item,
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
