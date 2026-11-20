//! Single source of truth for key bindings shown to the user: the help
//! overlay and the footer hints are both generated from this table, so they
//! can't drift from each other.

pub struct Binding {
    pub keys: &'static str,
    pub action: &'static str,
}

pub struct Section {
    pub title: &'static str,
    pub bindings: &'static [Binding],
}

const fn b(keys: &'static str, action: &'static str) -> Binding {
    Binding { keys, action }
}

pub const HELP: &[Section] = &[
    Section {
        title: "Browse",
        bindings: &[
            b("j k  ↓ ↑", "move cursor"),
            b("h l  ← →", "previous / next pane"),
            b("Tab", "next pane"),
            b("g G", "first / last item"),
            b("PgUp PgDn  ^u ^d", "page up / down"),
            b("Enter", "open / play from here"),
            b("o", "jump to the playing track"),
        ],
    },
    Section {
        title: "Search",
        bindings: &[
            b("/", "search the focused pane"),
            b("Tab", "switch artist / album / song"),
            b("Enter", "keep results"),
            b("Esc", "cancel (in normal mode: clear filter)"),
        ],
    },
    Section {
        title: "Playback",
        bindings: &[
            b("Space", "play / pause"),
            b("n p", "next / previous track"),
            b(", .", "seek -/+ 10 s"),
            b("- +", "volume down / up"),
            b("s", "shuffle"),
            b("r", "repeat: off → all → one"),
        ],
    },
    Section { title: "App", bindings: &[b("?", "toggle this help"), b("q  ^c", "quit")] },
];

/// Compact hints for the footer, in priority order: the footer drops hints
/// from the end when the terminal is too narrow to show them all.
pub const FOOTER: &[(&str, &str)] = &[
    ("?", "help"),
    ("/", "search"),
    ("space", "play/pause"),
    ("n/p", "next/prev"),
    ("h/l", "panes"),
    (",/.", "seek"),
    ("-/+", "vol"),
    ("s", "shuffle"),
    ("r", "repeat"),
    ("q", "quit"),
];
