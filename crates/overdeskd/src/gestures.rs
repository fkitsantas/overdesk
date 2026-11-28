// SPDX-License-Identifier: AGPL-3.0-only
//! What this computer does when a Mac reports a gesture (a Magic Mouse double-tap or swipe).
//! The Mac only says what the fingers did; the user picks the action here, per gesture.

use std::path::Path;
use std::sync::{OnceLock, PoisonError, RwLock};

use overdesk_net::transport::NetError;
pub use overdesk_platform::input::{Desktop, GestureAction};
use overdesk_proto::{FEATURE_MAGIC_MOUSE, FEATURE_TRACKPAD, Gesture, Os};

/// The settings file in the store folder: `trigger=action` lines, and `seen=<Mac name>`.
pub const GESTURES_FILE: &str = "gestures.txt";

/// What the fingers did, as the settings list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Two fingers double-tapped a Magic Mouse (macOS: Mission Control).
    DoubleTap,
    /// A sideways swipe with the fingers moving left (macOS: the Space to the right).
    SwipeLeft,
    /// A sideways swipe with the fingers moving right (macOS: the Space to the left).
    SwipeRight,
    /// Smart zoom (macOS: a double tap).
    SmartZoom,
}

impl Trigger {
    /// Every trigger, in the order the window lists them.
    pub const ALL: [Self; 4] = [Self::DoubleTap, Self::SwipeLeft, Self::SwipeRight, Self::SmartZoom];

    /// The word saved in the file.
    pub const fn id(self) -> &'static str {
        match self {
            Self::DoubleTap => "double-tap",
            Self::SwipeLeft => "swipe-left",
            Self::SwipeRight => "swipe-right",
            Self::SmartZoom => "smart-zoom",
        }
    }

    /// What the window calls it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::DoubleTap => "Two-finger double-tap",
            Self::SwipeLeft => "Swipe left",
            Self::SwipeRight => "Swipe right",
            Self::SmartZoom => "Smart zoom",
        }
    }

    /// Which trigger `gesture` is; `None` for a pinch or rotation, which Windows replays natively.
    pub const fn of(gesture: Gesture) -> Option<Self> {
        match gesture {
            Gesture::TwoFingerDoubleTap => Some(Self::DoubleTap),
            Gesture::TwoFingerSwipe { right: false } => Some(Self::SwipeLeft),
            Gesture::TwoFingerSwipe { right: true } => Some(Self::SwipeRight),
            Gesture::SmartZoom => Some(Self::SmartZoom),
            Gesture::Magnify { .. } | Gesture::Rotate { .. } => None,
        }
    }

    /// What it does until the user chooses: as macOS Spaces go, fingers moving right reveal the
    /// Space on the left, so swipe right is the previous desktop.
    pub const fn default_action(self) -> GestureAction {
        match self {
            Self::DoubleTap => GestureAction::Overview,
            Self::SwipeLeft => GestureAction::NextDesktop,
            Self::SwipeRight => GestureAction::PreviousDesktop,
            Self::SmartZoom => GestureAction::Nothing,
        }
    }
}

/// The action chosen for each [`Trigger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureMap(pub [GestureAction; 4]);

impl Default for GestureMap {
    fn default() -> Self {
        Self(Trigger::ALL.map(Trigger::default_action))
    }
}

impl GestureMap {
    /// The action for `trigger` on `desktop`: the choice if this desktop has it (a file may come
    /// from another computer), else the default, else nothing.
    pub fn get(&self, trigger: Trigger, desktop: Desktop) -> GestureAction {
        let has = |a: GestureAction| a.chord(desktop).is_some();
        [self.0[trigger as usize], trigger.default_action()]
            .into_iter()
            .find(|a| has(*a))
            .unwrap_or(GestureAction::Nothing)
    }

    /// Chooses `action` for `trigger`.
    pub fn set(&mut self, trigger: Trigger, action: GestureAction) {
        self.0[trigger as usize] = action;
    }
}

