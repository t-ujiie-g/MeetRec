//! D3D11 Video Processor による BGRA → NV12 変換と縮小（GPU 上で完結）。
//! 出力は固定サイズのキャンバスで、入力はアスペクト比を保って中央に収める（余白は黒）。

use std::mem::ManuallyDrop;

use meetrec_core::session::fit_rect;
use windows::core::{Interface, Result as WinResult};
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};

/// 出力テクスチャの数。エンコーダが入力を保持している間に上書きしないよう循環させる。
/// 未検証: 低遅延モードのエンコーダが 6 フレーム以上入力を保持しないことを前提にしている。
const POOL: usize = 6;

/// 出力の色空間：YCbCr_Matrix=1（BT.709）、Nominal_Range=1（16-235）。
const OUTPUT_COLOR_SPACE: u32 = (1 << 2) | (1 << 4);

pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    black: ID3D11Texture2D,
    canvas: (u32, u32),
}

unsafe fn nv12_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
    initial: Option<&[u8]>,
) -> WinResult<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = initial.map(|bytes| D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr() as *const _,
        SysMemPitch: width,
        SysMemSlicePitch: 0,
    });
    let mut texture = None;
    device.CreateTexture2D(
        &desc,
        data.as_ref().map(|d| d as *const _),
        Some(&mut texture),
    )?;
    texture.ok_or_else(|| E_FAIL.into())
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        canvas: (u32, u32),
        fps: u32,
    ) -> WinResult<Converter> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: fps.max(1),
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: canvas.0,
                InputHeight: canvas.1,
                OutputFrameRate: rate,
                OutputWidth: canvas.0,
                OutputHeight: canvas.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: OUTPUT_COLOR_SPACE,
            };
            video_context.VideoProcessorSetOutputColorSpace(&processor, &space);
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);

            let mut outputs = Vec::with_capacity(POOL);
            for _ in 0..POOL {
                let texture = nv12_texture(device, canvas, None)?;
                let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &view_desc,
                    Some(&mut view),
                )?;
                outputs.push((
                    texture,
                    view.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
                ));
            }

            // 黒一色の NV12（Y=16, CbCr=128）。最初のフレームが来ないとき用。
            let (w, h) = (canvas.0 as usize, canvas.1 as usize);
            let mut pixels = vec![16u8; w * h + w * h / 2];
            pixels[w * h..].fill(128);
            let black = nv12_texture(device, canvas, Some(&pixels))?;

            Ok(Converter {
                video_device,
                video_context,
                enumerator,
                processor,
                outputs,
                next: 0,
                black,
                canvas,
            })
        }
    }

    pub fn black(&self) -> ID3D11Texture2D {
        self.black.clone()
    }

    /// `src`（BGRA）の左上 `content` の範囲を、キャンバスに収めた NV12 テクスチャにして返す。
    pub fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        content: (u32, u32),
    ) -> WinResult<ID3D11Texture2D> {
        unsafe {
            // プール再作成前のフレームではテクスチャが content より小さいことがある。
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut desc);
            let src_w = content.0.min(desc.Width).max(1);
            let src_h = content.1.min(desc.Height).max(1);

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device.CreateVideoProcessorInputView(
                src,
                &self.enumerator,
                &in_desc,
                Some(&mut input),
            )?;
            let input = input.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

            let source = RECT {
                left: 0,
                top: 0,
                right: src_w as i32,
                bottom: src_h as i32,
            };
            let (x, y, w, h) = fit_rect(src_w, src_h, self.canvas.0, self.canvas.1);
            let dest = RECT {
                left: x as i32,
                top: y as i32,
                right: (x + w) as i32,
                bottom: (y + h) as i32,
            };
            let target = RECT {
                left: 0,
                top: 0,
                right: self.canvas.0 as i32,
                bottom: self.canvas.1 as i32,
            };
            let (vc, vp) = (&self.video_context, &self.processor);
            vc.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&source));
            vc.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dest));
            vc.VideoProcessorSetOutputTargetRect(vp, true, Some(&target));

            self.next = (self.next + 1) % self.outputs.len();
            let (texture, view) = &self.outputs[self.next];
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input)),
                ..Default::default()
            };
            let result = vc.VideoProcessorBlt(vp, view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            Ok(texture.clone())
        }
    }
}
