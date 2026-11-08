//! Settings (the gear in the toolbar): the theme and the language. Both are kept in the state.

use super::*;
use crate::i18n::{self, Lang};
use uison::ui::IntoJson;

/// The themes, as the settings panel names them.
pub(super) const THEMES: [&str; 3] = ["light", "dark", "system"];

impl App {
    pub(super) fn toggle_settings(&mut self, cx: &mut Cx) {
        if self.settings {
            return self.close_settings(cx);
        }
        // Over a drop-down of sources it would be two lists at once.
        if self.dlg.as_ref().is_some_and(|d| d.drop.is_some()) {
            self.dialog_key("escape", cx);
        }
        self.settings = true;
        self.write_settings(cx.data);
    }

    pub(super) fn close_settings(&mut self, cx: &mut Cx) {
        self.settings = false;
        self.write_settings(cx.data);
    }

    pub(super) fn write_settings(&self, d: &mut Data) {
        d.set("set.open", self.settings);
        d.set("set.theme", self.theme.as_str());
        d.set("set.lang", i18n::lang().code());
    }

    /// A choice in the panel (`set-dark`, `set-ru`): it is taken at once and the panel stays open.
    pub(super) fn settings_pick(&mut self, cx: &mut Cx) {
        let Some(k) = cx.key.strip_prefix("set-") else { return };
        if THEMES.contains(&k) {
            self.set_theme(k, cx);
        } else if let Some(l) = Lang::parse(k) {
            self.set_lang(l, cx);
        }
        self.write_settings(cx.data);
        self.save_state();
    }

    pub(super) fn set_theme(&mut self, theme: &str, cx: &mut Cx) {
        self.theme = theme.to_string();
        cx.set_theme(theme);
        // The window's frame and the system's menus (a check has no window and leaves the program's look alone).
        #[cfg(target_os = "macos")]
        if self.persist {
            crate::macos::set_appearance(theme);
        }
        self.write_dark(cx.data);
    }

    /// `look.dark`: the symbols' ink follows the theme shown, not only the system's (`@env.dark`).
    pub(super) fn write_dark(&self, d: &mut Data) {
        let dark = match self.theme.as_str() {
            "dark" => true,
            "light" => false,
            _ => d.get("env.dark").is_some_and(Json::truthy),
        };
        d.set("look.dark", dark);
    }

    /// The screen is built again in the new language; the texts the app wrote are written again.
    pub(super) fn set_lang(&mut self, l: Lang, cx: &mut Cx) {
        if l == i18n::lang() {
            return;
        }
        i18n::set(l);
        *cx.tree.root() = crate::screen::screen().into_json();
        self.write_panels(cx.data);
        self.write_job(cx.data);
        App::write_connect_menus(cx.data);
    }
}
