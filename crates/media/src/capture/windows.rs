//! Windows capture (design doc §11, §5.1, §18; ADR 0012).
//!
//! DXGI Desktop Duplication (`IDXGIOutputDuplication`), not
//! Windows.Graphics.Capture and not the `scap` crate. Desktop Duplication is
//! the one Windows capture API whose shape already matches the
//! [`ScreenCapturer`] contract instead of having to be bent into it:
//!
//! - `AcquireNextFrame` is a synchronous poll with a timeout, so
//!   [`ScreenCapturer::next_frame`] needs no `WinRT` dispatcher, no callback
//!   thread and no frame-pool bookkeeping.
//! - `DXGI_ERROR_ACCESS_LOST` is documented as the error for a desktop
//!   switch, a session lock or a mode change, which is §18's
//!   [`MediaError::CaptureInterrupted`] verbatim. Nothing has to infer an
//!   interruption from a timeout or a silent stream.
//! - Adapter/output enumeration is native to DXGI, so
//!   [`CaptureTarget::Display`] is a real monitor index rather than a
//!   platform-specific handle, and the primary display is the output whose
//!   desktop rectangle starts at the virtual-screen origin.
//!
//! What Desktop Duplication does *not* give away for free is §11.1's "return
//! `None` when the frame is identical to the previous one". Neither
//! `LastPresentTime` nor the dirty-rect metadata means "the pixels changed" —
//! both only mean "something was repainted", and on a real desktop that is
//! routinely true of a screen that ends up byte-identical. So this backend
//! takes the cheap OS signal as a first filter and then hashes the frame the
//! way the X11 backend does, which is the only thing that answers the actual
//! question (ADR 0012).
//!
//! Frames arrive as `DXGI_FORMAT_B8G8R8A8_UNORM` GPU textures, which is
//! [`PixelFormat::Bgra8`] — the same format the X11 backend produces and the
//! one `encode::windows`/`encode::software` already consume, so nothing on the
//! encoder side changes.
//!
//! Like `encode::software`, the implementation is kept as an inline module so
//! the file list of §6 stays exact. It is gated on the `capture-windows`
//! feature rather than on `target_os` alone: `cargo build --workspace` keeps
//! building the stub below and pulls in none of the Direct3D 11/DXGI bindings
//! (ADR 0012).

#[cfg(feature = "capture-windows")]
pub use dxgi::WindowsCapturer;
#[cfg(not(feature = "capture-windows"))]
pub use stub::WindowsCapturer;

/// DXGI Desktop Duplication capture (§11, §18; ADR 0012).
///
/// The fourth and last place in the crate that needs `unsafe`, after
/// `decode::shm` (ADR 0005), `decode::windows_sandbox` (ADR 0007) and
/// `encode::windows` (ADR 0011): every `IDXGIOutputDuplication`/`ID3D11Device`
/// call in the `windows` crate's Direct3D 11 and DXGI bindings is `unsafe fn`
/// because it crosses into COM. Each `unsafe` block in this module carries a
/// `SAFETY:` note, as §21 requires.
#[cfg(feature = "capture-windows")]
#[allow(
    unsafe_code,
    reason = "DXGI Desktop Duplication is COM; every IDXGIOutputDuplication/ID3D11Device call in the `windows` crate is `unsafe fn`. See ADR 0012."
)]
mod dxgi {
    use std::time::Instant;

