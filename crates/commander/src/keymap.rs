//! Keys of the panels, as in Total Commander. One table, so it can move to a config file later.
//!
//! A key comes as its name from uison (`cx.key` of the `key` action: `f5`, `shift+f6`, `ctrl+r`, typed text
//! as it is). On a Mac, TC's Ctrl is either ⌃ or ⌘: `cmd+r` works like `ctrl+r`.

use crate::panel::SortKey;

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Enter a folder or open a file.
    Open,
    Parent,
    Root,
    /// Alt+←/→: the folders this panel was in.
    Back,
    Forward,
    SwitchPanel,
    /// Insert: toggle and down.
    Toggle,
    /// Space: toggle, count a folder's size, down.
    ToggleSize,
    /// Shift+arrows: toggle and move.
    ToggleUp,
    ToggleDown,
    SelectMask,
    UnselectMask,
    Invert,
    SelectAll,
    UnselectAll,
    View,
    Edit,
    Copy,
    Move,
    CopyHere,
    Rename,
    Mkdir,
    /// Shift+F4: a new (empty) file, then the editor.
    NewFile,
    /// Ctrl+F: storages — remembered connections, a new one.
    Connect,
    Delete,
    /// Alt+F1 / Alt+F2: source and root of the left / right panel.
    PickSource(usize),
    Sort(SortKey),
    Refresh,
    Swap,
    /// Ctrl+←/→: the folder under the cursor (or this folder) in the other panel.
    OpenInOther,
    Properties,
    Menu,
    Hidden,
    /// F1: every key, from the table.
    Help,
    /// The targets' full paths (one per line) / their names into the clipboard.
    CopyPaths,
    CopyNames,
    Quit,
    /// Typed text: quick search.
    Search(String),
}

/// A key binding: the keys (as `cx.key` names them), the command, and what it does in the help (F1).
pub struct Binding {
    pub keys: &'static [&'static str],
    pub cmd: Cmd,
    pub help: &'static str,
    /// The help's group.
    pub group: &'static str,
}

/// All bindings, in the help's order. The one place keys are defined: `command` and F1 both read it.
pub fn table() -> Vec<Binding> {
    let b = |group, keys, cmd, help| Binding { keys, cmd, help, group };
    const NAV: &str = "Moving around";
    const SEL: &str = "Selecting";
    const FILES: &str = "Files";
    const PANELS: &str = "Panels";
    const APP: &str = "The program";
    vec![
        b(NAV, &["up", "down"], Cmd::Up, "Cursor up / down"),
        b(NAV, &["down"], Cmd::Down, ""),
        b(NAV, &["pageup", "pagedown"], Cmd::PageUp, "A page up / down"),
        b(NAV, &["pagedown"], Cmd::PageDown, ""),
        b(NAV, &["home", "end"], Cmd::Home, "First / last row"),
        b(NAV, &["end"], Cmd::End, ""),
        b(NAV, &["enter"], Cmd::Open, "Enter a folder; open a file (the system's app, or the viewer)"),
        b(NAV, &["backspace", "ctrl+pageup"], Cmd::Parent, "Up a level; the cursor lands on the folder left"),
        b(NAV, &["ctrl+\\"], Cmd::Root, "To the root"),
        b(NAV, &["alt+left"], Cmd::Back, "Back to the previous folder"),
        b(NAV, &["alt+right"], Cmd::Forward, "Forward again"),
        b(NAV, &["letters"], Cmd::Search(String::new()), "Quick search by the start of a name; Esc closes it"),
        b(SEL, &["insert"], Cmd::Toggle, "Select / unselect, then down"),
        b(SEL, &["space"], Cmd::ToggleSize, "Select / unselect, then down; on a folder also counts its size"),
        b(SEL, &["shift+up"], Cmd::ToggleUp, "Select while moving up"),
        b(SEL, &["shift+down"], Cmd::ToggleDown, "Select while moving down"),
        b(SEL, &["+", "="], Cmd::SelectMask, "Select by a mask (*.txt)"),
        b(SEL, &["-"], Cmd::UnselectMask, "Unselect by a mask"),
        b(SEL, &["*"], Cmd::Invert, "Invert the selection (files)"),
        b(SEL, &["ctrl+a", "ctrl+="], Cmd::SelectAll, "Select all"),
        b(SEL, &["ctrl+-"], Cmd::UnselectAll, "Unselect all"),
        b(FILES, &["f3"], Cmd::View, "View (text, hex, pictures)"),
        b(FILES, &["f4"], Cmd::Edit, "Edit in the system's text editor"),
        b(FILES, &["shift+f4"], Cmd::NewFile, "New file, then edit it"),
        b(FILES, &["f5"], Cmd::Copy, "Copy to the other panel"),
        b(FILES, &["f6"], Cmd::Move, "Move to the other panel"),
        b(FILES, &["shift+f5"], Cmd::CopyHere, "Copy here, under another name"),
        b(FILES, &["shift+f6", "f2"], Cmd::Rename, "Rename (a mask for several)"),
        b(FILES, &["f7"], Cmd::Mkdir, "New folder (a/b/c makes all levels)"),
        b(FILES, &["f8", "delete", "shift+delete"], Cmd::Delete, "Delete"),
        b(FILES, &["alt+enter"], Cmd::Properties, "Properties: name, permissions, the source's actions"),
        b(FILES, &["shift+f10", "menu"], Cmd::Menu, "The context menu"),
        b(FILES, &["ctrl+alt+c"], Cmd::CopyPaths, "Copy the full paths (⌥⌘C as in Finder)"),
        b(FILES, &["ctrl+shift+c"], Cmd::CopyNames, "Copy the names"),
        b(PANELS, &["tab", "shift+tab"], Cmd::SwitchPanel, "The other panel"),
        b(PANELS, &["alt+f1"], Cmd::PickSource(0), "Source and folder of the left panel"),
        b(PANELS, &["alt+f2"], Cmd::PickSource(1), "Source and folder of the right panel"),
        b(PANELS, &["ctrl+f"], Cmd::Connect, "Connect: a new connection, or the remembered ones"),
        b(PANELS, &["ctrl+f3"], Cmd::Sort(SortKey::Name), "Sort by name (again: reverse)"),
        b(PANELS, &["ctrl+f4"], Cmd::Sort(SortKey::Ext), "Sort by extension"),
        b(PANELS, &["ctrl+f5"], Cmd::Sort(SortKey::Time), "Sort by date"),
        b(PANELS, &["ctrl+f6"], Cmd::Sort(SortKey::Size), "Sort by size"),
        b(PANELS, &["ctrl+r"], Cmd::Refresh, "Read the panel again"),
        b(PANELS, &["ctrl+u"], Cmd::Swap, "Swap the panels"),
        b(PANELS, &["ctrl+left", "ctrl+right"], Cmd::OpenInOther, "The folder under the cursor in the other panel"),
        b(PANELS, &["ctrl+h", "ctrl+."], Cmd::Hidden, "Show / hide hidden files"),
        b(APP, &["f1"], Cmd::Help, "This help"),
        b(APP, &["alt+f4", "cmd+q"], Cmd::Quit, "Quit"),
    ]
}

