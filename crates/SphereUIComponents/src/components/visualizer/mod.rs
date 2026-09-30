//! Window > Visualizer: floating meter windows fed by the master bus.
//!
//! The engine copies the master bus into a lock-free tap while a visualizer
//! is open ([`DirectAudio::visualizer_tap`]); each window pulls
//! from it on the UI thread, analyses what arrived ([`analysis`]), turns the
//! result into triangles ([`scene`]) that wgpu draws off screen ([`gpu`]), and
//! composites that image inside a GPUI picture-in-picture shell ([`window`])
//! with the labels and readouts drawn by GPUI on top.

pub mod analysis;
pub mod gpu;
pub mod scene;
pub mod window;

use analysis::Features;

/// The views, one menu entry each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisualizerKind {
    Spectrum,
    StereoImage,
    Loudness,
    Oscilloscope,
    Spectrogram,
}

impl VisualizerKind {
    pub const ALL: [Self; 5] = [
        Self::Spectrum,
        Self::StereoImage,
        Self::Loudness,
        Self::Oscilloscope,
        Self::Spectrogram,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Spectrum => "Spectrum",
            Self::StereoImage => "Stereo Image",
            Self::Loudness => "Loudness",
            Self::Oscilloscope => "Oscilloscope",
            Self::Spectrogram => "Spectrogram",
        }
    }

    /// The command that opens this view (`packages/shared/src/menu/menuItems.ts`).
    pub fn command(self) -> &'static str {
        match self {
            Self::Spectrum => "window:visualizer-spectrum",
            Self::StereoImage => "window:visualizer-stereo-image",
            Self::Loudness => "window:visualizer-loudness",
            Self::Oscilloscope => "window:visualizer-oscilloscope",
            Self::Spectrogram => "window:visualizer-spectrogram",
        }
    }

    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.command() == command)
    }

    /// What the analyzer has to compute for this view.
    pub fn features(self) -> Features {
        match self {
            Self::Spectrum | Self::Spectrogram => Features {
                spectrum: true,
                loudness: false,
            },
            Self::Loudness => Features {
                spectrum: false,
                loudness: true,
            },
            Self::StereoImage | Self::Oscilloscope => Features::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_round_trips_through_its_command() {
        for kind in VisualizerKind::ALL {
            assert_eq!(VisualizerKind::from_command(kind.command()), Some(kind));
            assert!(kind.command().starts_with("window:visualizer-"));
        }
        assert_eq!(VisualizerKind::from_command("window:big-clock"), None);
    }

    /// The menu is the command registry: a view with no menu entry is a view
    /// nobody can open.
    #[test]
    fn every_view_is_in_the_window_menu() {
        let window = crate::menu::MenuManifest::load()
            .menus
            .iter()
            .find(|menu| menu.id == "window")
            .expect("a Window menu");
        let mut commands = Vec::new();
        fn walk(items: &[crate::menu::MenuItem], out: &mut Vec<String>) {
            for item in items {
                if let Some(command) = item.command.as_ref() {
                    out.push(command.clone());
                }
                walk(&item.children, out);
            }
        }
        walk(&window.items, &mut commands);
        for kind in VisualizerKind::ALL {
            assert!(
                commands.iter().any(|command| command == kind.command()),
                "{} is not in Window > Visualizer",
                kind.command()
            );
        }
    }
}