    use lumepeer_core::constants::ENCODE_DEFAULT_FPS;
    use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, HMODULE};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND,
        DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
        IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    use windows::core::Interface as _;

    use crate::capture::{CaptureTarget, Frame, InputCapability, PixelFormat, ScreenCapturer};
    use crate::error::{MediaError, Result};

    /// Bytes per pixel of `DXGI_FORMAT_B8G8R8A8_UNORM`, the only format
    /// Desktop Duplication hands back for the desktop image.
    const BYTES_PER_PIXEL: usize = 4;

    /// Milliseconds in a second, for turning [`ENCODE_DEFAULT_FPS`] into the
    /// `AcquireNextFrame` timeout.
    const MILLIS_PER_SEC: u32 = 1_000;

    /// How long one [`ScreenCapturer::next_frame`] waits for the compositor to
    /// present something new before reporting "nothing changed".
    ///
    /// One frame interval at §14's [`ENCODE_DEFAULT_FPS`]: long enough that a
    /// caller polling at the encoder's own rate does not spin, short enough
    /// that a `stop()` from a revoke is never more than a frame away. Unlike
    /// X11's `GetImage`, Desktop Duplication has a frame clock of its own, so
    /// this is a real wait rather than a poll interval.
    fn acquire_timeout_ms() -> u32 {
        MILLIS_PER_SEC / u32::from(ENCODE_DEFAULT_FPS.max(1))
    }

    /// One attached monitor, with the adapter that drives it.
    ///
    /// The device handed to `DuplicateOutput` has to live on the same adapter
    /// as the output, so the two are kept together rather than looked up
    /// separately.
    struct Monitor {
        adapter: IDXGIAdapter1,
        output: IDXGIOutput,
        desc: DXGI_OUTPUT_DESC,
    }

    impl Monitor {
        /// Whether this is the OS's primary display.
        ///
        /// In virtual-screen coordinates the primary monitor's upper-left
        /// corner is always the origin (MSDN, "Multiple Display Monitors"), so
        /// this needs no `GetMonitorInfoW` round trip and no
        /// `Win32_UI_WindowsAndMessaging` bindings.
        fn is_primary(&self) -> bool {
            self.desc.DesktopCoordinates.left == 0 && self.desc.DesktopCoordinates.top == 0
        }

        fn width(&self) -> i32 {
            self.desc.DesktopCoordinates.right - self.desc.DesktopCoordinates.left
        }

        fn height(&self) -> i32 {
            self.desc.DesktopCoordinates.bottom - self.desc.DesktopCoordinates.top
        }
    }

    // The COM state is not printable and must never be logged; only the
    // geometry matters for logs, exactly as `encode::windows` treats the
    // transform.
    impl std::fmt::Debug for Monitor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Monitor")
                .field("primary", &self.is_primary())
                .field("width", &self.width())
                .field("height", &self.height())
                .finish_non_exhaustive()
        }
    }

    /// Every monitor currently attached to the desktop, in DXGI enumeration
    /// order: adapter 0's outputs first, then adapter 1's, and so on. That
    /// order is what [`CaptureTarget::Display`] indexes into.
    ///
    /// Outputs that are not attached to the desktop are skipped rather than
    /// counted: they cannot be duplicated, so leaving them in would make
    /// `Display(n)` name a display the host cannot see.
    fn attached_monitors() -> Result<Vec<Monitor>> {
        // SAFETY: CreateDXGIFactory1 is a plain dxgi.dll export that returns
        // an owned interface pointer of the requested type on success. It
        // needs no COM apartment of its own, which is why this module - unlike
        // `encode::windows`, which drives Media Foundation - never calls
        // CoInitializeEx and so cannot disturb the apartment of a caller that
        // already joined one.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| {
            MediaError::CaptureUnavailable(format!("CreateDXGIFactory1 failed: {e}"))
        })?;

        let mut monitors = Vec::new();
        for adapter_index in 0.. {
            // SAFETY: EnumAdapters1 either yields an owned IDXGIAdapter1 or
            // reports DXGI_ERROR_NOT_FOUND once the index runs past the last
            // adapter; it reads nothing this side owns.
            let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => {
                    return Err(MediaError::CaptureUnavailable(format!(
                        "enumerating display adapters failed: {e}"
                    )));
                }
            };

            for output_index in 0.. {
                // SAFETY: as EnumAdapters1 above, for this adapter's outputs.
                let output = match unsafe { adapter.EnumOutputs(output_index) } {
                    Ok(output) => output,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => {
                        return Err(MediaError::CaptureUnavailable(format!(
                            "enumerating displays failed: {e}"
                        )));
                    }
                };
                // SAFETY: GetDesc writes the output's description into a
                // plain `#[repr(C)]` struct it returns by value.
                let desc = unsafe { output.GetDesc() }.map_err(|e| {
                    MediaError::CaptureUnavailable(format!(
                        "reading a display's geometry failed: {e}"
                    ))
                })?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                monitors.push(Monitor {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }
        Ok(monitors)
    }

    /// Picks the monitor `target` names out of `monitors`.
    fn select(monitors: Vec<Monitor>, target: CaptureTarget) -> Result<Monitor> {
        let index = match target {
            // Falling back to the first attached monitor keeps a host whose
            // desktop origin is not where Windows says it is capturing
            // something real instead of refusing the session outright.
            CaptureTarget::PrimaryDisplay => {
                monitors.iter().position(Monitor::is_primary).unwrap_or(0)
            }
            CaptureTarget::Display(n) => usize::try_from(n).map_err(|_| {
                MediaError::CaptureUnavailable(format!("display index {n} is out of range"))
            })?,
        };
        let count = monitors.len();
        monitors.into_iter().nth(index).ok_or_else(|| {
            MediaError::CaptureUnavailable(format!(
                "no display {index}: this host has {count} attached display(s)"
            ))
        })
    }

    /// CPU-readable copy target, kept across frames so a steady-state capture
    /// allocates nothing on the GPU per frame.
    struct Staging {
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
    }

    /// A live duplication of one monitor.
    struct Active {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
        staging: Staging,
        /// Hash of the last frame handed out, so a screen that was repainted
        /// without actually changing yields `None` instead of a duplicate
        /// (§11.1), exactly as the X11 backend does it.
        last_hash: Option<[u8; 32]>,
        started_at: Instant,
    }

    // As `Monitor` above: COM state is not printable and must never be logged.
    impl std::fmt::Debug for Active {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Active")
                .field("width", &self.staging.width)
                .field("height", &self.staging.height)
                .finish_non_exhaustive()
        }
    }

    impl Active {
        /// Opens a duplication of the monitor `target` names.
        fn open(target: CaptureTarget) -> Result<Self> {
            let monitor = select(attached_monitors()?, target)?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            // SAFETY: D3D11CreateDevice is a plain d3d11.dll export. The
            // adapter reference is only borrowed for the call;
            // D3D_DRIVER_TYPE_UNKNOWN is the driver type MSDN requires when an
            // adapter is supplied, and the two out-parameters are locals that
            // outlive the call and receive owned interface pointers on
            // success. A null HMODULE means "no software rasterizer", which is
            // what D3D_DRIVER_TYPE_UNKNOWN wants.
            unsafe {
                D3D11CreateDevice(
                    &monitor.adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE(std::ptr::null_mut()),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&raw mut device),
                    None,
                    Some(&raw mut context),
                )
            }
            .map_err(|e| {
                MediaError::CaptureUnavailable(format!(
                    "no Direct3D 11 device on this display's adapter: {e}"
                ))
            })?;
            let (Some(device), Some(context)) = (device, context) else {
                return Err(MediaError::CaptureUnavailable(
                    "Direct3D reported success with no device".to_owned(),
                ));
            };

            // SAFETY: QueryInterface for the IDXGIOutput1 the duplication API
            // lives on; `output` is an interface this function owns.
            let output1 = monitor.output.cast::<IDXGIOutput1>().map_err(|e| {
                MediaError::CaptureUnavailable(format!(
                    "this display has no IDXGIOutput1 (Windows 8 or newer is required): {e}"
                ))
            })?;

            // SAFETY: DuplicateOutput borrows the device for the call and
            // returns an owned IDXGIOutputDuplication on success.
            let duplication =
                unsafe { output1.DuplicateOutput(&device) }.map_err(|e| map_start_error(&e))?;

            // SAFETY: GetDesc returns the duplication's description by value
            // and reads nothing this side owns.
            let mode = unsafe { duplication.GetDesc() }.ModeDesc;
            // The mode size is only a starting point: a rotated desktop and a
            // resolution change both show up in the acquired texture's own
            // description, which `frame_from` re-reads every frame and which
            // re-creates this staging texture whenever it disagrees.
            let staging = Staging::new(&device, mode.Width.max(1), mode.Height.max(1))?;

            Ok(Self {
                device,
                context,
                duplication,
                staging,
                last_hash: None,
                started_at: Instant::now(),
            })
        }

        /// One `AcquireNextFrame`/`ReleaseFrame` cycle.
        fn next_frame(&mut self) -> Result<Option<Frame>> {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: both out-parameters are locals that outlive the call.
            // On success the duplication owes this side exactly one
            // ReleaseFrame, which every path below pays before returning.
            match unsafe {
                self.duplication.AcquireNextFrame(
                    acquire_timeout_ms(),
                    &raw mut info,
                    &raw mut resource,
                )
            } {
                Ok(()) => {}
                // Nothing was presented within the timeout: §11.1's "identical
                // to the previous one", straight from the compositor.
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(map_runtime_error(&e)),
            }

            let outcome = self.frame_from(resource.as_ref(), &info);

            // SAFETY: balances the AcquireNextFrame above. It has to run on
            // the failure paths too: an unreleased frame makes every later
            // AcquireNextFrame fail with DXGI_ERROR_INVALID_CALL, turning one
            // bad frame into a permanently dead capture.
            let released = unsafe { self.duplication.ReleaseFrame() };

            let frame = outcome?;
            released.map_err(|e| map_runtime_error(&e))?;
            Ok(frame)
        }

        /// Turns an acquired desktop surface into an owned, tightly packed
        /// BGRA8 [`Frame`], or `None` when the desktop image did not change.
        fn frame_from(
            &mut self,
            resource: Option<&IDXGIResource>,
            info: &DXGI_OUTDUPL_FRAME_INFO,
        ) -> Result<Option<Frame>> {
            // A zero `LastPresentTime` means nothing was presented since the
            // last acquire, and the surface handed back with it carries no
            // desktop image at all - not a stale one, an uninitialized one.
            // Measured on real hardware, not inferred from the docs: the very
            // first acquire after `DuplicateOutput` reports
            // `LastPresentTime == 0, AccumulatedFrames == 0` and its surface
            // is uniformly zero, so treating it as "the current screen" (to
            // give a newly attached viewer something immediately) publishes a
            // black frame of the host's desktop. There is no such thing as a
            // valid image without a present; the honest answer is "nothing
            // changed" and the first real present delivers a full frame a few
            // milliseconds later (§11.1, ADR 0012).
            if info.LastPresentTime == 0 {
                return Ok(None);
            }
            let Some(resource) = resource else {
                return Err(MediaError::CaptureInterrupted(
                    "desktop duplication acquired a frame with no surface".to_owned(),
                ));
            };
            // SAFETY: QueryInterface on a resource this function borrows; the
            // desktop image is always a 2D texture.
            let texture = resource.cast::<ID3D11Texture2D>().map_err(|e| {
                MediaError::CaptureInterrupted(format!(
                    "the acquired desktop surface is not a texture: {e}"
                ))
            })?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: GetDesc writes into a plain `#[repr(C)]` local that
            // outlives the call and reads only the texture's own state.
            unsafe { texture.GetDesc(&raw mut desc) };

            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err(MediaError::CaptureInterrupted(format!(
                    "desktop duplication handed back an unexpected pixel format: {:?}",
                    desc.Format.0
                )));
            }
            let (width, height) = (desc.Width, desc.Height);
            if width == 0 || height == 0 {
                return Err(MediaError::CaptureInterrupted(
                    "desktop duplication handed back an empty surface".to_owned(),
                ));
            }

            // A resolution change, a rotation or a monitor swap all show up
            // here as a texture that no longer matches the copy target.
            if self.staging.width != width || self.staging.height != height {
                self.staging = Staging::new(&self.device, width, height)?;
            }

            // SAFETY: CopyResource is a GPU-side copy between two textures of
            // identical description, both created on or owned by this
            // struct's device. The immediate context is only ever touched
            // through `&mut self`, so no other thread can be inside it.
            unsafe {
                self.context.CopyResource(&self.staging.texture, &texture);
            }

            let data = read_back(&self.context, &self.staging.texture, width, height)?;

            // A present is not a change: Windows repaints regions that end up
            // pixel-identical (measured here - 13 consecutive presents, each
            // with dirty rects, all byte-identical), so `LastPresentTime` and
            // the dirty-rect metadata both say "new frame" for a screen that
            // did not visibly change. Hashing is what actually answers §11.1's
            // "identical to the previous one", and it is far cheaper than
            // encoding and shipping a duplicate 4K frame (ADR 0012).
            let hash = *blake3::hash(&data).as_bytes();
            if self.last_hash == Some(hash) {
                return Ok(None);
            }
            self.last_hash = Some(hash);

            let timestamp_us =
                u64::try_from(self.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
            Ok(Some(Frame {
                width,
                height,
                format: PixelFormat::Bgra8,
                timestamp_us,
                data,
            }))
        }
    }

    impl Staging {
        /// Allocates a CPU-readable BGRA8 texture of exactly `width`x`height`.
        fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                // A staging texture is never bound to the pipeline; it exists
                // only to be mapped, which is what makes the CPU read legal.
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is a local that outlives the call and is only
            // read; `texture` receives an owned interface pointer on success.
            unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
                .map_err(|e| {
                    MediaError::CaptureUnavailable(format!(
                        "allocating a {width}x{height} readback texture failed: {e}"
                    ))
                })?;
            let texture = texture.ok_or_else(|| {
                MediaError::CaptureUnavailable(
                    "Direct3D reported success with no readback texture".to_owned(),
                )
            })?;
            Ok(Self {
                texture,
                width,
                height,
            })
        }
    }

    /// Copies a mapped staging texture into an owned, tightly packed buffer.
    ///
    /// Direct3D's `RowPitch` is at least `width * 4` and usually more, while
    /// [`Frame::data`] is what the encoders index with a `width * 4` stride, so
    /// the rows are repacked rather than handed over as-is.
    fn read_back(
        context: &ID3D11DeviceContext,
        staging: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` was created with D3D11_USAGE_STAGING and
        // D3D11_CPU_ACCESS_READ, which is what makes a D3D11_MAP_READ of
        // subresource 0 legal; `mapped` is a local that outlives the call.
        unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }.map_err(
            |e| MediaError::CaptureInterrupted(format!("mapping the captured frame failed: {e}")),
        )?;

        let row_pitch = mapped.RowPitch as usize;
        let row_bytes = (width as usize) * BYTES_PER_PIXEL;
        let mapped_len = row_pitch.checked_mul(height as usize);

        let copied = match mapped_len {
            Some(len) if !mapped.pData.is_null() && row_pitch >= row_bytes => {
                // SAFETY: Map guarantees `pData` addresses `RowPitch * Height`
                // readable bytes of a 2D staging texture until the matching
                // Unmap below, and the pointer is checked non-null here. The
                // slice is confined to this block: every byte read out of it
                // is copied into an owned Vec before Unmap runs.
                Some(unsafe {
                    let src = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len);
                    let mut out = Vec::with_capacity(row_bytes * (height as usize));
                    for row in 0..(height as usize) {
                        let start = row * row_pitch;
                        out.extend_from_slice(&src[start..start + row_bytes]);
                    }
                    out
                })
            }
            _ => None,
        };

        // SAFETY: balances the Map above. It has to run even on the rejected
        // path: a staging texture left mapped can never be copied into again.
        unsafe { context.Unmap(staging, 0) };

        copied.ok_or_else(|| {
            MediaError::CaptureInterrupted(
                "desktop duplication mapped an unusable frame buffer".to_owned(),
            )
        })
    }

    /// Maps a failure while opening a duplication.
    ///
    /// Everything here is "capture cannot run right now", never
    /// [`MediaError::PermissionDenied`]: Desktop Duplication has no user-facing
    /// prompt to decline, so reporting one would tell the UI to blame the host
    /// for something they were never asked (§18).
    fn map_start_error(error: &windows::core::Error) -> MediaError {
        let code = error.code();
        let reason = if code == E_ACCESSDENIED {
            "the secure desktop (lock screen, UAC prompt or fast user switch) is in the foreground"
        } else if code == E_INVALIDARG {
            // MSDN: DuplicateOutput reports E_INVALIDARG when the calling
            // process is already duplicating this output. One duplication per
            // output per process is the hard limit, which is why `start`
            // drops any previous one first.
            "this process is already duplicating this display"
        } else if code == DXGI_ERROR_UNSUPPORTED {
            "this display does not support desktop duplication (a hybrid-graphics or remote session can do this)"
        } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
            "the maximum number of desktop duplications on this display is already in use"
        } else if code == DXGI_ERROR_SESSION_DISCONNECTED {
            "this session is disconnected"
        } else {
            return MediaError::CaptureUnavailable(format!("DuplicateOutput failed: {error}"));
        };
        MediaError::CaptureUnavailable(format!("desktop duplication is unavailable: {reason}"))
    }

    /// Maps a failure on a duplication that was already running.
    ///
    /// Once frames have started, a failure means capture stopped, which is
    /// [`MediaError::CaptureInterrupted`] rather than "unavailable" — the
    /// distinction the caller needs to revoke instead of retrying (§18). The
    /// recognized codes get a specific reason so the UI can say what happened;
    /// the rest still stop capture rather than being swallowed.
    fn map_runtime_error(error: &windows::core::Error) -> MediaError {
        let code = error.code();
        let reason = if code == DXGI_ERROR_ACCESS_LOST || code == E_ACCESSDENIED {
            "the desktop changed: a screen lock, a UAC prompt, a user switch or a mode change"
        } else if code == DXGI_ERROR_SESSION_DISCONNECTED {
            "the session was disconnected"
        } else if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
            "the graphics device was reset or removed"
        } else {
            return MediaError::CaptureInterrupted(format!("desktop duplication failed: {error}"));
        };
        MediaError::CaptureInterrupted(format!("capture stopped: {reason}"))
    }

    /// DXGI Desktop Duplication capturer (§11; ADR 0012).
    #[derive(Debug, Default)]
    pub struct WindowsCapturer {
        active: Option<Active>,
    }

    impl WindowsCapturer {
        /// Creates a capturer that opens a duplication on
        /// [`ScreenCapturer::start`].
        #[must_use]
        pub const fn new() -> Self {
            Self { active: None }
        }

        /// Frames per second this backend is polled at (§11, §14).
        #[must_use]
        pub const fn suggested_fps() -> u8 {
            ENCODE_DEFAULT_FPS
        }

        /// How many displays this host can currently capture, which is the
        /// exclusive upper bound of [`CaptureTarget::Display`].
        ///
        /// # Errors
        /// [`MediaError::CaptureUnavailable`] if DXGI cannot be reached at all.
        pub fn display_count() -> Result<usize> {
            attached_monitors().map(|monitors| monitors.len())
        }
    }

    impl ScreenCapturer for WindowsCapturer {
        fn start(&mut self, target: CaptureTarget) -> Result<()> {
            // Drop any previous duplication first: Windows caps how many a
            // display may have, so a restart must not compete with itself.
            self.active = None;
            self.active = Some(Active::open(target)?);
            Ok(())
        }

        fn next_frame(&mut self) -> Result<Option<Frame>> {
            let active = self
                .active
                .as_mut()
                .ok_or_else(|| MediaError::CaptureUnavailable("capturer not started".to_owned()))?;
            active.next_frame()
        }

        fn stop(&mut self) {
            // Releasing IDXGIOutputDuplication is what actually stops Windows
            // from handing this process desktop frames; there is nothing else
            // to tear down, and dropping twice is a no-op.
            self.active = None;
        }

        fn input_capability(&self) -> InputCapability {
            // SendInput reaches the whole desktop, the same way XTEST does on
            // X11. `lumepeer-core` has already authorized every event by the
            // time an injector sees it (§2.3, §11).
            InputCapability::Full
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use std::sync::Mutex;

        use super::*;

        /// Windows allows exactly one duplication of a given output per
        /// process: a second `DuplicateOutput` on the same display reports
        /// `E_INVALIDARG` while the first is alive. `cargo test` runs test
        /// functions in parallel threads of one process, so the tests that
        /// actually duplicate the screen take this first, or they would race
        /// each other into a false "no duplicable desktop" skip.
        static ONE_DUPLICATION_AT_A_TIME: Mutex<()> = Mutex::new(());

        /// A capture backend must never claim it is running when it is not.
        #[test]
        fn frames_are_refused_before_start_and_after_stop() {
            let mut capturer = WindowsCapturer::new();
            assert!(matches!(
                capturer.next_frame(),
                Err(MediaError::CaptureUnavailable(_))
            ));
            // Idempotent per the trait contract, including before any start.
            capturer.stop();
            capturer.stop();
            assert!(matches!(
                capturer.next_frame(),
                Err(MediaError::CaptureUnavailable(_))
            ));
            assert_eq!(capturer.input_capability(), InputCapability::Full);
        }

        /// `CaptureTarget::Display` indexes real monitors, so an index past
        /// the last one must be refused rather than silently captured as
        /// something else.
        #[test]
        fn a_display_index_past_the_last_monitor_is_refused() {
            let Ok(count) = WindowsCapturer::display_count() else {
                eprintln!("skipping: DXGI is unreachable on this machine");
                return;
            };
            let past_the_end = u32::try_from(count).unwrap_or(u32::MAX);
            let mut capturer = WindowsCapturer::new();
            assert!(matches!(
                capturer.start(CaptureTarget::Display(past_the_end)),
                Err(MediaError::CaptureUnavailable(_))
            ));
        }

        /// Exactly one monitor is the primary, and it is the one
        /// `PrimaryDisplay` resolves to.
        #[test]
        fn the_primary_display_is_found_among_the_attached_monitors() {
            let Ok(monitors) = attached_monitors() else {
                eprintln!("skipping: DXGI is unreachable on this machine");
                return;
            };
            if monitors.is_empty() {
                eprintln!("skipping: no display is attached to this desktop");
                return;
            }
            eprintln!("{} attached display(s): {monitors:?}", monitors.len());
            assert_eq!(
                monitors.iter().filter(|m| m.is_primary()).count(),
                1,
                "exactly one attached monitor sits at the virtual-screen origin"
            );
            for monitor in &monitors {
                assert!(monitor.width() > 0 && monitor.height() > 0);
            }
            let primary = select(monitors, CaptureTarget::PrimaryDisplay).unwrap();
            assert!(primary.is_primary());
        }

        /// The real thing: a live desktop must produce a real, tightly packed
        /// BGRA8 frame. Skipped rather than failed where there is no desktop
        /// session to duplicate (a headless or session-0 CI runner), the same
        /// way the X11 backend's equivalent test degrades.
        #[test]
        fn capture_produces_a_frame_when_a_display_is_available() {
            // Declared first so it outlives `capturer`, which only releases
            // the duplication when it drops.
            let _serialized = ONE_DUPLICATION_AT_A_TIME.lock();
            let mut capturer = WindowsCapturer::new();
            if let Err(e) = capturer.start(CaptureTarget::PrimaryDisplay) {
                eprintln!("skipping: no duplicable desktop on this machine ({e})");
                return;
            }

            // A completely static desktop legitimately presents nothing, so
            // poll for a bounded while rather than demanding the first call
            // produce a frame.
            let mut frame = None;
            for _ in 0..POLL_ATTEMPTS {
                match capturer.next_frame() {
                    Ok(Some(f)) => {
                        frame = Some(f);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => panic!("capture failed on a live desktop: {e}"),
                }
            }
            let Some(frame) = frame else {
                eprintln!("skipping: this desktop presented nothing while the test ran");
                return;
            };

            assert!(frame.width > 0 && frame.height > 0);
            assert_eq!(frame.format, PixelFormat::Bgra8);
            assert_eq!(
                frame.data.len(),
                (frame.width as usize) * (frame.height as usize) * BYTES_PER_PIXEL,
                "frames must be tightly packed, not RowPitch-strided"
            );

            // The regression guard for the bug real hardware caught: the
            // surface that comes back with `LastPresentTime == 0` is
            // uniformly zero, so a backend that hands it out publishes a
            // black picture of the host's screen. A frame that reached a
            // viewer must never be uniform - a live desktop with a present
            // behind it never is.
            let uniform = frame.data.windows(2).all(|w| w[0] == w[1]);
            eprintln!(
                "captured {}x{} BGRA8, {} bytes, first pixel {:?}, uniform={uniform}",
                frame.width,
                frame.height,
                frame.data.len(),
                &frame.data[..BYTES_PER_PIXEL]
            );
            assert!(
                !uniform,
                "a delivered frame must be a real desktop image, not the empty \
                 surface of a present-less acquire"
            );

            capturer.stop();
            assert!(matches!(
                capturer.next_frame(),
                Err(MediaError::CaptureUnavailable(_))
            ));
        }

        /// Every frame that does reach a viewer has to be a distinct picture:
        /// §11.1 says `None` for "identical to the previous one", and Windows
        /// presents often enough that taking its word for it would ship
        /// duplicate 4K frames down the wire.
        #[test]
        fn no_two_delivered_frames_in_a_row_are_identical() {
            // See `capture_produces_a_frame_when_a_display_is_available`.
            let _serialized = ONE_DUPLICATION_AT_A_TIME.lock();
            let mut capturer = WindowsCapturer::new();
            if let Err(e) = capturer.start(CaptureTarget::PrimaryDisplay) {
                eprintln!("skipping: no duplicable desktop on this machine ({e})");
                return;
            }

            let mut hashes = Vec::new();
            for _ in 0..POLL_ATTEMPTS {
                match capturer.next_frame() {
                    Ok(Some(frame)) => hashes.push(*blake3::hash(&frame.data).as_bytes()),
                    Ok(None) => {}
                    Err(e) => panic!("capture failed on a live desktop: {e}"),
                }
            }
            if hashes.len() < 2 {
                eprintln!("skipping: this desktop produced fewer than two frames");
                return;
            }
            eprintln!("{} frame(s) delivered over the poll window", hashes.len());
            let mut deduped = hashes.clone();
            deduped.dedup();
            assert_eq!(
                deduped.len(),
                hashes.len(),
                "a repeated frame was delivered instead of None"
            );
        }

        /// Bounded poll for the tests above: at `ENCODE_DEFAULT_FPS` this is a
        /// couple of seconds of real desktop time, plenty for a blinking
        /// caret or a clock, and it cannot hang the suite.
        const POLL_ATTEMPTS: usize = 60;
    }
}

/// Capture stub for a Windows build without the `capture-windows` feature.
///
/// Kept so `cargo build --workspace` (the CI default path) still compiles this
/// module, needs no Direct3D 11/DXGI bindings, and still exposes
/// `capture::windows::WindowsCapturer` to anything that names it.
#[cfg(not(feature = "capture-windows"))]
mod stub {
    use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
    use crate::error::{MediaError, Result};

    /// DXGI/WGC capturer, not built in.
    #[derive(Debug, Default)]
    pub struct WindowsCapturer {
        _private: (),
    }

    impl WindowsCapturer {
        /// Creates the stub capturer.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }
    }

    /// Why every call below refuses, in one place.
    fn not_built_in() -> MediaError {
        MediaError::CaptureUnavailable(
            "windows capture is not built in: rebuild with the `capture-windows` feature"
                .to_owned(),
        )
    }

    impl ScreenCapturer for WindowsCapturer {
        fn start(&mut self, _target: CaptureTarget) -> Result<()> {
            Err(not_built_in())
        }

        fn next_frame(&mut self) -> Result<Option<Frame>> {
            Err(not_built_in())
        }

        fn stop(&mut self) {}

        fn input_capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }
}
