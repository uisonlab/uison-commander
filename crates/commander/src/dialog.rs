//! Dialogs: what is asked and what the answer does. One dialog at a time; the screen shows it from `dlg.*`.

use ops::{Answer, Mode};
use uison::ui::{Data, Json, obj};
use vfs::{ActionInfo, Param, VPath};

/// What a button (or Enter) does.
#[derive(Clone, Debug)]
pub enum Act {
    Close,
    /// Copy / move the panel's targets to the typed path.
    Transfer {
        mode: Mode,
        side: usize,
        names: Vec<String>,
        /// Shift+F5 / Shift+F6: the target is in this panel, whatever the typed path looks like.
        here: bool,
    },
    Mkdir {
        side: usize,
    },
    /// Shift+F4: an empty file with the typed name, then the editor.
    NewFile {
        side: usize,
    },
    Mask {
        side: usize,
        select: bool,
    },
    Delete {
        side: usize,
        names: Vec<String>,
    },
    /// An answer to a running operation.
    Answer(Answer),
    /// "Rename…" in a conflict: ask the new name, then answer with it.
    AskRename,
    /// The new name of a conflict is typed.
    RenameTo,
    /// Pick a root or a source from the list (Alt+F1/F2).
    PickRoot {
        side: usize,
    },
    /// A source's extra action, with its parameters from the dialog.
    RunAction {
        side: usize,
        action: ActionInfo,
        targets: Vec<VPath>,
    },
    /// Open the parameters of a source action (from Properties).
    ActionParams {
        side: usize,
        index: usize,
    },
    ViewSearch,
    /// Connect with what the dialog says (connector `index` of `connect::all()`); it opens in this panel.
    /// The dialog stays until it works, and shows why not. `test` — only see that it works.
    /// `edit` — the remembered connection being changed (its secrets come from the Keychain if left empty).
    Connect {
        side: usize,
        index: usize,
        edit: Option<usize>,
        test: bool,
    },
    /// Properties: rename the entry and change its permissions to what the dialog shows.
    ApplyProps {
        side: usize,
        name: String,
        mode: Option<u32>,
    },
    /// Quit although an operation runs: cancel it first.
    QuitNow,
    /// Yes to the question of a connection (trust a server's key): connect again with it.
    ConfirmConnect,
}

/// How a form's line above the buttons looks: waiting, failed, worked, or asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Busy,
    Error,
    Ok,
    Ask,
}

/// Permission checkboxes: (data key, bit), owner / group / others × read / write / execute.
pub const PERMS: [(&str, u32); 9] =
    [("ur", 0o400), ("uw", 0o200), ("ux", 0o100), ("gr", 0o040), ("gw", 0o020), ("gx", 0o010), ("or", 0o004), ("ow", 0o002), ("ox", 0o001)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Message,
    Input,
    List,
    Params,
    Props,
    /// Help → About: a narrow card of its own (the screen knows its text).
    About,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::Input => "input",
            Kind::List => "list",
            Kind::Params => "params",
            Kind::Props => "props",
            Kind::About => "about",
        }
    }
}

#[derive(Clone)]
pub struct Button {
    pub label: String,
    pub variant: &'static str,
    pub act: Act,
    /// At the left edge of the row, apart from the others (Test of a connection form).
    pub left: bool,
}

impl Button {
    pub fn new(label: &str, variant: &'static str, act: Act) -> Button {
        Button { label: label.into(), variant, act, left: false }
    }

    pub fn left(self) -> Button {
        Button { left: true, ..self }
    }

    pub fn cancel() -> Button {
        Button::new(t!("Cancel"), "secondary", Act::Close)
    }
}

/// A list row (roots and sources): what it opens.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub label: String,
    pub note: String,
    pub source: usize,
    pub path: VPath,
    /// A remembered connection, not connected yet (index into the app's profiles).
    pub profile: Option<usize>,
    /// A group title (a source's name): shown, not chosen.
    pub header: bool,
}

#[derive(Clone)]
pub struct Dialog {
    pub kind: Kind,
    pub title: String,
    pub text: String,
    /// The input field's starting value.
    pub value: String,
    /// The first button is the default (Enter), the last one is Escape.
    pub buttons: Vec<Button>,
    pub items: Vec<ListItem>,
    pub sel: usize,
    pub params: Vec<Param>,
    pub props: Vec<(String, String)>,
    /// Properties: the name to edit (`None` — the source can't rename).
    pub name: Option<String>,
    /// Properties: the permissions to edit (`None` — the source can't change them).
    pub mode: Option<u32>,
    /// A question of an operation: closing it must answer.
    pub question: bool,
    /// Parameters: grey text in an empty field (id, hint).
    pub hints: Vec<(String, String)>,
    /// Parameters that are a file on this machine: a "Choose…" button by the field.
    pub files: Vec<String>,
    /// A form's line above the buttons (connecting, why it failed).
    pub status: Option<(Status, String)>,
    /// What was typed before a question of an operation covered the dialog: put back when it returns.
    pub typed: Option<vfs::ActionArgs>,
    /// A list shown as a drop-down under this panel's title (Alt+F1/F2), not as a window: one click picks.
    pub drop: Option<usize>,
    /// The user moved through the list: the row chosen is theirs, not the panel's.
    pub moved: bool,
    /// Wider than usual: long rows (the keys) — or many buttons, which make it so by themselves.
    pub wide: bool,
    /// The connections: the list on the left, its own buttons on the right (Add, Edit, Delete, Close);
    /// `buttons` stay for Enter (connect) and Escape.
    pub side: bool,
}

