//! Windows 视频背景：Media Engine 硬件解码，画到 GPUI 合成目标正下方的 DirectComposition 视觉树。
//!
//! GPUI 占用窗口的 topmost 合成目标并透明清屏；这里占用下层目标，视频从卡片下透出，
//! 与 macOS AVPlayerLayer 方案相同（合成公式见 [`super::super::underlay_composition`]）。
//! 解码与颜色转换都在 GPU 上，帧直接进交换链，不经过 CPU 拷贝和 GPUI 纹理上传。
//! D3D11、DirectComposition 与 Media Engine 对象只在专用工作线程上创建、使用和释放；
//! UI 线程只发送几何与播放状态。下层目标被占用（Acrylic 材质）或解码失败时报告失败，
//! 调用方回退到场景内播放。
use anyhow::{Context as _, Result};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HMODULE, HWND, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
        Media::MediaFoundation::*,
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
    },
    core::{BSTR, Interface, implement},
};

/// 播放时轮询新帧的间隔；std 在 Windows 上用高精度计时器，远小于 60 fps 帧距。
const FRAME_POLL: Duration = Duration::from_millis(3);
/// 暂停时只等命令。
const IDLE_POLL: Duration = Duration::from_millis(250);

/// 一个窗口内视频层的几何，单位为窗口客户区物理像素。
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Geometry {
    /// 裁剪区（卡片或整窗）的左上角与大小。
    pub clip: [f32; 4],
    pub radius: f32,
    /// 视频相对裁剪区的偏移与相对原生尺寸的缩放；原生尺寸未知时为 `None`。
    pub video: Option<[f32; 4]>,
    pub opacity: f32,
}

enum Command {
    Place(isize, Geometry),
    Remove(isize),
    Play(bool),
}

#[derive(Default)]
struct Status {
    ready: AtomicBool,
    failed: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
}

/// UI 线程持有的句柄。释放即断开命令通道，工作线程随之释放全部 COM 对象并退出。
pub(super) struct Source {
    commands: mpsc::Sender<Command>,
    status: Arc<Status>,
    placed: HashMap<isize, Geometry>,
    playing: bool,
}

impl Source {
    pub(super) fn open(path: PathBuf) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let status = Arc::new(Status::default());
        let worker_status = status.clone();
        thread::Builder::new().name("pebrel-video-underlay".into()).spawn(move || {
            let result = run(&path, &receiver, &worker_status);
            crate::logging::debug_log(format!("video underlay worker exited: {result:?}"));
            if let Err(error) = result {
                log::warn!("video background underlay failed: {error:#}");
                worker_status.failed.store(true, Ordering::Release);
            }
        })?;
        Ok(Self { commands, status, placed: HashMap::new(), playing: false })
    }

    /// 第一帧已呈现到交换链。
    pub(super) fn ready(&self) -> bool {
        self.status.ready.load(Ordering::Acquire) && !self.failed()
    }

    pub(super) fn failed(&self) -> bool {
        self.status.failed.load(Ordering::Acquire)
    }

    pub(super) fn native_size(&self) -> Option<(f32, f32)> {
        let width = self.status.width.load(Ordering::Acquire);
        let height = self.status.height.load(Ordering::Acquire);
        (width > 0 && height > 0).then_some((width as f32, height as f32))
    }

    pub(super) fn windows(&self) -> impl Iterator<Item = isize> + '_ {
        self.placed.keys().copied()
    }

    pub(super) fn place(&mut self, hwnd: isize, geometry: Geometry) {
        if self.placed.get(&hwnd) != Some(&geometry) {
            self.placed.insert(hwnd, geometry);
            let _ = self.commands.send(Command::Place(hwnd, geometry));
        }
    }

    pub(super) fn remove(&mut self, hwnd: isize) {
        if self.placed.remove(&hwnd).is_some() {
            let _ = self.commands.send(Command::Remove(hwnd));
        }
    }

    pub(super) fn set_playing(&mut self, playing: bool) {
        if self.playing != playing {
            self.playing = playing;
            let _ = self.commands.send(Command::Play(playing));
        }
    }
}

/// 逆序释放：MF 与 COM 公寓最后关闭。
struct Apartment {
    media_foundation: bool,
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            if self.media_foundation {
                let _ = MFShutdown();
            }
            CoUninitialize();
        }
    }
}

