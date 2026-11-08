//! Saving the state, quitting, the mouse on rows.

use super::*;

impl App {
    pub(super) fn save_state(&mut self) {
        if !self.persist {
            return;
        }
        let s = state::State::of(self).to_json();
        if s != self.saved {
            self.saved = s.clone();
            state::save(s);
        }
    }

    pub(super) fn quit(&mut self) {
        if self.persist {
            state::save_now(&state::State::of(self).to_json());
        }
        std::process::exit(0);
    }

    pub(super) fn row_click(&mut self, cx: &mut Cx) {
        let Some((side, row)) = row_key(cx.key) else { return };
        if self.dlg.as_ref().is_some_and(|d| d.drop.is_some()) {
            self.dialog_key("escape", cx);
        }
        self.set_active(side, cx);
        self.close_quick(cx);
        self.panels[side].move_cursor(row as isize);
        self.moved(side, cx);
        if cx.clicks == 2 {
            self.open(cx);
        }
    }

    /// A context menu item: on the row it was opened on.
    pub(super) fn context(&mut self, c: Cmd, cx: &mut Cx) {
        if let Some((side, row)) = row_key(cx.key) {
            self.set_active(side, cx);
            if !self.panels[side].has_selection() {
                self.panels[side].move_cursor(row as isize);
                self.moved(side, cx);
            }
        }
        self.run(c, cx);
    }
}