impl Dialog {
    pub fn new(kind: Kind, title: &str, text: &str) -> Dialog {
        Dialog {
            kind,
            title: title.into(),
            text: text.into(),
            value: String::new(),
            buttons: Vec::new(),
            items: Vec::new(),
            sel: 0,
            params: Vec::new(),
            props: Vec::new(),
            name: None,
            mode: None,
            question: false,
            hints: Vec::new(),
            files: Vec::new(),
            status: None,
            typed: None,
            drop: None,
            moved: false,
            wide: false,
            side: false,
        }
    }

    pub fn message(title: &str, text: &str) -> Dialog {
        Dialog::new(Kind::Message, title, text).buttons(vec![Button::new(t!("OK"), "primary", Act::Close)])
    }

    pub fn input(title: &str, text: &str, value: &str, ok: &str, act: Act) -> Dialog {
        let mut d = Dialog::new(Kind::Input, title, text).buttons(vec![Button::new(ok, "primary", act), Button::cancel()]);
        d.value = value.into();
        d
    }

    pub fn buttons(mut self, b: Vec<Button>) -> Dialog {
        self.buttons = b;
        self
    }

    pub fn write(&self, d: &mut Data) {
        d.set("dlg.open", self.drop.is_none() && self.kind != Kind::About);
        d.set("dlg.about", self.drop.is_none() && self.kind == Kind::About);
        d.set("drop.side", drop_side(self.drop));
        d.set("dlg.kind", self.kind.name());
        // A conflict's seven buttons, the connections' six don't fit the usual width.
        d.set("dlg.wide", self.wide || self.buttons.len() > 4);
        d.set("dlg.side", self.side);
        d.set("dlg.none", !self.items.iter().any(|i| !i.header));
        d.set("dlg.title", self.title.as_str());
        d.set("dlg.text", self.text.as_str());
        d.set("dlg.value", self.value.as_str());
        d.set("dlg.sel", self.sel as f64);
        self.write_buttons(d);
        self.write_status(d);
        let items: Vec<Json> = self
            .items
            .iter()
            .map(|i| obj([("label", Json::from(i.label.clone())), ("note", Json::from(i.note.clone())), ("h", Json::from(i.header))]))
            .collect();
        // The list is as tall as its rows, up to a limit; then it scrolls.
        let h: f64 = self.items.iter().map(|i| if i.header { 24.0 } else { 27.0 }).sum();
        // Beside the buttons: at least as tall as they are.
        d.set("dlg.listH", (h + 4.0).min(520.0).max(if self.side { 190.0 } else { 0.0 }));
        d.set("dlg.items", Json::Array(items));
        let params: Vec<Json> = self
            .params
            .iter()
            .map(|p| {
                let hint = self.hints.iter().find(|(id, _)| id == p.id()).map(|(_, h)| t!(h).to_string()).unwrap_or_default();
                let mut j = param_json(p);
                if let Json::Object(m) = &mut j {
                    if let Some(v) = self.typed.as_ref().and_then(|t| t.get(p.id())) {
                        let v = if matches!(p, Param::Flag { .. }) { Json::from(v == "true") } else { Json::from(v.to_string()) };
                        m.retain(|(k, _)| k != "value");
                        m.push(("value".into(), v));
                    }
                    m.push(("hint".into(), Json::from(hint)));
                    m.push(("browse".into(), Json::from(cfg!(target_os = "macos") && self.files.iter().any(|f| f == p.id()))));
                }
                j
            })
            .collect();
        d.set("dlg.params", Json::Array(params));
        let props: Vec<Json> = self.props.iter().map(|(k, v)| obj([("k", Json::from(k.clone())), ("v", Json::from(v.clone()))])).collect();
        d.set("dlg.props", Json::Array(props));
        d.set("dlg.nameOn", self.name.is_some());
        d.set("dlg.name", self.name.clone().unwrap_or_default());
        d.set("dlg.permOn", self.mode.is_some());
        let mode = self.mode.unwrap_or(0);
        d.set("dlg.octal", octal(mode));
        set_ticks(d, mode);
    }