fn run(path: &std::path::Path, receiver: &mpsc::Receiver<Command>, status: &Status) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let mut apartment = Apartment { media_foundation: false };
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    apartment.media_foundation = true;

    let device = create_device()?;
    let dxgi: IDXGIDevice = device.cast()?;
    let composition: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi)? };
    let engine = create_engine(&device)?;
    unsafe { engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))? };
    crate::logging::debug_log(format!("video underlay engine created for {}", path.display()));

    let result = (|| -> Result<()> {
        let mut placements: HashMap<isize, Placement> = HashMap::new();
        let mut chain: Option<(IDXGISwapChain1, RECT)> = None;
        let mut playing = false;
        let mut last_frame = None;
        loop {
            let mut dirty = false;
            let mut next = if playing {
                match receiver.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            } else {
                match receiver.recv_timeout(IDLE_POLL) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            };
            while let Some(command) = next {
                match command {
                    Command::Place(hwnd, geometry) => {
                        if !placements.contains_key(&hwnd) {
                            crate::logging::debug_log(format!("video underlay place {hwnd:#x} {geometry:?}"));
                            let placement = Placement::new(&composition, hwnd)?;
                            if let Some((swap_chain, _)) = &chain {
                                placement.attach(swap_chain).context("attaching the swap chain")?;
                            }
                            placements.insert(hwnd, placement);
                        }
                        placements[&hwnd].arrange(geometry).context("arranging the video layer")?;
                        dirty = true;
                    },
                    Command::Remove(hwnd) => dirty |= placements.remove(&hwnd).is_some(),
                    Command::Play(play) => {
                        crate::logging::debug_log(format!("video underlay play={play}"));
                        playing = play;
                        unsafe {
                            if play { engine.Play() } else { engine.Pause() }
                                .context("changing the playback state")?;
                        }
                    },
                }
                next = receiver.try_recv().ok();
            }
            if let Ok(error) = unsafe { engine.GetError() } {
                let (code, extended) = unsafe { (error.GetErrorCode(), error.GetExtendedErrorCode()) };
                anyhow::bail!("media engine error {code}: {extended:?}");
            }
            if chain.is_none() && unsafe { engine.HasVideo() }.as_bool() {
                let (mut width, mut height) = (0, 0);
                unsafe { engine.GetNativeVideoSize(Some(&mut width), Some(&mut height)) }
                    .context("reading the native video size")?;
                if width > 0 && height > 0 {
                    let swap_chain = create_swap_chain(&device, width, height)
                        .context("creating the composition swap chain")?;
                    for placement in placements.values() {
                        placement.attach(&swap_chain).context("attaching the swap chain")?;
                    }
                    crate::logging::debug_log(format!("video underlay swap chain {width}x{height}"));
                    status.width.store(width, Ordering::Release);
                    status.height.store(height, Ordering::Release);
                    chain = Some((
                        swap_chain,
                        RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 },
                    ));
                    dirty = true;
                }
            }
            if playing
                && let Some((swap_chain, rect)) = &chain
                && let Ok(time) = unsafe { engine.OnVideoStreamTick() }
                && last_frame != Some(time)
            {
                last_frame = Some(time);
                let target: ID3D11Texture2D =
                    unsafe { swap_chain.GetBuffer(0) }.context("getting the back buffer")?;
                unsafe { engine.TransferVideoFrame(&target, None, rect, None) }
                    .context("transferring the video frame")?;
                unsafe { swap_chain.Present(0, DXGI_PRESENT(0)) }.ok().context("presenting")?;
                if !status.ready.swap(true, Ordering::AcqRel) {
                    crate::logging::debug_log("video underlay first frame presented");
                }
            }
            if dirty {
                unsafe { composition.Commit() }.context("committing the composition")?;
            }
            if playing {
                thread::sleep(FRAME_POLL);
            }
        }
    })();
    unsafe {
        let _ = engine.Shutdown();
    }
    result
}

fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.context("D3D11 returned no device")?;
    // Media Engine 在自己的线程上使用同一设备解码。
    let multithread: ID3D11Multithread = device.cast()?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok(device)
}

#[implement(IMFMediaEngineNotify)]
struct Notify;

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    // 状态由工作线程轮询；回调只为满足 Media Engine 的必需属性。
    fn EventNotify(&self, _: u32, _: usize, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
}

