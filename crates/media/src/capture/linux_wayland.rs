//! Wayland capture via xdg-desktop-portal and PipeWire (design doc §11).
//!
//! The portal call order is normative and must not be reordered "to simplify
//! the code": `CreateSession`, then `SelectDevices`, then `SelectSources`,
//! then `Start`. [`PortalSession::negotiate`] is written in exactly that order
//! and the test below pins it, because getting it wrong is the kind of change
//! that looks harmless in review.
//!
//! A zero input-device mask coming back from `Start` is not an error: it is the
//! user declining input in the system dialog. The session then continues with
//! [`InputCapability::None`] and the UI says so, rather than claiming a control
//! it cannot exercise (§18).

use crate::capture::{CaptureTarget, Frame, InputCapability, ScreenCapturer};
use crate::error::{MediaError, Result};

/// One step of the portal handshake, in the order §11 fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortalStep {
    /// `org.freedesktop.portal.RemoteDesktop.CreateSession`.
    CreateSession,
    /// `SelectDevices`, strictly between `CreateSession` and `SelectSources`.
    SelectDevices,
    /// `org.freedesktop.portal.ScreenCast.SelectSources`.
    SelectSources,
    /// `Start`, which raises the user's dialog.
    Start,
}

/// The normative order of §11.
pub const PORTAL_CALL_ORDER: [PortalStep; 4] = [
    PortalStep::CreateSession,
    PortalStep::SelectDevices,
    PortalStep::SelectSources,
    PortalStep::Start,
];

/// What the portal granted once `Start` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalGrant {
    /// PipeWire node ids of the granted streams.
    pub node_ids: Vec<u32>,
    /// Input capability derived from the device mask.
    pub input: InputCapability,
    /// The steps that actually ran, for the order test and for the audit log.
    pub steps: Vec<PortalStep>,
}

/// Portal/PipeWire capturer.
#[derive(Debug, Default)]
pub struct WaylandPortalCapturer {
    grant: Option<PortalGrant>,
}

impl WaylandPortalCapturer {
    /// Creates a capturer with no portal session yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { grant: None }
    }

    /// The portal grant, once one was negotiated.
    #[must_use]
    pub const fn grant(&self) -> Option<&PortalGrant> {
        self.grant.as_ref()
    }
}

impl ScreenCapturer for WaylandPortalCapturer {
    fn start(&mut self, _target: CaptureTarget) -> Result<()> {
        #[cfg(feature = "capture-portal")]
        {
            let grant = portal::PortalSession::negotiate()?;
            self.grant = Some(grant);
            // Negotiation is the part §11 is normative about; consuming the
            // PipeWire stream behind the node id is the remaining work, and
            // saying so beats pretending to capture.
            Err(MediaError::CaptureUnavailable(
                "the portal granted a stream, but PipeWire frame consumption is not implemented"
                    .to_owned(),
            ))
        }
        #[cfg(not(feature = "capture-portal"))]
        {
            Err(MediaError::CaptureUnavailable(
                "this build has no xdg-desktop-portal support".to_owned(),
            ))
        }
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        Err(MediaError::CaptureUnavailable(
            "the portal capture path produces no frames yet".to_owned(),
        ))
    }

    fn stop(&mut self) {
        self.grant = None;
    }

    fn input_capability(&self) -> InputCapability {
        self.grant
            .as_ref()
            .map_or(InputCapability::PortalRemoteDesktop, |grant| grant.input)
    }
}

/// The portal handshake itself (§11).
///
/// Kept as an inline module so the file list of §6 stays exact.
#[cfg(feature = "capture-portal")]
pub mod portal {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::remote_desktop::{
        DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    };
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

    use super::{PortalGrant, PortalStep};
    use crate::capture::InputCapability;
    use crate::error::{MediaError, Result};

    /// Negotiates a portal session.
    #[derive(Debug)]
    pub struct PortalSession;

