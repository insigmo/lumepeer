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
//!   switch, a session lock or a mode change, which is §18's capture
//!   interruption verbatim (see [`MediaError::CaptureInterrupted`] and, for
//!   the secure-desktop case specifically,
//!   [`MediaError::SecureDesktopActive`] — `docs/bugs/11-uac-degradation.md`).
//!   Nothing has to infer an interruption from a timeout or a silent stream.
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
pub use dxgi::{WindowsCapturer, WindowsInjector};
#[cfg(not(feature = "capture-windows"))]
pub use stub::{WindowsCapturer, WindowsInjector};

/// DXGI Desktop Duplication capture, plus `SendInput` injection (§11, §18;
/// ADR 0012).
///
/// The fourth place in the crate that needs `unsafe`, after `decode::shm`
/// (ADR 0005), `decode::windows_sandbox` (ADR 0007) and `encode::windows`
/// (ADR 0011): every `IDXGIOutputDuplication`/`ID3D11Device` call in the
/// `windows` crate's Direct3D 11 and DXGI bindings is `unsafe fn` because it
/// crosses into COM, and `SendInput` (below, `WindowsInjector`) is `unsafe`
/// for the same FFI reason even though it touches no COM interface. Each
/// `unsafe` block in this module carries a `SAFETY:` note, as §21 requires.
#[cfg(feature = "capture-windows")]
#[allow(
    unsafe_code,
    reason = "DXGI Desktop Duplication is COM and SendInput is raw FFI; every IDXGIOutputDuplication/ID3D11Device call in the `windows` crate is `unsafe fn`. See ADR 0012."
)]
mod dxgi {
    use std::time::{Duration, Instant};