/// What is saved: the choices, and the Mac that made the section appear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The action per gesture.
    pub map: GestureMap,
    /// The name of a Mac with a Magic Mouse or trackpad that was connected before, so the
    /// section stays after it leaves.
    pub seen: Option<String>,
}

impl Settings {
    /// Reads settings text; a missing, unknown or malformed line leaves its default.
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            if key == "seen" {
                let name = overdesk_net::session::sanitize_name(value).trim().to_owned();
                s.seen = (!name.is_empty()).then_some(name);
            } else if let (Some(t), Some(a)) = (
                Trigger::ALL.into_iter().find(|t| t.id() == key),
                GestureAction::from_id(value),
            ) {
                s.map.set(t, a);
            }
        }
        s
    }

    /// The text of the file.
    pub fn render(&self) -> String {
        let mut text: String = Trigger::ALL
            .iter()
            .map(|t| format!("{}={}\n", t.id(), self.map.0[*t as usize].id()))
            .collect();
        if let Some(name) = &self.seen {
            text += &format!("seen={name}\n");
        }
        text
    }

    /// The saved settings; the defaults if there are none.
    pub fn load(dir: &Path) -> Self {
        Self::parse(&std::fs::read_to_string(dir.join(GESTURES_FILE)).unwrap_or_default())
    }

    /// Saves them, atomically, and makes them the live choices at once.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] if the file cannot be written; the choices still apply until the app quits.
    pub fn save(&self, dir: &Path) -> Result<(), NetError> {
        set_live(self.map);
        overdesk_net::store::write_private(&dir.join(GESTURES_FILE), self.render().as_bytes())
    }
}

/// The choices the injector follows; read per gesture, so a change applies without reconnecting.
static LIVE: RwLock<GestureMap> = RwLock::new(GestureMap([
    GestureAction::Overview,
    GestureAction::NextDesktop,
    GestureAction::PreviousDesktop,
    GestureAction::Nothing,
]));

/// Makes `map` the choices the injector follows.
pub fn set_live(map: GestureMap) {
    *LIVE.write().unwrap_or_else(PoisonError::into_inner) = map;
}

/// The kind of desktop this computer runs; read once.
pub fn desktop() -> Desktop {
    static HERE: OnceLock<Desktop> = OnceLock::new();
    *HERE.get_or_init(|| match crate::this_os() {
        Os::Windows => Desktop::Windows,
        Os::MacOs => Desktop::Mac,
        _ => Desktop::from_desktop(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()),
    })
}

/// Whether this computer lets the user choose the actions. A Mac does not: it types its own
/// system shortcuts for a Mac's gestures, as if the mouse were attached to it.
pub fn has_picker() -> bool {
    desktop() != Desktop::Mac
}

/// The keys to type for `gesture` under the live choices; `None` if it is not a mapped gesture
/// (a pinch, or smart zoom on a Mac, which are replayed as real gestures).
pub fn chord_for(gesture: Gesture) -> Option<&'static [u32]> {
    let trigger = Trigger::of(gesture)?;
    let desktop = desktop();
    if desktop == Desktop::Mac && trigger == Trigger::SmartZoom {
        return None;
    }
    let map = *LIVE.read().unwrap_or_else(PoisonError::into_inner);
    Some(map.get(trigger, desktop).chord(desktop).unwrap_or_default())
}

/// Whether a peer with `os` and `features` is a Mac that reports a Magic Mouse or a trackpad.
pub fn reports_pointing(os: Os, features: u64) -> bool {
    os == Os::MacOs && features & (FEATURE_MAGIC_MOUSE | FEATURE_TRACKPAD) != 0
}