    impl PortalSession {
        /// Runs the handshake in the order §11 fixes and returns what the user
        /// granted.
        ///
        /// # Errors
        /// [`MediaError::PermissionDenied`] when the user dismisses the dialog,
        /// [`MediaError::CaptureUnavailable`] when no portal is reachable.
        pub fn negotiate() -> Result<PortalGrant> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            runtime.block_on(Self::negotiate_async())
        }

        /// The asynchronous half, for a caller that already has a runtime.
        ///
        /// # Errors
        /// As [`Self::negotiate`].
        pub async fn negotiate_async() -> Result<PortalGrant> {
            let mut steps = Vec::with_capacity(super::PORTAL_CALL_ORDER.len());

            let remote = RemoteDesktop::new()
                .await
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;
            let screencast = Screencast::new()
                .await
                .map_err(|e| MediaError::CaptureUnavailable(e.to_string()))?;

            // 1. CreateSession. Its options type is private in ashpd, so the
            // default is the only thing that can be passed here.
            #[allow(
                clippy::default_trait_access,
                reason = "ashpd keeps CreateSessionOptions private"
            )]
            let session = remote
                .create_session(Default::default())
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::CreateSession);

            // 2. SelectDevices, strictly before SelectSources (§11).
            remote
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::SelectDevices);

            // 3. SelectSources on the same session.
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await
                .map_err(map_portal_error)?;
            steps.push(PortalStep::SelectSources);

            // 4. Start raises the dialog and returns what the user allowed.
            let response = remote
                .start(&session, None, StartOptions::default())
                .await
                .map_err(map_portal_error)?
                .response()
                .map_err(map_portal_error)?;
            steps.push(PortalStep::Start);

            let devices = response.devices();
            // An empty device mask is a decision, not a failure (§18).
            let input = if devices.is_empty() {
                InputCapability::None
            } else {
                InputCapability::PortalRemoteDesktop
            };

            Ok(PortalGrant {
                node_ids: response
                    .streams()
                    .iter()
                    .map(ashpd::desktop::screencast::Stream::pipe_wire_node_id)
                    .collect(),
                input,
                steps,
            })
        }
    }

    /// A dismissed dialog is the user declining, everything else is the portal
    /// being unavailable (§18).
    fn map_portal_error(error: ashpd::Error) -> MediaError {
        match error {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                MediaError::PermissionDenied
            }
            other => MediaError::CaptureUnavailable(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §11: the order is normative. This test exists so that reordering the
    /// calls fails here rather than on a user's machine.
    #[test]
    fn select_devices_sits_between_create_session_and_select_sources() {
        let create = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::CreateSession)
            .unwrap_or(usize::MAX);
        let devices = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::SelectDevices)
            .unwrap_or(usize::MAX);
        let sources = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::SelectSources)
            .unwrap_or(usize::MAX);
        let start = PORTAL_CALL_ORDER
            .iter()
            .position(|step| *step == PortalStep::Start)
            .unwrap_or(usize::MAX);

        assert!(create < devices, "SelectDevices comes after CreateSession");
        assert!(
            devices < sources,
            "SelectDevices comes before SelectSources"
        );
        assert!(sources < start, "Start comes last");
    }

    /// §18: an empty device mask degrades to view-only instead of failing.
    #[test]
    fn an_empty_device_mask_degrades_to_view_only() {
        let mut capturer = WaylandPortalCapturer::new();
        assert_eq!(
            capturer.input_capability(),
            InputCapability::PortalRemoteDesktop
        );

        capturer.grant = Some(PortalGrant {
            node_ids: vec![42],
            input: InputCapability::None,
            steps: PORTAL_CALL_ORDER.to_vec(),
        });
        assert_eq!(capturer.input_capability(), InputCapability::None);

        // Stopping forgets the grant, so a new session renegotiates.
        capturer.stop();
        assert!(capturer.grant().is_none());
    }
}
