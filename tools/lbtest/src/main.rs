//! 検証用（MeetRec 本体には入らない）：Windows の音の録り方を、MeetRec と切り離して確かめる。
//! 出す側のストリームのカテゴリごとに、プロセスループバックと出力機器のループバックで録れるかを測り、
//! 出力機器のオーディオセッション・消音・音量を見る／変える。使い方は docs/windows-teams-audio.md。

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::core::{HRESULT, Interface, Ref, Result, implement};

const AMP: f32 = 0.01;

#[derive(Default, Debug, Clone, Copy)]
struct Stat {
    frames: u64,
    silent_frames: u64,
    rms: f64,
    peak: f32,
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rms={:.6} peak={:.6} frames={} silentflag={}",
            self.rms, self.peak, self.frames, self.silent_frames
        )
    }
}

unsafe fn default_render() -> Result<IMMDevice> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        en.GetDefaultAudioEndpoint(eRender, eConsole)
    }
}

/// 既定の出力機器に 440Hz を出す。
unsafe fn render(cat: i32, raw: bool, sessmute: bool, secs: u64) -> Result<()> {
    unsafe {
        let dev = default_render()?;
        let client: IAudioClient2 = dev.Activate(CLSCTX_ALL, None)?;
        let props = AudioClientProperties {
            cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AUDIO_STREAM_CATEGORY(cat),
            Options: if raw {
                AUDCLNT_STREAMOPTIONS_RAW
            } else {
                AUDCLNT_STREAMOPTIONS_NONE
            },
        };
        client.SetClientProperties(&props)?;
        let mix = client.GetMixFormat()?;
        let f = std::ptr::read_unaligned(mix);
        let (rate, ch, bits) = (f.nSamplesPerSec, f.nChannels as usize, f.wBitsPerSample);
        assert_eq!(bits, 32, "float32 のミックス形式だけ対応");
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, mix, None)?;
        CoTaskMemFree(Some(mix as *const _));
        let size = client.GetBufferSize()?;
        let rc: IAudioRenderClient = client.GetService()?;
        let sv: ISimpleAudioVolume = client.GetService()?;
        sv.SetMute(sessmute, std::ptr::null())?;
        client.Start()?;
        let start = Instant::now();
        let mut n: u64 = 0;
        while start.elapsed() < Duration::from_secs(secs) {
            let pad = client.GetCurrentPadding()?;
            let avail = size - pad;
            if avail > 0 {
                let p = rc.GetBuffer(avail)? as *mut f32;
                for i in 0..avail as usize {
                    let v =
                        AMP * (2.0 * std::f32::consts::PI * 440.0 * (n as f32) / rate as f32).sin();
                    n += 1;
                    for c in 0..ch {
                        p.add(i * ch + c).write_unaligned(v);
                    }
                }
                rc.ReleaseBuffer(avail, 0)?;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        client.Stop()?;
        sv.SetMute(false, std::ptr::null())?;
        Ok(())
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateDone(Arc<(Mutex<bool>, Condvar)>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateDone_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
        Ok(())
    }
}

#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

unsafe fn activate_loopback(pid: u32, exclude: bool) -> Result<IAudioClient> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: if exclude {
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    } else {
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    },
                },
            },
        };
        let variant = BlobVariant {
            vt: 65,
            reserved: [0; 3],
            size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            data: &params as *const _ as *const u8,
        };
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let handler: IActivateAudioInterfaceCompletionHandler = ActivateDone(done.clone()).into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant as *const BlobVariant as *const PROPVARIANT),
            &handler,
        )?;
        let mut g = done.0.lock().unwrap();
        while !*g {
            let (ng, t) = done.1.wait_timeout(g, Duration::from_secs(5)).unwrap();
            g = ng;
            if t.timed_out() {
                return Err(E_FAIL.into());
            }
        }
        drop(g);
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        unk.ok_or_else(|| windows::core::Error::from(E_FAIL))?
            .cast()
    }
}

/// 開始前のクライアントから ms ミリ秒ぶん読んで音量を測る。float なら true。
unsafe fn measure(client: &IAudioClient, float: bool, ch: usize, ms: u64) -> Result<Stat> {
    unsafe {
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        let mut st = Stat::default();
        let mut sum = 0f64;
        let mut count = 0u64;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            std::thread::sleep(Duration::from_millis(10));
            while cap.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                st.frames += frames as u64;
                if flags & 2 != 0 {
                    st.silent_frames += frames as u64;
                } else {
                    for i in 0..frames as usize * ch {
                        let v = if float {
                            (data as *const f32).add(i).read_unaligned()
                        } else {
                            (data as *const i16).add(i).read_unaligned() as f32 / 32768.0
                        };
                        sum += (v as f64) * (v as f64);
                        count += 1;
                        st.peak = st.peak.max(v.abs());
                    }
                }
                cap.ReleaseBuffer(frames)?;
            }
        }
        client.Stop()?;
        if count > 0 {
            st.rms = (sum / count as f64).sqrt();
        }
        Ok(st)
    }
}