    use lumepeer_core::constants::{
        ENCODE_DEFAULT_FPS, SECURE_DESKTOP_RECOVERY_BACKOFF_MS,
        SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS,
    };
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
        DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
        DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
        DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC, IDXGIAdapter1, IDXGIFactory1,
        IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDCW,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, SRCCOPY, SelectObject,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK,
        VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT,
        VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
    };
    use windows::core::{Interface as _, PCWSTR};

    use lumepeer_core::protocol::{
        CursorShapeData, InputDetail, InputEventPayload, POINTER_BUTTON_LOGICAL_BASE,
    };

    use lumepeer_core::constants::MAX_CURSOR_SHAPE_PIXELS;

    use crate::capture::{
        CaptureTarget, Frame, InputCapability, InputInjector, PixelFormat, ScreenCapturer,
        cursor_shape as bounded_cursor_shape,
    };
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

    /// One GDI `BitBlt` of the monitor named by a DXGI `DeviceName`
    /// (`\\.\DISPLAY1` and friends) into a tightly packed BGRA8 buffer.
    ///
    /// This is the "give a freshly attached viewer something real" path: it
    /// reads whatever is on screen right now, with no dependence on presents,
    /// which is exactly what Desktop Duplication cannot do before its first
    /// real one. It runs once per duplication, on the frame clock, so the
    /// extra copy is amortized to nothing.
    ///
    /// # Errors
    /// [`MediaError::CaptureInterrupted`] if any GDI step fails; the encode
    /// loop treats that like any other capture error and retries from a
    /// fresh duplication.
    fn gdi_snapshot(device_name: &[u16; 32], width: i32, height: i32) -> Result<Vec<u8>> {
        // SAFETY: every call below takes plain values or owns what it
        // creates; the DCs and bitmap are released on every exit path via
        // `DeleteDC`/`DeleteObject`, and `SelectObject`'s return value is
        // deliberately left selected — the DC dies immediately afterwards.
        unsafe {
            let dc = CreateDCW(
                PCWSTR::null(),
                PCWSTR(device_name.as_ptr()),
                PCWSTR::null(),
                None,
            );
            if dc.is_invalid() {
                return Err(MediaError::CaptureInterrupted(
                    "GDI could not open a DC for this display".to_owned(),
                ));
            }

            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>())
                        .unwrap_or(u32::MAX),
                    biWidth: width,
                    // A negative height asks for top-down rows, which is the
                    // order both Desktop Duplication and the wire format use;
                    // bottom-up GDI order would ship the picture flipped.
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let section = CreateDIBSection(
                Some(dc),
                core::ptr::from_ref(&bi),
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
            .map_err(|e| MediaError::CaptureInterrupted(format!("CreateDIBSection failed: {e}")))?;
            let memory_dc = CreateCompatibleDC(Some(dc));
            let old = SelectObject(memory_dc, section.into());

            let blitted = BitBlt(memory_dc, 0, 0, width, height, Some(dc), 0, 0, SRCCOPY).is_ok();

            let bytes = usize::try_from(width).unwrap_or(0)
                * usize::try_from(height).unwrap_or(0)
                * BYTES_PER_PIXEL;
            let data = if blitted && !bits.is_null() {
                Some(std::slice::from_raw_parts(bits.cast::<u8>(), bytes).to_vec())
            } else {
                None
            };

            SelectObject(memory_dc, old);
            let _ = DeleteObject(section.into());
            let _ = DeleteDC(memory_dc);
            let _ = DeleteDC(dc);

            match data {
                Some(data) => Ok(data),
                None => Err(MediaError::CaptureInterrupted(
                    "the GDI snapshot BitBlt failed".to_owned(),
                )),
            }
        }
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
        /// The last cursor bitmap Desktop Duplication reported, kept across
        /// frames because a shape update is only reported when it changes,
        /// not on every acquire (MSDN, `PointerShapeBufferSize`).
        pointer_shape: Option<PointerShape>,
        /// Bumped each time Desktop Duplication reports a *new* bitmap.
        ///
        /// The DXGI reply carries no identity for a shape, so this stands in
        /// for one: [`ScreenCapturer::cursor_shape`] answers "unchanged" by
        /// comparing this against what it last handed out, rather than by
        /// comparing up to `MAX_CURSOR_SHAPE_PIXELS` of pixels every frame.
        pointer_shape_seq: u64,
        /// `pointer_shape_seq` of the shape already sent to the guest.
        reported_shape_seq: Option<u64>,
        /// Whether the cursor is drawn into the frames this backend produces.
        /// Turned off when the guest has said it will draw the cursor itself
        /// (§11; `FEATURE_CURSOR_SHAPE`).
        embed_cursor: bool,
        /// Top-left corner to draw `pointer_shape` at, in this monitor's
        /// frame coordinates, or `None` while the OS reports the pointer
        /// hidden. Already hotspot-adjusted by DXGI (MSDN,
        /// `DXGI_OUTDUPL_POINTER_POSITION`), so no offset is applied here.
        pointer_position: Option<(i32, i32)>,
        /// The last desktop image read back from the GPU, without the cursor
        /// composited in, so a pointer-only update (see `next_frame`) can
        /// redraw the cursor at its new spot without leaving a trail of
        /// previous positions baked into the picture.
        last_frame_data: Option<Vec<u8>>,
        /// The DXGI `DeviceName` of the duplicated monitor
        /// (`\\.\DISPLAY1` and friends), so the one-shot first-frame GDI
        /// snapshot can address the same display Desktop Duplication was
        /// opened for.
        device_name: [u16; 32],
        /// Whether no frame has been handed out yet. Desktop Duplication's
        /// surface is uninitialized garbage until its first real present
        /// (ADR 0012), but a freshly attached viewer must see *something*
        /// immediately — a static desktop may not present again for minutes.
        /// The very first `next_frame` therefore answers with a plain GDI
        /// snapshot instead; from the second frame on, duplication alone
        /// drives delivery (§11.1).
        awaiting_first_frame: bool,
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
            let desc = unsafe { duplication.GetDesc() };
            let mode = desc.ModeDesc;
            // The GDI first-frame snapshot has to address the same display
            // this duplication was opened for; the duplication description
            // does not carry a device name, but the output's own does.
            //
            // SAFETY: GetDesc returns the output's description by value; it
            // reads nothing this side owns.
            let output_desc = unsafe { monitor.output.GetDesc() }.map_err(|e| {
                MediaError::CaptureUnavailable(format!(
                    "the duplicated display reported no description: {e}"
                ))
            })?;
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
                pointer_shape: None,
                pointer_shape_seq: 0,
                reported_shape_seq: None,
                embed_cursor: true,
                pointer_position: None,
                last_frame_data: None,
                device_name: output_desc.DeviceName,
                awaiting_first_frame: true,
                started_at: Instant::now(),
            })
        }

        /// One `AcquireNextFrame`/`ReleaseFrame` cycle.
        fn next_frame(&mut self) -> Result<Option<Frame>> {
            // The very first frame a viewer sees must be the screen as it is
            // *now*, not whenever the desktop next happens to present (a
            // static desktop may not present for minutes, and ADR 0012 rules
            // out trusting the uninitialized pre-present surface). One GDI
            // snapshot answers that; from here on duplication drives
            // delivery.
            if self.awaiting_first_frame {
                let data = gdi_snapshot(
                    &self.device_name,
                    self.staging.width.cast_signed(),
                    self.staging.height.cast_signed(),
                )?;
                self.last_frame_data = Some(data.clone());
                self.awaiting_first_frame = false;
                return Ok(self.finish_frame(data));
            }

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
        /// BGRA8 [`Frame`] with the cursor composited on top (ADR 0012's
        /// "drawing it is left to a later change"), or `None` when nothing
        /// visible changed.
        fn frame_from(
            &mut self,
            resource: Option<&IDXGIResource>,
            info: &DXGI_OUTDUPL_FRAME_INFO,
        ) -> Result<Option<Frame>> {
            // A shape update is only reported when it actually changes, not
            // on every acquire, so this is fetched before anything else and
            // cached on `self` regardless of which path below returns.
            if info.PointerShapeBufferSize > 0 {
                self.pointer_shape = Some(fetch_pointer_shape(
                    &self.duplication,
                    info.PointerShapeBufferSize,
                )?);
                self.pointer_shape_seq = self.pointer_shape_seq.wrapping_add(1);
            }
            if info.LastMouseUpdateTime != 0 {
                self.pointer_position = info.PointerPosition.Visible.as_bool().then_some((
                    info.PointerPosition.Position.x,
                    info.PointerPosition.Position.y,
                ));
            }

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
                // The cursor alone can still have moved: DXGI reports that as
                // a real acquire with a new PointerPosition but no new
                // desktop image. Recompositing it onto the last delivered
                // (cursor-free) desktop pixels keeps cursor motion visible
                // over an otherwise static screen instead of it going stale
                // between real repaints.
                if info.LastMouseUpdateTime == 0 {
                    return Ok(None);
                }
                let Some(base) = self.last_frame_data.clone() else {
                    return Ok(None);
                };
                return Ok(self.finish_frame(base));
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
            // Cached cursor-free, so a later pointer-only update (above) has
            // a clean base to redraw the cursor onto instead of compositing
            // on top of wherever it last was.
            self.last_frame_data = Some(data.clone());
            Ok(self.finish_frame(data))
        }

        /// Composites the cached cursor onto `data` (if one is visible),
        /// hashes the result and turns it into a [`Frame`], or `None` when
        /// §11.1's "identical to the previous one" holds even with the
        /// cursor included.
        fn finish_frame(&mut self, mut data: Vec<u8>) -> Option<Frame> {
            if self.embed_cursor
                && let (Some(pos), Some(shape)) =
                    (self.pointer_position, self.pointer_shape.as_ref())
            {
                composite_pointer(
                    &mut data,
                    self.staging.width,
                    self.staging.height,
                    pos,
                    shape,
                );
            }

            // A present is not a change: Windows repaints regions that end up
            // pixel-identical (measured here - 13 consecutive presents, each
            // with dirty rects, all byte-identical), so `LastPresentTime` and
            // the dirty-rect metadata both say "new frame" for a screen that
            // did not visibly change. Hashing is what actually answers §11.1's
            // "identical to the previous one", and it is far cheaper than
            // encoding and shipping a duplicate 4K frame (ADR 0012). Hashing
            // after compositing means cursor motion over a static screen
            // still counts as a change, which is correct: the picture a
            // viewer sees did change.
            let hash = *blake3::hash(&data).as_bytes();
            if self.last_hash == Some(hash) {
                return None;
            }
            self.last_hash = Some(hash);

            let timestamp_us =
                u64::try_from(self.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
            Some(Frame {
                width: self.staging.width,
                height: self.staging.height,
                format: PixelFormat::Bgra8,
                timestamp_us,
                data,
            })
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

    /// A cursor bitmap as `GetFramePointerShape` hands it back: still in
    /// whichever of the three DXGI shape encodings the OS chose, since that
    /// choice is what [`composite_pointer`] switches on.
    struct PointerShape {
        kind: u32,
        width: u32,
        height: u32,
        pitch: u32,
        pixels: Vec<u8>,
    }

    /// Reads the current cursor bitmap off `duplication`.
    ///
    /// Only called when `DXGI_OUTDUPL_FRAME_INFO::PointerShapeBufferSize` is
    /// nonzero, i.e. the shape changed since the last acquire - Desktop
    /// Duplication does not repeat it on every frame.
    fn fetch_pointer_shape(
        duplication: &IDXGIOutputDuplication,
        buffer_size: u32,
    ) -> Result<PointerShape> {
        let mut buffer = vec![0u8; buffer_size as usize];
        let mut required = 0u32;
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: `buffer` is a locally owned allocation exactly
        // `buffer_size` bytes long, which is what `GetFramePointerShape`
        // requires the capacity argument to match; `required` and
        // `shape_info` are locals that outlive the call and are only written
        // to.
        unsafe {
            duplication.GetFramePointerShape(
                buffer_size,
                buffer.as_mut_ptr().cast(),
                &raw mut required,
                &raw mut shape_info,
            )
        }
        .map_err(|e| {
            MediaError::CaptureInterrupted(format!("reading the cursor shape failed: {e}"))
        })?;
        buffer.truncate(required as usize);
        Ok(PointerShape {
            kind: shape_info.Type,
            width: shape_info.Width,
            height: shape_info.Height,
            pitch: shape_info.Pitch,
            pixels: buffer,
        })
    }

    /// Draws `shape` onto `data` with its top-left corner at `pos`, clipped
    /// to `frame_width`x`frame_height`.
    ///
    /// `pos` needs no hotspot adjustment: DXGI's `PointerPosition.Position`
    /// already names where the shape's top-left corner belongs (MSDN,
    /// `DXGI_OUTDUPL_POINTER_POSITION`).
    fn composite_pointer(
        data: &mut [u8],
        frame_width: u32,
        frame_height: u32,
        pos: (i32, i32),
        shape: &PointerShape,
    ) {
        if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned() {
            composite_monochrome(data, frame_width, frame_height, pos, shape);
        } else if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned() {
            composite_color(data, frame_width, frame_height, pos, shape, false);
        } else if shape.kind
            == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                .0
                .cast_unsigned()
        {
            composite_color(data, frame_width, frame_height, pos, shape, true);
        }
        // Any other value is a shape type this DXGI version does not
        // document; leaving the frame undrawn is the honest degradation.
    }

    /// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR` / `..._MASKED_COLOR`: a 32bpp
    /// BGRA bitmap.
    ///
    /// Plain `COLOR` alpha-blends normally. `MASKED_COLOR` reuses the alpha
    /// byte as a 1-bit AND mask instead of real alpha (MSDN): a set bit means
    /// XOR the color onto the background (used for cursors like the text
    /// I-beam that must stay visible over any background), a clear bit means
    /// draw the color opaque.
    fn composite_color(
        data: &mut [u8],
        frame_width: u32,
        frame_height: u32,
        pos: (i32, i32),
        shape: &PointerShape,
        masked: bool,
    ) {
        let (pos_x, pos_y) = pos;
        for row in 0..shape.height {
            let Some(dst_y) = pos_y
                .checked_add_unsigned(row)
                .filter(|y| u32::try_from(*y).is_ok_and(|y| y < frame_height))
            else {
                continue;
            };
            let src_row = (row * shape.pitch) as usize;
            for col in 0..shape.width {
                let Some(dst_x) = pos_x
                    .checked_add_unsigned(col)
                    .filter(|x| u32::try_from(*x).is_ok_and(|x| x < frame_width))
                else {
                    continue;
                };
                let src_off = src_row + (col as usize) * BYTES_PER_PIXEL;
                let Some(src) = shape.pixels.get(src_off..src_off + BYTES_PER_PIXEL) else {
                    continue;
                };
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "dst_x/dst_y were just checked non-negative and in range above"
                )]
                let dst_off =
                    ((dst_y as u32 * frame_width + dst_x as u32) as usize) * BYTES_PER_PIXEL;
                let Some(dst) = data.get_mut(dst_off..dst_off + BYTES_PER_PIXEL) else {
                    continue;
                };
                if masked {
                    if src[3] == 0 {
                        dst[0] = src[0];
                        dst[1] = src[1];
                        dst[2] = src[2];
                    } else {
                        dst[0] ^= src[0];
                        dst[1] ^= src[1];
                        dst[2] ^= src[2];
                    }
                    dst[3] = 0xFF;
                } else {
                    let alpha = u16::from(src[3]);
                    if alpha == 0 {
                        continue;
                    }
                    if alpha == 0xFF {
                        dst[..3].copy_from_slice(&src[..3]);
                    } else {
                        for c in 0..3 {
                            let s = u16::from(src[c]);
                            let d = u16::from(dst[c]);
                            // s, d in 0..=255 and alpha in 1..=254 here (0 and
                            // 255 are handled above), so the blend is always
                            // in 0..=255: never a truncating cast.
                            #[allow(
                                clippy::cast_possible_truncation,
                                reason = "blend of two u8 channels by a u8 alpha over 255 is bounded to 0..=255"
                            )]
                            let blended = ((s * alpha + d * (255 - alpha)) / 255) as u8;
                            dst[c] = blended;
                        }
                    }
                    dst[3] = 0xFF;
                }
            }
        }
    }

    /// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME`: a 1bpp AND mask stacked
    /// directly above a 1bpp XOR mask of the same size, so `shape.height` is
    /// double the actual cursor height (MSDN).
    ///
    /// Per-pixel result follows the classic Win32 monochrome cursor rule:
    /// AND=1,XOR=0 leaves the background alone (transparent), AND=0,XOR=0 is
    /// opaque black, AND=0,XOR=1 is opaque white, AND=1,XOR=1 inverts
    /// whatever is already there.
    fn composite_monochrome(
        data: &mut [u8],
        frame_width: u32,
        frame_height: u32,
        pos: (i32, i32),
        shape: &PointerShape,
    ) {
        let (pos_x, pos_y) = pos;
        let cursor_height = shape.height / 2;
        for row in 0..cursor_height {
            let Some(dst_y) = pos_y
                .checked_add_unsigned(row)
                .filter(|y| u32::try_from(*y).is_ok_and(|y| y < frame_height))
            else {
                continue;
            };
            let and_row = (row * shape.pitch) as usize;
            let xor_row = ((row + cursor_height) * shape.pitch) as usize;
            for col in 0..shape.width {
                let Some(dst_x) = pos_x
                    .checked_add_unsigned(col)
                    .filter(|x| u32::try_from(*x).is_ok_and(|x| x < frame_width))
                else {
                    continue;
                };
                let byte_index = (col / 8) as usize;
                let bit = 7 - (col % 8);
                let (Some(&and_byte), Some(&xor_byte)) = (
                    shape.pixels.get(and_row + byte_index),
                    shape.pixels.get(xor_row + byte_index),
                ) else {
                    continue;
                };
                let and_bit = (and_byte >> bit) & 1;
                let xor_bit = (xor_byte >> bit) & 1;
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "dst_x/dst_y were just checked non-negative and in range above"
                )]
                let dst_off =
                    ((dst_y as u32 * frame_width + dst_x as u32) as usize) * BYTES_PER_PIXEL;
                let Some(dst) = data.get_mut(dst_off..dst_off + BYTES_PER_PIXEL) else {
                    continue;
                };
                match (and_bit, xor_bit) {
                    (0, 0) => {
                        dst[0] = 0;
                        dst[1] = 0;
                        dst[2] = 0;
                        dst[3] = 0xFF;
                    }
                    (0, 1) => {
                        dst[0] = 0xFF;
                        dst[1] = 0xFF;
                        dst[2] = 0xFF;
                        dst[3] = 0xFF;
                    }
                    (1, 0) => {}
                    _ => {
                        dst[0] ^= 0xFF;
                        dst[1] ^= 0xFF;
                        dst[2] ^= 0xFF;
                    }
                }
            }
        }
    }

    /// Maps a failure while opening a duplication.
    ///
    /// Everything here is "capture cannot run right now", never
    /// [`MediaError::PermissionDenied`]: Desktop Duplication has no user-facing
    /// prompt to decline, so reporting one would tell the UI to blame the host
    /// for something they were never asked (§18).
    fn map_start_error(error: &windows::core::Error) -> MediaError {
        let code = error.code();
        if code == E_ACCESSDENIED {
            // Distinct from the generic branch below: `WindowsCapturer`'s own
            // reopen loop (see `next_frame`) matches on this variant to keep
            // retrying instead of giving up on the first failed reopen
            // (docs/bugs/11-uac-degradation.md).
            return MediaError::SecureDesktopActive(
                "the secure desktop (lock screen, UAC prompt or fast user switch) is in the foreground"
                    .to_owned(),
            );
        }
        let reason = if code == E_INVALIDARG {
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
    /// distinction the caller needs to revoke instead of retrying (§18).
    ///
    /// One exception, since `docs/bugs/11-uac-degradation.md`:
    /// `DXGI_ERROR_ACCESS_LOST`/`E_ACCESSDENIED` come back
    /// [`MediaError::SecureDesktopActive`] instead. That is still "stop
    /// capturing this instant", but `WindowsCapturer::next_frame` absorbs it
    /// with its own reopen-with-backoff loop and only turns it into a real
    /// [`MediaError::CaptureInterrupted`] — the caller's cue to revoke —
    /// once that loop's attempt budget is exhausted. Every other code here
    /// still means revoke immediately; the caller is not expected to retry
    /// them.
    ///
    /// The recognized codes get a specific reason so the UI can say what
    /// happened; the rest still stop capture rather than being swallowed.
    fn map_runtime_error(error: &windows::core::Error) -> MediaError {
        let code = error.code();
        if code == DXGI_ERROR_ACCESS_LOST || code == E_ACCESSDENIED {
            return MediaError::SecureDesktopActive(
                "the desktop changed: a screen lock, a UAC prompt, a user switch or a mode change"
                    .to_owned(),
            );
        }
        let reason = if code == DXGI_ERROR_SESSION_DISCONNECTED {
            "the session was disconnected"
        } else if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
            "the graphics device was reset or removed"
        } else {
            return MediaError::CaptureInterrupted(format!("desktop duplication failed: {error}"));
        };
        MediaError::CaptureInterrupted(format!("capture stopped: {reason}"))
    }

    /// Bookkeeping for `WindowsCapturer::next_frame`'s reopen-with-backoff
    /// loop, kept apart from `Active` so it survives the duplication being
    /// dropped and can be driven with an injected clock in tests
    /// (`docs/bugs/11-uac-degradation.md`).
    #[derive(Debug)]
    struct Recovery {
        /// Reopen attempts made so far, including the current one.
        attempts: u32,
        /// Earliest instant the next reopen attempt may run.
        next_attempt_at: Instant,
        /// Reason from the most recent failed reopen (or the failure that
        /// started this recovery), surfaced to the caller on every poll.
        reason: String,
    }

    impl Recovery {
        /// Starts tracking a recovery whose first attempt is due immediately:
        /// there is no reason to wait out a backoff before ever trying once.
        fn started(reason: String) -> Self {
            Self {
                attempts: 0,
                next_attempt_at: Instant::now(),
                reason,
            }
        }

        /// Whether a reopen attempt is due at `now`.
        fn due(&self, now: Instant) -> bool {
            now >= self.next_attempt_at
        }

        /// Counts one more attempt and reports whether
        /// [`SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS`] is now exhausted.
        fn record_attempt(&mut self) -> bool {
            self.attempts += 1;
            self.attempts > SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS
        }

        /// Schedules the next attempt one backoff interval after `now`, and
        /// updates the reason a caller polling in between should see.
        fn schedule_next(&mut self, now: Instant, reason: String) {
            self.reason = reason;
            self.next_attempt_at = now + Duration::from_millis(SECURE_DESKTOP_RECOVERY_BACKOFF_MS);
        }
    }

    /// DXGI Desktop Duplication capturer (§11; ADR 0012).
    #[derive(Debug, Default)]
    pub struct WindowsCapturer {
        active: Option<Active>,
        /// What [`ScreenCapturer::start`] was last asked to capture, kept so a
        /// reopen after the secure desktop clears targets the same monitor.
        target: Option<CaptureTarget>,
        /// Set while a duplication lost to the secure desktop is being
        /// retried; cleared as soon as a reopen succeeds or the attempt
        /// budget in [`Recovery::record_attempt`] is exhausted.
        recovering: Option<Recovery>,
    }

    impl WindowsCapturer {
        /// Creates a capturer that opens a duplication on
        /// [`ScreenCapturer::start`].
        #[must_use]
        pub const fn new() -> Self {
            Self {
                active: None,
                target: None,
                recovering: None,
            }
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

        /// Every display this host can capture, in the order
        /// [`CaptureTarget::Display`] indexes (§11 `MonitorsList`; ADR 0028).
        ///
        /// # Errors
        /// [`MediaError::CaptureUnavailable`] if DXGI cannot be reached at all.
        pub fn attached_monitors_info() -> Result<Vec<crate::capture::HostMonitor>> {
            let monitors = attached_monitors()?;
            Ok(monitors
                .iter()
                .enumerate()
                .map(|(index, monitor)| {
                    #[allow(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "enumeration index, far below u32::MAX"
                    )]
                    crate::capture::HostMonitor {
                        id: index as u32,
                        // DesktopCoordinates are i32 RECT edges; a monitor
                        // rectangle is non-degenerate, and saturating on the
                        // subtraction keeps a broken driver report a zero
                        // size instead of a wrapped one.
                        width: u32::try_from(
                            monitor
                                .desc
                                .DesktopCoordinates
                                .right
                                .saturating_sub(monitor.desc.DesktopCoordinates.left),
                        )
                        .unwrap_or(0),
                        height: u32::try_from(
                            monitor
                                .desc
                                .DesktopCoordinates
                                .bottom
                                .saturating_sub(monitor.desc.DesktopCoordinates.top),
                        )
                        .unwrap_or(0),
                        primary: monitor.is_primary(),
                    }
                })
                .collect())
        }
    }

    impl ScreenCapturer for WindowsCapturer {
        fn start(&mut self, target: CaptureTarget) -> Result<()> {
            // Drop any previous duplication first: Windows caps how many a
            // display may have, so a restart must not compete with itself.
            self.active = None;
            self.recovering = None;
            self.target = Some(target);
            self.active = Some(Active::open(target)?);
            Ok(())
        }

        /// One poll, plus the secure-desktop reopen loop of
        /// `docs/bugs/11-uac-degradation.md`.
        ///
        /// A duplication lost to `MediaError::SecureDesktopActive` is not
        /// handed back to the caller as a failure on the spot: it is dropped
        /// here and reopening is retried on later polls, at most once per
        /// [`SECURE_DESKTOP_RECOVERY_BACKOFF_MS`], for up to
        /// [`SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS`] attempts. Every poll
        /// while that is running still returns
        /// `Err(MediaError::SecureDesktopActive)` — never `Ok` — so the
        /// caller (`apps/desktop/src-tauri/src/view.rs::spawn_encode_loop`)
        /// can tell "still stuck, keep the session up and say so" apart from
        /// "gave up, revoke", without counting attempts of its own. Only once
        /// the budget above is exhausted does this return the ordinary
        /// [`MediaError::CaptureInterrupted`] that means revoke.
        fn next_frame(&mut self) -> Result<Option<Frame>> {
            if let Some(active) = self.active.as_mut() {
                match active.next_frame() {
                    Err(MediaError::SecureDesktopActive(reason)) => {
                        self.active = None;
                        self.recovering = Some(Recovery::started(reason));
                    }
                    other => {
                        // A frame (or "nothing changed") is proof the
                        // duplication is healthy; a failure of any other kind
                        // is the caller's to handle, unrelated to recovery.
                        self.recovering = None;
                        return other;
                    }
                }
            }

            let (Some(target), Some(recovery)) = (self.target, self.recovering.as_mut()) else {
                // Reached only if `next_frame` is called before `start`:
                // `self.active` being `None` above would then leave
                // `recovering` unset too, since only the branch that just
                // set it falls through to here.
                return Err(MediaError::CaptureUnavailable(
                    "capturer not started".to_owned(),
                ));
            };

            let now = Instant::now();
            if !recovery.due(now) {
                return Err(MediaError::SecureDesktopActive(recovery.reason.clone()));
            }
            if recovery.record_attempt() {
                let reason = recovery.reason.clone();
                self.recovering = None;
                return Err(MediaError::CaptureInterrupted(format!(
                    "gave up reopening after the secure desktop, {SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS} attempts: {reason}"
                )));
            }
            match Active::open(target) {
                Ok(active) => {
                    self.active = Some(active);
                    self.recovering = None;
                    // The duplication is fresh; nothing has been acquired
                    // from it yet, so honestly there is no frame this poll.
                    Ok(None)
                }
                Err(MediaError::SecureDesktopActive(reason)) => {
                    recovery.schedule_next(now, reason.clone());
                    Err(MediaError::SecureDesktopActive(reason))
                }
                Err(other) => {
                    self.recovering = None;
                    Err(other)
                }
            }
        }

        fn stop(&mut self) {
            // Releasing IDXGIOutputDuplication is what actually stops Windows
            // from handing this process desktop frames; there is nothing else
            // to tear down, and dropping twice is a no-op.
            self.active = None;
            self.recovering = None;
            self.target = None;
        }

        fn input_capability(&self) -> InputCapability {
            // SendInput reaches the whole desktop, the same way XTEST does on
            // X11. `lumepeer-core` has already authorized every event by the
            // time an injector sees it (§2.3, §11).
            InputCapability::Full
        }

        fn cursor_shape(&mut self) -> Option<CursorShapeData> {
            let active = self.active.as_mut()?;
            if active.reported_shape_seq == Some(active.pointer_shape_seq) {
                return None;
            }
            let shape = to_cursor_shape(active.pointer_shape.as_ref()?)?;
            // Recorded only once the shape passed the bound of §14: one that
            // was refused must be reconsidered if a later frame reports it.
            active.reported_shape_seq = Some(active.pointer_shape_seq);
            Some(shape)
        }

        fn set_cursor_embedded(&mut self, embedded: bool) {
            if let Some(active) = self.active.as_mut()
                && active.embed_cursor != embedded
            {
                active.embed_cursor = embedded;
                // The picture genuinely changed: the next frame either gains
                // or loses a cursor, and the §11.1 hash must not call it a
                // duplicate.
                active.last_hash = None;
            }
        }
    }

    /// Turns a DXGI cursor bitmap into the premultiplied BGRA of §11's
    /// `CursorShapeData`, or `None` when it is not a shape that can travel.
    ///
    /// Three encodings arrive here and only one of them is already what the
    /// wire wants:
    ///
    /// - `COLOR` is 32bpp BGRA with **straight** alpha, so it is premultiplied
    ///   on the way out. Skipping that step doubles the brightness of every
    ///   antialiased edge on the guest's screen.
    /// - `MASKED_COLOR` reuses the alpha byte as a 1-bit AND mask: clear means
    ///   draw the colour opaque, set means XOR it onto whatever is behind.
    /// - `MONOCHROME` is a 1bpp AND mask stacked above a 1bpp XOR mask, so its
    ///   reported height is twice the cursor's.
    ///
    /// XOR is the case an RGBA overlay cannot express at all: it is a function
    /// of the pixels underneath, and the guest is drawing on a layer above the
    /// video where those are not available. Every remote-desktop client has to
    /// choose, and this one renders an inverting pixel as opaque black —
    /// the overwhelmingly common inverting cursor is the text I-beam, and it
    /// sits over a light background almost every time it is used. The
    /// alternative is not "better colours", it is keeping the cursor composited
    /// into the video, which is exactly what a host without this channel does.
    fn to_cursor_shape(shape: &PointerShape) -> Option<CursorShapeData> {
        // Before the conversion buffer is sized, not after: §14's bound is
        // checked on both sides of the wire, and this is the side that would
        // otherwise allocate first and refuse second.
        let area = (shape.width as usize).checked_mul(shape.height as usize)?;
        if area == 0 || area > MAX_CURSOR_SHAPE_PIXELS {
            return None;
        }
        let kind = shape.kind;
        if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned() {
            return monochrome_cursor(shape);
        }
        let masked = kind
            == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                .0
                .cast_unsigned();
        if !masked && kind != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned() {
            // A shape type this DXGI version does not document. Sending
            // nothing keeps the guest's overlay empty, which is honest;
            // guessing at the layout would draw noise over the picture.
            return None;
        }
        color_cursor(shape, masked)
    }

    /// `COLOR` and `MASKED_COLOR`, both 32bpp with the same geometry.
    fn color_cursor(shape: &PointerShape, masked: bool) -> Option<CursorShapeData> {
        let (width, height) = (shape.width, shape.height);
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * BYTES_PER_PIXEL);
        for row in 0..height {
            let row_start = (row * shape.pitch) as usize;
            for column in 0..width {
                let offset = row_start + (column as usize) * BYTES_PER_PIXEL;
                let source = shape.pixels.get(offset..offset + BYTES_PER_PIXEL)?;
                if masked {
                    // Alpha is an AND mask here, not alpha: clear draws the
                    // colour, set inverts what is behind (see `to_cursor_shape`).
                    let opaque = source[3] == 0;
                    if opaque {
                        pixels.extend_from_slice(&[source[0], source[1], source[2], 0xFF]);
                    } else {
                        pixels.extend_from_slice(&[0, 0, 0, 0xFF]);
                    }
                } else {
                    let alpha = u16::from(source[3]);
                    for channel in source.iter().take(3) {
                        pixels.push(premultiply(*channel, alpha));
                    }
                    pixels.push(source[3]);
                }
            }
        }
        bounded_cursor_shape(
            u16::try_from(width).ok()?,
            u16::try_from(height).ok()?,
            0,
            0,
            pixels,
        )
    }

    /// `MONOCHROME`: a 1bpp AND mask stacked directly above a 1bpp XOR mask,
    /// so the reported height is twice the cursor's.
    ///
    /// Halving the height is the step `composite_monochrome` also takes and
    /// the one that is easiest to lose when this logic is moved; without it
    /// the shape is twice as tall as the cursor and its bottom half is the
    /// XOR mask drawn as if it were pixels.
    fn monochrome_cursor(shape: &PointerShape) -> Option<CursorShapeData> {
        let height = shape.height / 2;
        let width = shape.width;
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * BYTES_PER_PIXEL);
        for row in 0..height {
            let and_row = (row * shape.pitch) as usize;
            let xor_row = ((row + height) * shape.pitch) as usize;
            for column in 0..width {
                let byte_index = (column / 8) as usize;
                let bit = 7 - (column % 8);
                let and_byte = *shape.pixels.get(and_row + byte_index)?;
                let xor_byte = *shape.pixels.get(xor_row + byte_index)?;
                let and_bit = (and_byte >> bit) & 1;
                let xor_bit = (xor_byte >> bit) & 1;
                // Opaque white where the XOR mask paints through a clear AND
                // bit, transparent where the AND bit alone is set, and opaque
                // black for both the plain-black case and the inverting one
                // (see `to_cursor_shape` for why inversion lands here).
                pixels.extend_from_slice(match (and_bit, xor_bit) {
                    (0, 1) => &[0xFF, 0xFF, 0xFF, 0xFF],
                    (1, 0) => &[0, 0, 0, 0],
                    _ => &[0, 0, 0, 0xFF],
                });
            }
        }
        bounded_cursor_shape(
            u16::try_from(width).ok()?,
            u16::try_from(height).ok()?,
            0,
            0,
            pixels,
        )
    }

    /// One straight-alpha channel, premultiplied.
    fn premultiply(channel: u8, alpha: u16) -> u8 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a product of two bytes divided by 255 is itself a byte"
        )]
        let scaled = (u16::from(channel) * alpha / 255) as u8;
        scaled
    }

    /// Extra mouse buttons carried in `MOUSEINPUT.mouseData` alongside
    /// `MOUSEEVENTF_XDOWN`/`MOUSEEVENTF_XUP` (winuser.h; a stable Win32 ABI
    /// constant, not re-exposed under `Win32_UI_Input_KeyboardAndMouse`).
    const XBUTTON1: u32 = 0x0001;
    const XBUTTON2: u32 = 0x0002;

    /// Named keys the guest can send that are not a single character (§9.1's
    /// `logical`, matching `apps/desktop/src/view-window.ts`'s `NAMED_KEYS`
    /// table plus its `0xe100 + N` F-key encoding one to one). Everything
    /// else `Self::key` treats as a Unicode code point.
    fn named_key_vk(logical: u32) -> Option<VIRTUAL_KEY> {
        Some(match logical {
            0x08 => VK_BACK,
            0x09 => VK_TAB,
            0x0d => VK_RETURN,
            0x1b => VK_ESCAPE,
            0x7f => VK_DELETE,
            0xe000 => VK_LEFT,
            0xe001 => VK_UP,
            0xe002 => VK_RIGHT,
            0xe003 => VK_DOWN,
            0xe004 => VK_HOME,
            0xe005 => VK_END,
            0xe006 => VK_PRIOR,
            0xe007 => VK_NEXT,
            0xe008 => VK_INSERT,
            0xe010 => VK_SHIFT,
            0xe011 => VK_CONTROL,
            0xe012 => VK_MENU,
            0xe013 => VK_LWIN,
            0xe014 => VK_CAPITAL,
            0xe101..=0xe118 => VIRTUAL_KEY(VK_F1.0 + u16::try_from(logical - 0xe101).unwrap_or(0)),
            _ => return None,
        })
    }

    /// Input injection through `SendInput` (§11).
    ///
    /// Stateless: every call synthesizes one already-authorized event and
    /// nothing here ever consults a grant (§2.3, §11), matching
    /// `X11Injector`.
    #[derive(Debug, Default)]
    pub struct WindowsInjector {
        _private: (),
    }

    impl WindowsInjector {
        /// Nothing to set up: `SendInput` needs no connection or handle.
        /// Fallible signature only to match `X11Injector::connect` and leave
        /// room for a future capability probe.
        ///
        /// # Errors
        /// Never, today.
        pub const fn connect() -> Result<Self> {
            Ok(Self { _private: () })
        }

        fn send(inputs: &[INPUT]) -> Result<()> {
            let size = i32::try_from(size_of::<INPUT>()).unwrap_or(0);
            // SAFETY: `inputs` is a fully initialized, valid `&[INPUT]` for its
            // own length, exactly what `SendInput` requires; no pointer into it
            // is retained past this call.
            let sent = unsafe { SendInput(inputs, size) };
            if sent as usize != inputs.len() {
                // The Win32 error code is the only thing that tells UIPI
                // rejection (desktop locked, elevated foreground window, a
                // secure desktop from UAC) apart from every other cause; the
                // previous message swallowed it, which made this failure mode
                // indistinguishable from any other in the log (§18).
                let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
                return Err(MediaError::InputUnavailable(format!(
                    "SendInput accepted {sent} of {} synthesized events (GetLastError = {last_error:?}; desktop locked or blocked by UIPI?)",
                    inputs.len()
                )));
            }
            Ok(())
        }

        fn key_vk(vk: VIRTUAL_KEY, pressed: bool) -> Result<()> {
            Self::send(&[INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: if pressed {
                            KEYBD_EVENT_FLAGS::default()
                        } else {
                            KEYEVENTF_KEYUP
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }])
        }

        /// One UTF-16 code unit, sent by value rather than by virtual key: the
        /// guest's `logical` is a Unicode code point, not a layout-dependent
        /// key, and `KEYEVENTF_UNICODE` is exactly the escape hatch Windows
        /// gives for that (matches any layout, any language).
        fn key_unicode(unit: u16, pressed: bool) -> Result<()> {
            Self::send(&[INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: if pressed {
                            KEYEVENTF_UNICODE
                        } else {
                            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }])
        }

        fn key(logical: u32, pressed: bool) -> Result<()> {
            if let Some(vk) = named_key_vk(logical) {
                return Self::key_vk(vk, pressed);
            }
            let ch = char::from_u32(logical).ok_or_else(|| {
                MediaError::InputUnavailable(format!(
                    "logical key {logical} is neither a named key nor a valid code point"
                ))
            })?;
            let mut buf = [0u16; 2];
            for unit in ch.encode_utf16(&mut buf) {
                Self::key_unicode(*unit, pressed)?;
            }
            Ok(())
        }

        fn button(logical: u32, pressed: bool) -> Result<()> {
            let index = logical.saturating_sub(POINTER_BUTTON_LOGICAL_BASE);
            let (flags, mouse_data) = match (index, pressed) {
                (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (0, false) => (MOUSEEVENTF_LEFTUP, 0),
                (1, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (1, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (4, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                _ => {
                    return Err(MediaError::InputUnavailable(format!(
                        "pointer button {index} is not supported"
                    )));
                }
            };
            Self::send(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: mouse_data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }])
        }

        /// `x`/`y` are already normalized to 0..=65535 of the captured surface
        /// (§9.1), which is exactly `MOUSEEVENTF_ABSOLUTE`'s coordinate space
        /// for the primary monitor — the same surface `CaptureTarget::
        /// PrimaryDisplay` captures — so no scaling is needed here.
        fn move_to(x: u16, y: u16) -> Result<()> {
            Self::send(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: i32::from(x),
                        dy: i32::from(y),
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }])
        }

        fn wheel(dx: i16, dy: i16) -> Result<()> {
            let mut inputs = Vec::with_capacity(2);
            for (delta, flags) in [(dy, MOUSEEVENTF_WHEEL), (dx, MOUSEEVENTF_HWHEEL)] {
                if delta == 0 {
                    continue;
                }
                inputs.push(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            // `mouseData` is a `DWORD` in WinAPI but read as a
                            // signed wheel delta; sign-extend through `i32` so
                            // a negative (scroll down/left) delta round-trips
                            // through the same two's-complement bit pattern.
                            mouseData: i32::from(delta).cast_unsigned(),
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
            if inputs.is_empty() {
                return Ok(());
            }
            Self::send(&inputs)
        }
    }

    impl InputInjector for WindowsInjector {
        fn inject(&mut self, event: &InputEventPayload) -> Result<()> {
            match event.detail {
                InputDetail::PointerMove { x, y } => Self::move_to(x, y),
                InputDetail::Wheel { dx, dy } => Self::wheel(dx, dy),
                InputDetail::Press | InputDetail::Release => {
                    let pressed = matches!(event.detail, InputDetail::Press);
                    if event.logical >= POINTER_BUTTON_LOGICAL_BASE {
                        Self::button(event.logical, pressed)
                    } else {
                        Self::key(event.logical, pressed)
                    }
                }
            }
        }

        fn capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(
            clippy::unwrap_used,
            clippy::expect_used,
            reason = "a failed assumption must fail the test"
        )]

        use std::sync::Mutex;

        use super::*;

        /// Windows allows exactly one duplication of a given output per
        /// process: a second `DuplicateOutput` on the same display reports
        /// `E_INVALIDARG` while the first is alive. `cargo test` runs test
        /// functions in parallel threads of one process, so the tests that
        /// actually duplicate the screen take this first, or they would race
        /// each other into a false "no duplicable desktop" skip.
        static ONE_DUPLICATION_AT_A_TIME: Mutex<()> = Mutex::new(());

        /// Named keys map to the virtual key the guest actually means;
        /// anything outside the named table falls through to `key`'s
        /// Unicode path instead (matches `view-window.ts`'s `NAMED_KEYS`).
        #[test]
        fn named_keys_map_to_the_matching_virtual_key() {
            assert_eq!(named_key_vk(0x08), Some(VK_BACK));
            assert_eq!(named_key_vk(0x0d), Some(VK_RETURN));
            assert_eq!(named_key_vk(0xe000), Some(VK_LEFT));
            assert_eq!(named_key_vk(0xe014), Some(VK_CAPITAL));
            // F1 and F24, the ends of the 0xe100+N encoding.
            assert_eq!(named_key_vk(0xe101), Some(VK_F1));
            assert_eq!(named_key_vk(0xe118).map(|vk| vk.0), Some(VK_F1.0 + 23));
            // A plain character code point is not a named key: `key` sends it
            // through `KEYEVENTF_UNICODE` instead.
            assert_eq!(named_key_vk(u32::from(b'a')), None);
        }

        /// The reopen loop's own logic, independent of DXGI: due immediately
        /// on the first attempt, not due again until a full backoff elapses,
        /// and exhausted only once `SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS`
        /// attempts have actually been counted (docs/bugs/11-uac-degradation.md).
        #[test]
        fn recovery_backoff_paces_attempts_and_has_a_bounded_budget() {
            let mut recovery = Recovery::started("secure desktop".to_owned());
            let start = Instant::now();

            // The very first attempt needs no wait.
            assert!(recovery.due(start));

            recovery.schedule_next(start, "still blocked".to_owned());
            assert!(!recovery.due(start));
            assert!(
                recovery.due(start + Duration::from_millis(SECURE_DESKTOP_RECOVERY_BACKOFF_MS))
            );

            for attempt in 1..=SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS {
                let exhausted = recovery.record_attempt();
                assert_eq!(
                    exhausted,
                    attempt > SECURE_DESKTOP_RECOVERY_MAX_ATTEMPTS,
                    "attempt {attempt} should not exhaust the budget early"
                );
            }
            // One more, past the budget, must exhaust it.
            assert!(recovery.record_attempt());
        }

        /// `COLOR` arrives with straight alpha and the wire wants it
        /// premultiplied. Skipping that step is invisible on the opaque
        /// pixels and doubles the brightness of every antialiased edge.
        #[test]
        fn a_colour_cursor_is_premultiplied_on_the_way_out() {
            // Two pixels: opaque red, and half-transparent red.
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 2,
                height: 1,
                pitch: 8,
                pixels: vec![0, 0, 255, 255, 0, 0, 255, 128],
            };
            let converted = to_cursor_shape(&shape).expect("a 2x1 colour cursor is a valid shape");
            assert_eq!((converted.width, converted.height), (2, 1));
            assert_eq!(&converted.rgba[..4], &[0, 0, 255, 255]);
            // 255 * 128 / 255 == 128, and the alpha byte is untouched.
            assert_eq!(&converted.rgba[4..], &[0, 0, 128, 128]);
        }

        /// `MASKED_COLOR` has no alpha at all: the fourth byte is a 1-bit AND
        /// mask, and reading it as alpha would make every cursor drawn this
        /// way almost invisible.
        #[test]
        fn a_masked_colour_cursor_is_opaque_where_the_mask_is_clear() {
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                    .0
                    .cast_unsigned(),
                width: 2,
                height: 1,
                pitch: 8,
                pixels: vec![10, 20, 30, 0, 10, 20, 30, 0xFF],
            };
            let converted = to_cursor_shape(&shape).expect("a 2x1 masked cursor is a valid shape");
            assert_eq!(&converted.rgba[..4], &[10, 20, 30, 0xFF]);
            // The inverting half is rendered opaque rather than dropped.
            assert_eq!(converted.rgba[7], 0xFF);
        }

        /// The monochrome trap: DXGI reports a height that is twice the
        /// cursor's, because the XOR mask is stacked under the AND mask.
        /// Losing the halving gives a shape twice as tall with the mask drawn
        /// into its bottom half.
        #[test]
        fn a_monochrome_cursor_is_half_the_height_dxgi_reports() {
            // 8x2 reported, so a 8x1 cursor. AND row then XOR row, 1bpp with
            // one byte per row.
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned(),
                width: 8,
                height: 2,
                pitch: 1,
                // AND: 0b1100_0000 — the first two pixels are "leave alone".
                // XOR: 0b1010_0000 — pixel 0 inverts, pixel 2 is white.
                pixels: vec![0b1100_0000, 0b1010_0000],
            };
            let converted = to_cursor_shape(&shape).expect("a monochrome cursor is a valid shape");
            assert_eq!((converted.width, converted.height), (8, 1));
            assert_eq!(converted.rgba.len(), 8 * 4);
            // AND=1, XOR=1: the inverting case, rendered opaque black.
            assert_eq!(&converted.rgba[0..4], &[0, 0, 0, 0xFF]);
            // AND=1, XOR=0: transparent.
            assert_eq!(&converted.rgba[4..8], &[0, 0, 0, 0]);
            // AND=0, XOR=1: opaque white.
            assert_eq!(&converted.rgba[8..12], &[0xFF, 0xFF, 0xFF, 0xFF]);
            // AND=0, XOR=0: opaque black.
            assert_eq!(&converted.rgba[12..16], &[0, 0, 0, 0xFF]);
        }

        /// A shape type this DXGI version does not document produces nothing
        /// rather than a guess: the guest's overlay stays empty, which is
        /// honest, instead of drawing noise over the picture.
        #[test]
        fn an_unknown_shape_type_produces_no_cursor_at_all() {
            let shape = PointerShape {
                kind: 0xDEAD_BEEF,
                width: 1,
                height: 1,
                pitch: 4,
                pixels: vec![0, 0, 0, 0],
            };
            assert!(to_cursor_shape(&shape).is_none());
        }

        /// The bound of §14 is checked before the shape leaves this backend,
        /// not only on the way back in.
        #[test]
        fn a_cursor_over_the_pixel_bound_is_refused_before_it_is_sent() {
            let side = 256u32; // 65536 pixels, four times MAX_CURSOR_SHAPE_PIXELS.
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: side,
                height: side,
                pitch: side * 4,
                pixels: vec![0u8; (side * side * 4) as usize],
            };
            assert!(to_cursor_shape(&shape).is_none());
        }

        /// A truncated buffer is a shape to drop, never an index past the end.
        #[test]
        fn a_short_cursor_buffer_is_refused_rather_than_indexed_out_of_bounds() {
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 4,
                height: 4,
                pitch: 16,
                pixels: vec![0u8; 8],
            };
            assert!(to_cursor_shape(&shape).is_none());
        }

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

            // The first frame is the contract this backend exists to keep:
            // whatever the desktop's present clock is doing, a freshly started
            // capturer answers with the screen as it is right now (the GDI
            // snapshot path), never with "nothing changed yet".
            let frame = match capturer.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => panic!("the first frame after start cannot be a duplicate"),
                Err(e) => panic!("capture failed on a live desktop: {e}"),
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

        /// A 4x4 opaque-black frame, tightly packed BGRA8 - the canvas the
        /// cursor-compositing tests below draw onto.
        fn blank_frame() -> Vec<u8> {
            [0, 0, 0, 0xFF].repeat(16)
        }

        fn pixel(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
            let off = ((y * width + x) as usize) * BYTES_PER_PIXEL;
            data[off..off + BYTES_PER_PIXEL].try_into().unwrap()
        }

        /// A fully opaque red pixel, `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR`,
        /// draws its color straight onto the frame.
        #[test]
        fn color_cursor_draws_opaque_pixels() {
            let mut data = blank_frame();
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 1,
                height: 1,
                pitch: 4,
                pixels: vec![0, 0, 0xFF, 0xFF], // BGRA: opaque red
            };
            composite_pointer(&mut data, 4, 4, (1, 1), &shape);
            assert_eq!(pixel(&data, 4, 1, 1), [0, 0, 0xFF, 0xFF]);
            // Untouched neighbor stays the canvas's original black.
            assert_eq!(pixel(&data, 4, 0, 0), [0, 0, 0, 0xFF]);
        }

        /// A zero-alpha `COLOR` pixel is fully transparent: the background
        /// underneath must be left alone, not overwritten with garbage.
        #[test]
        fn color_cursor_skips_transparent_pixels() {
            let mut data = blank_frame();
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 1,
                height: 1,
                pitch: 4,
                pixels: vec![0xFF, 0xFF, 0xFF, 0],
            };
            composite_pointer(&mut data, 4, 4, (2, 2), &shape);
            assert_eq!(pixel(&data, 4, 2, 2), [0, 0, 0, 0xFF]);
        }

        /// `MASKED_COLOR` reuses the alpha byte as a 1-bit AND mask (MSDN):
        /// alpha 0 means draw the color opaque, alpha 0xFF means XOR the
        /// color onto whatever is already there.
        #[test]
        fn masked_color_cursor_replaces_or_xors_by_the_mask_bit() {
            let mut data = blank_frame();
            // Seed a known background so the XOR branch has something to
            // combine with.
            let bg_off = 5 * BYTES_PER_PIXEL; // row 1, col 1 of a 4-wide frame
            data[bg_off..bg_off + 4].copy_from_slice(&[0x0F, 0x0F, 0x0F, 0xFF]);
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                    .0
                    .cast_unsigned(),
                width: 2,
                height: 1,
                pitch: 8,
                pixels: vec![
                    0xF0, 0xF0, 0xF0, 0x00, // col 0: mask clear -> opaque replace
                    0xF0, 0xF0, 0xF0, 0xFF, // col 1: mask set -> XOR
                ],
            };
            composite_pointer(&mut data, 4, 4, (0, 1), &shape);
            assert_eq!(pixel(&data, 4, 0, 1), [0xF0, 0xF0, 0xF0, 0xFF]);
            assert_eq!(pixel(&data, 4, 1, 1), [0xFF, 0xFF, 0xFF, 0xFF]);
        }

        /// `MONOCHROME` stacks a 1bpp AND mask directly above a 1bpp XOR
        /// mask of the same size (MSDN), so all four AND/XOR combinations
        /// have to land on the classic Win32 monochrome-cursor outcomes:
        /// invert, white, transparent, black.
        #[test]
        fn monochrome_cursor_covers_all_four_mask_combinations() {
            let mut data = blank_frame();
            let bg_off = 0usize;
            data[bg_off..bg_off + 4].copy_from_slice(&[0x33, 0x55, 0x77, 0xFF]);
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned(),
                width: 4,
                height: 2, // one real row: AND row then XOR row
                pitch: 1,
                pixels: vec![
                    0b1010_0000, // AND row: col0=1 col1=0 col2=1 col3=0
                    0b1100_0000, // XOR row: col0=1 col1=1 col2=0 col3=0
                ],
            };
            composite_pointer(&mut data, 4, 4, (0, 0), &shape);
            // col0: AND=1,XOR=1 -> invert the seeded background.
            assert_eq!(pixel(&data, 4, 0, 0), [0xCC, 0xAA, 0x88, 0xFF]);
            // col1: AND=0,XOR=1 -> opaque white.
            assert_eq!(pixel(&data, 4, 1, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
            // col2: AND=1,XOR=0 -> transparent, background untouched.
            assert_eq!(pixel(&data, 4, 2, 0), [0, 0, 0, 0xFF]);
            // col3: AND=0,XOR=0 -> opaque black.
            assert_eq!(pixel(&data, 4, 3, 0), [0, 0, 0, 0xFF]);
        }

        /// A cursor partly off the frame's top-left edge (negative position,
        /// routine near a monitor's corner) must clip instead of panicking
        /// or wrapping into an unrelated pixel via a negative-to-unsigned
        /// cast.
        #[test]
        fn cursor_partially_off_the_top_left_edge_is_clipped_not_panicking() {
            let mut data = blank_frame();
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 2,
                height: 2,
                pitch: 8,
                pixels: vec![0xFF; 32], // 2x2 opaque white
            };
            composite_pointer(&mut data, 4, 4, (-1, -1), &shape);
            // Only the in-bounds corner (shape-local (1,1) -> frame (0,0))
            // was drawable.
            assert_eq!(pixel(&data, 4, 0, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
        }

        /// A cursor running off the bottom-right edge must likewise clip
        /// rather than writing past the end of `data`.
        #[test]
        fn cursor_partially_off_the_bottom_right_edge_is_clipped_not_panicking() {
            let mut data = blank_frame();
            let shape = PointerShape {
                kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned(),
                width: 2,
                height: 2,
                pitch: 8,
                pixels: vec![0xFF; 32],
            };
            composite_pointer(&mut data, 4, 4, (3, 3), &shape);
            assert_eq!(pixel(&data, 4, 3, 3), [0xFF, 0xFF, 0xFF, 0xFF]);
        }
    }
}

