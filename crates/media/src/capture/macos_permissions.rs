//! The macOS privacy permissions Lumepeer needs, read and asked for in one
//! place, for the app's first-run permissions screen.
//!
//! No installer can switch them on: Screen & System Audio Recording,
//! Accessibility and Microphone are TCC permissions only the person at the
//! Mac grants (or a managed-device profile). Until this module each was asked
//! at the moment a session first needed it — Screen Recording in the middle
//! of a guest's first connection, Accessibility at its first click — so the
//! first session on a new Mac never worked. The app now asks for all three
//! before anyone connects and shows which are still off (ADR 0158).
//!
//! Each question here is a plain query of this process's own TCC state:
//! nothing is cached, so a permission switched on in System Settings shows up
//! at the next read.

#![allow(
    unsafe_code,
    reason = "`AXIsProcessTrusted` and `AVMediaTypeAudio` have no binding in this workspace's objc2 set, and `AVCaptureDevice` is reached through the Objective-C runtime; every block carries a SAFETY note, per §21"
)]

use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, Bool};
use objc2_core_graphics::{
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_foundation::NSString;

/// Where the microphone permission stands, as `AVCaptureDevice` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Microphone {
    /// Never asked: asking shows the system's own prompt.
    NotDetermined,
    /// Refused, or barred by policy: only System Settings can change it now.
    Denied,
    /// Allowed.
    Authorized,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Whether this process holds the Accessibility permission. A plain query
    /// of the calling process's own TCC state, with no arguments.
    safe fn AXIsProcessTrusted() -> bool;
}

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    /// `AVMediaTypeAudio`. Read rather than spelled out, which also links
    /// `AVFoundation`, the framework `AVCaptureDevice` lives in.
    static AVMediaTypeAudio: Option<&'static NSString>;
}

/// `AVAuthorizationStatusNotDetermined`.
const AV_NOT_DETERMINED: isize = 0;
/// `AVAuthorizationStatusAuthorized`. The other two, restricted and denied,
/// are both a refusal only System Settings can undo.
const AV_AUTHORIZED: isize = 3;

/// Whether macOS lets this process record the screen and the system's sound.
#[must_use]
pub fn screen_recording() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Asks for Screen & System Audio Recording. The first time ever this shows
/// the system's prompt; after that it shows nothing, but it is still what
/// puts the app in System Settings' list for the person to switch on.
pub fn request_screen_recording() {
    let _ = CGRequestScreenCaptureAccess();
}

/// Whether macOS lets this process control the Mac: post the guest's input.
///
/// Either grant will do, the same rule the injector applies: Accessibility,
/// which is what System Settings shows, or the narrower post-event grant.
#[must_use]
pub fn accessibility() -> bool {
    AXIsProcessTrusted() || CGPreflightPostEventAccess()
}

/// Asks to control the Mac: the system's "would like to control this
/// computer" prompt, which leads to the Accessibility list.
pub fn request_accessibility() {
    let _ = CGRequestPostEventAccess();
}

/// The `AVCaptureDevice` class and the media type of sound, when `AVFoundation`
/// answers for both.
fn capture_device() -> Option<(&'static AnyClass, &'static NSString)> {
    let class = AnyClass::get(c"AVCaptureDevice")?;
    // SAFETY: a constant AVFoundation exports for the life of the process;
    // read once, never written.
    let audio = unsafe { AVMediaTypeAudio }?;
    Some((class, audio))
}

/// Whether this process may record the microphone.
#[must_use]
pub fn microphone() -> Microphone {
    let Some((class, audio)) = capture_device() else {
        return Microphone::Denied;
    };
    // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` takes an
    // `AVMediaType` (an `NSString`) and returns an `AVAuthorizationStatus`,
    // an `NSInteger`.
    let status: isize = unsafe { msg_send![class, authorizationStatusForMediaType: audio] };
    match status {
        AV_NOT_DETERMINED => Microphone::NotDetermined,
        AV_AUTHORIZED => Microphone::Authorized,
        _ => Microphone::Denied,
    }
}

/// Asks for the microphone: the system's Allow / Don't Allow prompt while
/// the person has never answered it, nothing after that.
pub fn request_microphone() {
    let Some((class, audio)) = capture_device() else {
        return;
    };
    // The answer is read back through `microphone`, which the screen polls.
    let answered = RcBlock::new(|_granted: Bool| {});
    // SAFETY: `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`
    // takes an `AVMediaType` and a `void (^)(BOOL)` block, which it copies
    // before returning.
    unsafe {
        let () = msg_send![class, requestAccessForMediaType: audio, completionHandler: &*answered];
    }
}