unsafe fn cap_process(pid: u32, exclude: bool, ms: u64) -> Result<Stat> {
    unsafe {
        let wave = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let client = activate_loopback(pid, exclude)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            2_000_000,
            0,
            &wave,
            None,
        )?;
        measure(&client, false, 2, ms)
    }
}

unsafe fn cap_endpoint(ms: u64) -> Result<Stat> {
    unsafe {
        let dev = default_render()?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let f = std::ptr::read_unaligned(mix);
        let ch = f.nChannels as usize;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            2_000_000,
            0,
            mix,
            None,
        )?;
        CoTaskMemFree(Some(mix as *const _));
        measure(&client, true, ch, ms)
    }
}

fn show(label: &str, r: Result<Stat>) {
    match r {
        Ok(s) => println!("    {label:<28} {s}"),
        Err(e) => println!("    {label:<28} ERROR {e}"),
    }
}

/// 子プロセスで音を出し、3 通りの方式で測る。
unsafe fn scenario(name: &str, cat: i32, raw: bool, sessmute: bool, full: bool) {
    unsafe {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args([
                "render",
                &cat.to_string(),
                &(raw as u8).to_string(),
                &(sessmute as u8).to_string(),
                "8",
            ])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(800));
        println!("[{name}] cat={cat} raw={raw} sessmute={sessmute}");
        show(
            "process include(child)",
            cap_process(child.id(), false, 1000),
        );
        if full {
            // 関係のないプロセス（何もしない子）を除外対象にする＝それ以外すべてを録る。
            let mut idle = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["sleep"])
                .spawn()
                .unwrap();
            show(
                "process exclude(unrelated)",
                cap_process(idle.id(), true, 1000),
            );
            let _ = idle.kill();
            let _ = idle.wait();
        }
        show("endpoint loopback", cap_endpoint(1000));
        let _ = child.kill();
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(300));
    }
}

unsafe fn matrix(which: &str) -> Result<()> {
    unsafe {
        let dev = default_render()?;
        let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
        let mute0 = vol.GetMute()?;
        let level0 = vol.GetMasterVolumeLevelScalar()?;
        // 1=音量 2=消音 4=メーター を機器（ハードウェア）側で持つか。
        println!(
            "endpoint: mute={} volume={level0:.3} hardware_support={:#x}",
            mute0.as_bool(),
            vol.QueryHardwareSupport()?
        );
        let other = AudioCategory_Other.0;
        let comm = AudioCategory_Communications.0;
        if which == "base" || which == "all" {
            scenario("Other", other, false, false, true);
            scenario("Communications", comm, false, false, true);
            scenario("Other raw", other, true, false, true);
            scenario("Communications raw", comm, true, false, true);
        }
        if which == "cats" || which == "all" {
            for (n, c) in [
                ("Alerts", 4),
                ("SoundEffects", 5),
                ("GameEffects", 6),
                ("GameMedia", 7),
                ("GameChat", 8),
                ("Speech", 9),
                ("Movie", 10),
                ("Media", 11),
                ("FarFieldSpeech", 12),
                ("UniformSpeech", 13),
                ("VoiceTyping", 14),
            ] {
                scenario(n, c, false, false, false);
            }
        }
        if which == "mute" || which == "all" {
            println!("== 出力機器を消音 ==");
            vol.SetMute(true, std::ptr::null())?;
            scenario("Other / endpoint muted", other, false, false, false);
            scenario("Communications / endpoint muted", comm, false, false, false);
            vol.SetMute(mute0.as_bool(), std::ptr::null())?;
            println!("== 出力機器の音量 1%（消音ではない） ==");
            vol.SetMute(false, std::ptr::null())?;
            vol.SetMasterVolumeLevelScalar(0.01, std::ptr::null())?;
            scenario("Other / volume 1%", other, false, false, false);
            scenario("Communications / volume 1%", comm, false, false, false);
            println!("== 出力機器の音量 0%（消音ではない） ==");
            vol.SetMasterVolumeLevelScalar(0.0, std::ptr::null())?;
            scenario("Communications / volume 0%", comm, false, false, false);
            vol.SetMasterVolumeLevelScalar(level0, std::ptr::null())?;
            vol.SetMute(mute0.as_bool(), std::ptr::null())?;
            println!("== セッションだけ消音（音量ミキサーでアプリを消すのに相当） ==");
            scenario("Other / session muted", other, false, true, false);
            scenario("Communications / session muted", comm, false, true, false);
        }
        println!(
            "restored: mute={} volume={:.3}",
            vol.GetMute()?.as_bool(),
            vol.GetMasterVolumeLevelScalar()?
        );
        Ok(())
    }
}