/// Capture stub for a Windows build without the `capture-windows` feature.
///
/// Kept so `cargo build --workspace` (the CI default path) still compiles this
/// module, needs no Direct3D 11/DXGI bindings, and still exposes
/// `capture::windows::WindowsCapturer` to anything that names it.
#[cfg(not(feature = "capture-windows"))]
mod stub {
    use lumepeer_core::protocol::InputEventPayload;

    use crate::capture::{CaptureTarget, Frame, InputCapability, InputInjector, ScreenCapturer};
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

        /// Not built in: the stub cannot enumerate anything (ADR 0028).
        ///
        /// # Errors
        /// Always [`MediaError::CaptureUnavailable`].
        pub fn attached_monitors_info() -> Result<Vec<crate::capture::HostMonitor>> {
            Err(not_built_in())
        }

        /// Not built in: the stub cannot enumerate anything (ADR 0028).
        ///
        /// # Errors
        /// Always [`MediaError::CaptureUnavailable`].
        pub fn display_count() -> Result<usize> {
            Err(not_built_in())
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

    /// `SendInput` injector, not built in.
    #[derive(Debug, Default)]
    pub struct WindowsInjector {
        _private: (),
    }

    /// Why every call below refuses, in one place.
    fn injection_not_built_in() -> MediaError {
        MediaError::InputUnavailable(
            "windows input injection is not built in: rebuild with the `capture-windows` feature"
                .to_owned(),
        )
    }

    impl WindowsInjector {
        /// Refuses: rebuild with `capture-windows` to get real injection.
        ///
        /// # Errors
        /// Always.
        pub fn connect() -> Result<Self> {
            Err(injection_not_built_in())
        }
    }

    impl InputInjector for WindowsInjector {
        fn inject(&mut self, _event: &InputEventPayload) -> Result<()> {
            Err(injection_not_built_in())
        }

        fn capability(&self) -> InputCapability {
            InputCapability::Full
        }
    }
}
