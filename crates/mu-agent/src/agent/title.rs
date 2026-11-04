use super::*;

impl Agent {
    pub(super) fn apply_title(&mut self, sid: &str, t: String) {
        if self.session_id() != sid || self.title.text.is_some() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            s.set_title(&t, TitleSource::Auto);
        }
        self.title.text = Some(t);
        self.emit_status();
    }
}