/// 出力機器ごとのオーディオセッション（PID・状態・セッションの消音・1 秒間の最大ピーク）。
/// `set_mute` が Some((pid, m)) なら、その PID のセッションの消音を m にする。
unsafe fn sessions(set_mute: Option<(u32, bool)>) -> Result<()> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default_id = en
            .GetDefaultAudioEndpoint(eRender, eConsole)?
            .GetId()?
            .to_string()?;
        let comm_id = en
            .GetDefaultAudioEndpoint(eRender, eCommunications)?
            .GetId()?
            .to_string()?;
        let devices = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devices.GetCount()? {
            let dev = devices.Item(i)?;
            let id = dev.GetId()?.to_string()?;
            let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
            println!(
                "device[{i}] default={} communications={} mute={} volume={:.3} hardware_support={:#x}",
                id == default_id,
                id == comm_id,
                vol.GetMute()?.as_bool(),
                vol.GetMasterVolumeLevelScalar()?,
                vol.QueryHardwareSupport()?
            );
            let (mut min, mut max, mut step) = (0f32, 0f32, 0f32);
            vol.GetVolumeRange(&mut min, &mut max, &mut step)?;
            println!(
                "    range={min}dB..{max}dB step={step}dB now={}dB",
                vol.GetMasterVolumeLevel()?
            );
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None)?;
            let list = mgr.GetSessionEnumerator()?;
            for n in 0..list.GetCount()? {
                let c = list.GetSession(n)?;
                let c2: IAudioSessionControl2 = c.cast()?;
                let pid = c2.GetProcessId().unwrap_or(0);
                let sv: ISimpleAudioVolume = c.cast()?;
                if let Some((p, m)) = set_mute
                    && p == pid
                {
                    sv.SetMute(m, std::ptr::null())?;
                }
                let meter: IAudioMeterInformation = c.cast()?;
                let mut peak = 0f32;
                if set_mute.is_none() {
                    for _ in 0..20 {
                        peak = peak.max(meter.GetPeakValue().unwrap_or(0.0));
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                println!(
                    "    pid={pid} state={} sessmute={} sessvol={:.2} system={} peak={peak:.4}",
                    c.GetState()?.0,
                    sv.GetMute()?.as_bool(),
                    sv.GetMasterVolume()?,
                    c2.IsSystemSoundsSession().0 == 0
                );
            }
        }
        Ok(())
    }
}

unsafe fn endpoint_set(mute: Option<bool>, level: Option<f32>) -> Result<()> {
    unsafe {
        let vol: IAudioEndpointVolume = default_render()?.Activate(CLSCTX_ALL, None)?;
        if let Some(m) = mute {
            vol.SetMute(m, std::ptr::null())?;
        }
        if let Some(l) = level {
            vol.SetMasterVolumeLevelScalar(l, std::ptr::null())?;
        }
        println!(
            "endpoint: mute={} volume={:.3}",
            vol.GetMute()?.as_bool(),
            vol.GetMasterVolumeLevelScalar()?
        );
        Ok(())
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
        let r = match a.get(1).map(String::as_str) {
            Some("render") => render(
                a[2].parse().unwrap(),
                a[3] == "1",
                a[4] == "1",
                a[5].parse().unwrap(),
            ),
            // watch <秒>：既定の出力機器の消音・音量が変わるたびに、開始からの秒数つきで出す。
            Some("watch") => (|| {
                let vol: IAudioEndpointVolume = default_render()?.Activate(CLSCTX_ALL, None)?;
                let start = Instant::now();
                let mut last = (false, -1f32);
                while start.elapsed() < Duration::from_secs(a[2].parse().unwrap()) {
                    let now = (vol.GetMute()?.as_bool(), vol.GetMasterVolumeLevelScalar()?);
                    if now != last {
                        let t = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis();
                        println!(
                            "utc_ms={t} +{:.1}s mute={} volume={:.3} ({}dB)",
                            start.elapsed().as_secs_f32(),
                            now.0,
                            now.1,
                            vol.GetMasterVolumeLevel()?
                        );
                        last = now;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(())
            })(),
            Some("sessions") => sessions(None),
            // sessmute <pid> <0|1>
            Some("sessmute") => sessions(Some((a[2].parse().unwrap(), a[3] == "1"))),
            // epmute <0|1> / epvol <0.0-1.0>
            Some("epmute") => endpoint_set(Some(a[2] == "1"), None),
            Some("epvol") => endpoint_set(None, Some(a[2].parse().unwrap())),
            Some("sleep") => {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            }
            Some("matrix") => matrix(a.get(2).map_or("all", String::as_str)),
            // cap <include|exclude> <pid> <ms>
            Some("cap") => cap_process(
                a[3].parse().unwrap(),
                a[2] == "exclude",
                a[4].parse().unwrap(),
            )
            .map(|s| println!("process {} pid={} {s}", a[2], a[3])),
            Some("endpoint") => {
                cap_endpoint(a[2].parse().unwrap()).map(|s| println!("endpoint {s}"))
            }
            _ => {
                eprintln!(
                    "usage: lbtest matrix [base|cats|mute|all] | cap <include|exclude> <pid> <ms> | endpoint <ms> \
                     | sessions | sessmute <pid> <0|1> | epmute <0|1> | epvol <0-1> | watch <sec>"
                );
                Ok(())
            }
        };
        if let Err(e) = r {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