fn create_engine(device: &ID3D11Device) -> Result<IMFMediaEngine> {
    let mut token = 0;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
    let manager = manager.context("Media Foundation returned no DXGI device manager")?;
    unsafe { manager.ResetDevice(device, token)? };
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 3)? };
    let attributes = attributes.context("Media Foundation returned no attributes")?;
    let notify: IMFMediaEngineNotify = Notify.into();
    unsafe {
        attributes.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager)?;
        attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
        attributes
            .SetUINT32(&MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32)?;
    }
    let factory: IMFMediaEngineClassFactory =
        unsafe { CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)? };
    let engine = unsafe { factory.CreateInstance(0, &attributes)? };
    unsafe {
        engine.SetMuted(true)?;
        engine.SetLoop(true)?;
        engine.SetAutoPlay(false)?;
    }
    Ok(engine)
}

/// 交换链保持视频原生尺寸，缩放交给合成器，窗口尺寸变化无需重建。
fn create_swap_chain(device: &ID3D11Device, width: u32, height: u32) -> Result<IDXGISwapChain1> {
    let factory: IDXGIFactory2 = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()? };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        // 合成交换链不接受 IGNORE；视频帧本身不透明。
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    Ok(unsafe { factory.CreateSwapChainForComposition(device, &desc, None)? })
}

/// 每个窗口一棵树：根视觉承担卡片圆角裁剪与不透明度，子视觉承载按布局缩放的视频。
struct Placement {
    _target: IDCompositionTarget,
    root: IDCompositionVisual,
    video: IDCompositionVisual,
    clip: IDCompositionRectangleClip,
    effect: IDCompositionEffectGroup,
    scale: IDCompositionScaleTransform,
}

impl Placement {
    fn new(composition: &IDCompositionDevice, hwnd: isize) -> Result<Self> {
        let target = unsafe { composition.CreateTargetForHwnd(HWND(hwnd as _), false) }
            .context("the lower composition target of this window is already in use")?;
        unsafe {
            let root = composition.CreateVisual()?;
            let video = composition.CreateVisual()?;
            let clip = composition.CreateRectangleClip()?;
            let effect = composition.CreateEffectGroup()?;
            let scale = composition.CreateScaleTransform()?;
            effect.SetOpacity2(0.0)?;
            root.SetClip(&clip)?;
            root.SetEffect(&effect)?;
            video.SetTransform(&scale)?;
            root.AddVisual(&video, false, None)?;
            target.SetRoot(&root)?;
            Ok(Self { _target: target, root, video, clip, effect, scale })
        }
    }

    fn attach(&self, swap_chain: &IDXGISwapChain1) -> Result<()> {
        unsafe { self.video.SetContent(swap_chain)? };
        Ok(())
    }

    fn arrange(&self, geometry: Geometry) -> Result<()> {
        let [x, y, width, height] = geometry.clip;
        let radius = geometry.radius;
        unsafe {
            self.root.SetOffsetX2(x)?;
            self.root.SetOffsetY2(y)?;
            self.clip.SetLeft2(0.0)?;
            self.clip.SetTop2(0.0)?;
            self.clip.SetRight2(width)?;
            self.clip.SetBottom2(height)?;
            self.clip.SetTopLeftRadiusX2(radius)?;
            self.clip.SetTopLeftRadiusY2(radius)?;
            self.clip.SetTopRightRadiusX2(radius)?;
            self.clip.SetTopRightRadiusY2(radius)?;
            self.clip.SetBottomLeftRadiusX2(radius)?;
            self.clip.SetBottomLeftRadiusY2(radius)?;
            self.clip.SetBottomRightRadiusX2(radius)?;
            self.clip.SetBottomRightRadiusY2(radius)?;
            match geometry.video {
                Some([offset_x, offset_y, scale_x, scale_y]) => {
                    self.video.SetOffsetX2(offset_x)?;
                    self.video.SetOffsetY2(offset_y)?;
                    self.scale.SetScaleX2(scale_x)?;
                    self.scale.SetScaleY2(scale_y)?;
                    self.effect.SetOpacity2(geometry.opacity)?;
                },
                None => self.effect.SetOpacity2(0.0)?,
            }
        }
        Ok(())
    }
}