    /// The buttons at the left edge and the rest at the right, each with its index in `buttons`.
    pub fn write_buttons(&self, d: &mut Data) {
        let side = |left: bool| -> Json {
            let row = self.buttons.iter().enumerate().filter(|(_, b)| b.left == left);
            Json::Array(
                row.map(|(i, b)| obj([("label", Json::from(b.label.clone())), ("variant", Json::from(b.variant)), ("i", Json::from(i as f64))]))
                    .collect(),
            )
        };
        d.set("dlg.buttonsL", side(true));
        d.set("dlg.buttons", side(false));
    }

    pub fn write_status(&self, d: &mut Data) {
        let (kind, text) = match &self.status {
            Some((Status::Busy, t)) => ("busy", t.as_str()),
            Some((Status::Error, t)) => ("error", t.as_str()),
            Some((Status::Ok, t)) => ("ok", t.as_str()),
            Some((Status::Ask, t)) => ("ask", t.as_str()),
            None => ("", ""),
        };
        d.set("dlg.status", text);
        d.set("dlg.statusKind", kind);
    }

    /// A connection form (its first button connects).
    pub fn is_form(&self) -> bool {
        matches!(self.buttons.first().map(|b| &b.act), Some(Act::Connect { .. }))
    }

    pub fn write_sel(&self, d: &mut Data) {
        d.set("dlg.sel", self.sel as f64);
    }

    pub fn clear(d: &mut Data) {
        d.set("dlg.open", false);
        d.set("dlg.about", false);
        d.set("drop.side", "");
        // Typed passwords and keys don't stay in the data after the dialog.
        d.set("dlg.params", Json::Array(Vec::new()));
        d.set("dlg.value", "");
    }

    /// The permissions as ticked: the bits beyond rwx (setuid, sticky) stay as they were.
    pub fn ticked_mode(&self, d: &Data) -> Option<u32> {
        let m = self.mode?;
        Some(m & !0o777 | ticks(d))
    }

    /// The answers to the parameters, as typed into the dialog.
    pub fn args(&self, d: &Data) -> vfs::ActionArgs {
        let mut out = Vec::new();
        for (i, p) in self.params.iter().enumerate() {
            let v = d.get(&format!("dlg.params.{i}.value"));
            let v = match v {
                Some(Json::Bool(b)) => b.to_string(),
                Some(j) => j.as_str().map(str::to_string).unwrap_or_default(),
                None => String::new(),
            };
            out.push((p.id().to_string(), v));
        }
        vfs::ActionArgs(out)
    }
}

/// `left`, `right`, or nothing: which panel's drop-down is open.
fn drop_side(drop: Option<usize>) -> &'static str {
    match drop {
        Some(0) => "left",
        Some(_) => "right",
        None => "",
    }
}

/// `644`: read, write, execute of the owner, the group and the others. Setuid, setgid and sticky are
/// not shown and stay as they were.
pub fn octal(m: u32) -> String {
    format!("{:03o}", m & 0o777)
}

/// The rwx bits as ticked.
pub fn ticks(d: &Data) -> u32 {
    PERMS.iter().filter(|(k, _)| matches!(d.get(&format!("dlg.perm.{k}")), Some(Json::Bool(true)))).fold(0, |a, (_, b)| a | b)
}

pub fn set_ticks(d: &mut Data, mode: u32) {
    for (k, bit) in PERMS {
        d.set(&format!("dlg.perm.{k}"), mode & bit != 0);
    }
}

/// A parameter, its titles in the current language (sources and connectors name them in English).
fn param_json(p: &Param) -> Json {
    let tr = |s: &String| Json::from(t!(s).to_string());
    match p {
        Param::Choice { title, options, default, .. } => {
            let opts: Vec<Json> = options
                .iter()
                .map(|(v, l)| obj([("value", Json::from(v.clone())), ("label", Json::from(crate::i18n::tr_quiet(l).to_string()))]))
                .collect();
            obj([("kind", Json::from("choice")), ("title", tr(title)), ("options", Json::Array(opts)), ("value", Json::from(default.clone()))])
        }
        Param::Text { title, default, .. } => obj([("kind", Json::from("text")), ("title", tr(title)), ("value", Json::from(default.clone()))]),
        Param::Secret { title, .. } => obj([("kind", Json::from("secret")), ("title", tr(title)), ("value", Json::from(""))]),
        Param::Flag { title, default, .. } => {
            obj([("kind", Json::from("flag")), ("title", Json::from("")), ("label", tr(title)), ("value", Json::from(*default))])
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn octal_is_rwx_only() {
        assert_eq!(super::octal(0o100644), "644");
        assert_eq!(super::octal(0o4755), "755");
        assert_eq!(super::octal(0o7), "007");
    }
}