/// How a key looks in the help: `ctrl+f3` → `Ctrl+F3` (on a Mac `⌃F3`, and ⌘ works as Ctrl too).
pub fn key_title(k: &str) -> String {
    let mac = cfg!(target_os = "macos");
    // `+` itself is a key, not a joint.
    if k == "+" {
        return "+".into();
    }
    k.split('+')
        .filter(|p| !p.is_empty())
        .map(|p| match p {
            "ctrl" if mac => "⌃".to_string(),
            "cmd" => "⌘".to_string(),
            "alt" if mac => "⌥".to_string(),
            "shift" if mac => "⇧".to_string(),
            "ctrl" | "alt" | "shift" => format!("{}{}+", p[..1].to_uppercase(), &p[1..]),
            "pageup" => "PgUp".into(),
            "pagedown" => "PgDn".into(),
            "backspace" => {
                if mac {
                    "⌫".into()
                } else {
                    "Backspace".into()
                }
            }
            "enter" => "Enter".into(),
            "letters" => t!("Letters").into(),
            p if p.len() == 1 => p.to_uppercase(),
            p => format!("{}{}", p[..1].to_uppercase(), &p[1..]),
        })
        .collect::<Vec<_>>()
        .join("")
        .replace("++", "+")
}

pub fn command(key: &str) -> Option<Cmd> {
    // ⌘ as Ctrl, except the keys macOS owns.
    let owned;
    let key = match key.strip_prefix("cmd+") {
        Some("q") => return Some(Cmd::Quit),
        Some("backspace") => return Some(Cmd::Delete),
        Some(rest) if !rest.starts_with("ctrl+") => {
            owned = format!("ctrl+{rest}");
            owned.as_str()
        }
        _ => key,
    };
    // The single key of a binding listed with its pair in the help (`up` of "up / down") is its own command.
    let solo = match key {
        "down" => Some(Cmd::Down),
        "pagedown" => Some(Cmd::PageDown),
        "end" => Some(Cmd::End),
        _ => None,
    };
    if solo.is_some() {
        return solo;
    }
    if let Some(b) = table().into_iter().find(|b| b.keys.contains(&key) && !matches!(b.cmd, Cmd::Search(_))) {
        return Some(b.cmd);
    }
    // Plain typed text: quick search (`+`, `-`, `*`, `=` are taken above).
    (!key.is_empty() && !key.contains('+') && key.chars().count() == 1).then(|| Cmd::Search(key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_keys() {
        assert_eq!(command("f5"), Some(Cmd::Copy));
        assert_eq!(command("shift+f6"), Some(Cmd::Rename));
        assert_eq!(command("ctrl+r"), Some(Cmd::Refresh));
        assert_eq!(command("cmd+r"), Some(Cmd::Refresh), "⌘ works as Ctrl");
        assert_eq!(command("cmd+q"), Some(Cmd::Quit));
        assert_eq!(command("ctrl+f6"), Some(Cmd::Sort(SortKey::Size)));
        assert_eq!(command("a"), Some(Cmd::Search("a".into())));
        assert_eq!(command("Ж"), Some(Cmd::Search("Ж".into())));
        assert_eq!(command("+"), Some(Cmd::SelectMask));
        assert_eq!(command("alt+x"), None);
        assert_eq!(command("down"), Some(Cmd::Down));
        assert_eq!(command("pagedown"), Some(Cmd::PageDown));
        assert_eq!(command("f1"), Some(Cmd::Help));
        assert_eq!(command("ctrl+\\"), Some(Cmd::Root));
        assert_eq!(key_title("shift+f6"), if cfg!(target_os = "macos") { "⇧F6" } else { "Shift+F6" });
        assert_eq!(key_title("+"), "+");
        assert_eq!(command("alt+left"), Some(Cmd::Back));
        assert_eq!(command("shift+f4"), Some(Cmd::NewFile));
        assert_eq!(command("cmd+f"), Some(Cmd::Connect));
    }
}
