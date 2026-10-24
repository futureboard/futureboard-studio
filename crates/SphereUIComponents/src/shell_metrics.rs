//! Measured heights of the application shell's top chrome.
//!
//! These are **coordinate-space constants**, not styling. The timeline converts
//! window-space Y into arrangement-local Y by subtracting [`APP_CHROME_HEIGHT`],
//! so ruler ticks, clip hit-testing, note drags, automation points and the
//! tempo track all depend on this number being exactly the drawn height of the
//! chrome above them.
//!
//! It used to be a bare `const APP_CHROME_HEIGHT: f32 = 36.0` copy-pasted into
//! three separate timeline files, each with a comment saying it "mirrors" the
//! others. Splitting the chrome into a titlebar plus a transport bar would have
//! silently desynchronised all three — the arrangement would still *draw*
//! correctly while every click landed one bar too high. One definition now.

/// Drawn height of the titlebar band, as the timeline's coordinate math has
/// always measured it.
///
/// Deliberately kept at the historical value rather than recomputed from
/// `platform_chrome::TITLEBAR_HEIGHT_PX` (32) plus its 1 px bottom border: the
/// extra was tuned by hand against the running app, and changing it shifts
/// every arrangement hit-test by a few pixels. If it is wrong it should be
/// fixed deliberately, with the timeline re-verified — not as a side effect.
pub const TITLEBAR_BAND_HEIGHT: f32 = 36.0;

/// Drawn height of the transport bar that sits directly under the titlebar,
/// including its 1 px bottom border.
///
/// Sized for the readout panel plus 6 px above and below.
///
/// The timeline measures its real origin each frame, so this only moves the
/// first-frame fallback of [`APP_CHROME_HEIGHT`], not steady-state hit-tests.
pub const TRANSPORT_BAR_HEIGHT: f32 = 48.0;

/// Total height of everything above the arrangement's top edge.
pub const APP_CHROME_HEIGHT: f32 = TITLEBAR_BAND_HEIGHT + TRANSPORT_BAR_HEIGHT;

/// Height of the interactive control band inside the transport bar.
pub const TRANSPORT_CONTROL_HEIGHT: f32 = 32.0;

/// Height of the recessed readout panel in the middle of the transport bar.
pub const TRANSPORT_READOUT_HEIGHT: f32 = 36.0;

/// Square size of the transport and mode buttons.
pub const TRANSPORT_BUTTON_SIZE: f32 = 30.0;