/// The Mac the "Gestures from ..." section is about, or `None` to hide it: only where gestures
/// can be acted on (`receiver`), for a connected Mac that reports a pointing device or, after it
/// left, the one remembered.
pub fn section_mac(receiver: bool, connected: Option<&str>, seen: Option<&str>) -> Option<String> {
    receiver.then(|| connected.or(seen)).flatten().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_unknown_lines_keep_their_defaults() {
        let mut s = Settings::default();
        s.map.set(Trigger::DoubleTap, GestureAction::ShowDesktop);
        s.map.set(Trigger::SmartZoom, GestureAction::Back);
        s.seen = Some("MacBook".into());
        assert_eq!(Settings::parse(&s.render()), s);
        let odd = Settings::parse("double-tap=launch-missiles\nswipe-left=back\nnonsense\nfuture=1\nseen=\n=x\n");
        assert_eq!(
            odd.map.get(Trigger::DoubleTap, Desktop::Windows),
            GestureAction::Overview
        );
        assert_eq!(odd.map.get(Trigger::SwipeLeft, Desktop::Windows), GestureAction::Back);
        assert_eq!(odd.seen, None);
        assert_eq!(Settings::parse(""), Settings::default());
    }

    #[test]
    fn settings_save_to_the_store_and_apply_live() {
        let dir = std::env::temp_dir().join(format!("overdesk-gestures-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Settings::default();
        s.map.set(Trigger::SwipeRight, GestureAction::Forward);
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        set_live(s.map);
        // The chord follows the live choice on whatever desktop runs the test.
        let want = s.map.get(Trigger::SwipeRight, desktop()).chord(desktop());
        assert_eq!(chord_for(Gesture::TwoFingerSwipe { right: true }), want);
        assert_eq!(Settings::load(&dir.join("missing")), Settings::default());
        set_live(GestureMap::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn defaults_follow_macos_spaces_and_pinches_are_not_mapped() {
        let d = GestureMap::default();
        let w = Desktop::Windows;
        assert_eq!(d.get(Trigger::DoubleTap, w), GestureAction::Overview);
        assert_eq!(d.get(Trigger::SwipeRight, w), GestureAction::PreviousDesktop);
        assert_eq!(d.get(Trigger::SwipeLeft, w), GestureAction::NextDesktop);
        assert_eq!(d.get(Trigger::SmartZoom, w), GestureAction::Nothing);
        // A Mac types its own Mission Control and Space shortcuts; an unknown Linux desktop has none.
        assert_eq!(
            d.get(Trigger::DoubleTap, Desktop::Mac).chord(Desktop::Mac),
            Some(&[0xE0, 0x52][..])
        );
        assert_eq!(d.get(Trigger::DoubleTap, Desktop::Other), GestureAction::Nothing);
        // A choice this desktop lacks falls back to the default.
        let mut m = GestureMap::default();
        m.set(Trigger::SwipeLeft, GestureAction::Minimize);
        assert_eq!(m.get(Trigger::SwipeLeft, Desktop::Gnome), GestureAction::NextDesktop);
        assert_eq!(m.get(Trigger::SwipeLeft, Desktop::Windows), GestureAction::Minimize);
        assert_eq!(Trigger::of(Gesture::Magnify { milli: 5 }), None);
        for (i, t) in Trigger::ALL.into_iter().enumerate() {
            assert_eq!(t as usize, i, "the map is indexed by trigger");
        }
    }

    #[test]
    fn the_section_shows_for_a_mac_with_a_pointing_device_and_is_remembered() {
        assert!(reports_pointing(Os::MacOs, FEATURE_MAGIC_MOUSE));
        assert!(!reports_pointing(Os::MacOs, 0));
        assert!(!reports_pointing(Os::Windows, FEATURE_TRACKPAD));
        assert_eq!(section_mac(true, Some("Mac"), None).as_deref(), Some("Mac"));
        assert_eq!(section_mac(true, None, Some("Old Mac")).as_deref(), Some("Old Mac"));
        assert_eq!(section_mac(true, Some("New"), Some("Old")).as_deref(), Some("New"));
        assert_eq!(section_mac(true, None, None), None);
        assert_eq!(
            section_mac(false, Some("Mac"), Some("Mac")),
            None,
            "this OS cannot act on them"
        );
    }
}
