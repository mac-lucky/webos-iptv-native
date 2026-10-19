//! A text field typed into with the TV keyboard: the draft and whether the keyboard is up.
//!
//! The app draws no keyboard. The shell raises and dismisses the television's own panel as the
//! screen's `editing()` says and hands every committed string to the field's owner. Two facts of
//! LG's SDL shape this: the panel can vanish without telling anyone, so text that arrives while
//! the field thinks it is idle means the panel is up and the field goes back to editing rather
//! than dropping it; and a multi-character commit is a prediction replacing the last word, which
//! [`TextBuffer::commit`] already handles.

use tv_ui::text_buffer::TextBuffer;

use super::PanelKey;

/// The longest draft kept; the keyboard has no business producing more.
const MAX_LEN: usize = 2048;

#[derive(Clone, Debug)]
pub struct TextField {
    buf: TextBuffer,
    editing: bool,
}

impl TextField {
    pub fn new(text: &str) -> Self {
        Self {
            buf: TextBuffer::new(text.to_owned(), text.len()),
            editing: false,
        }
    }

    pub fn text(&self) -> &str {
        self.buf.text()
    }

    pub fn editing(&self) -> bool {
        self.editing
    }

    /// Raise or dismiss the keyboard.
    pub fn set_editing(&mut self, on: bool) {
        self.editing = on;
    }

    /// A commit from the keyboard; arriving while the field was not editing, it means the
    /// keyboard is up after all, and the field is editing again. Returns whether the text took it.
    pub fn on_text(&mut self, text: &str) -> bool {
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if text.is_empty() || self.buf.text().len() + text.len() > MAX_LEN {
            return false;
        }
        self.buf.commit(&text);
        self.editing = true;
        tv_machine::idle::invalidate();
        true
    }

    /// Editing keys the remote or the keyboard sends as key events. Returns whether the text
    /// changed.
    pub fn key(&mut self, key: PanelKey) -> bool {
        let before = self.buf.text().len();
        match key {
            PanelKey::Backspace => self.buf.backspace(),
            PanelKey::Clear => self.buf.clear(),
            _ => return false,
        }
        tv_machine::idle::invalidate();
        self.buf.text().len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_append_and_predictions_replace_the_last_word() {
        let mut f = TextField::new("");
        f.set_editing(true);
        assert!(f.on_text("n"));
        f.on_text("e");
        f.on_text(" ");
        f.on_text("w");
        f.on_text("world");
        assert_eq!(f.text(), "ne world");
    }

    #[test]
    fn text_while_idle_takes_the_keyboard_as_up() {
        let mut f = TextField::new("ab");
        assert!(f.on_text("c"));
        assert!(f.editing());
        assert_eq!(f.text(), "abc");
    }

    #[test]
    fn control_characters_and_empty_commits_are_dropped() {
        let mut f = TextField::new("");
        assert!(!f.on_text("\u{0}\n"));
        assert!(!f.editing());
        f.on_text("a\u{7}b");
        assert_eq!(f.text(), "ab");
    }

    #[test]
    fn backspace_and_clear() {
        let mut f = TextField::new("zolw\u{17c}");
        assert!(f.key(PanelKey::Backspace));
        assert_eq!(f.text(), "zolw");
        assert!(f.key(PanelKey::Clear));
        assert_eq!(f.text(), "");
        assert!(!f.key(PanelKey::Backspace));
        assert!(!f.key(PanelKey::Up));
    }

    #[test]
    fn the_draft_is_bounded() {
        let mut f = TextField::new(&"a".repeat(MAX_LEN));
        assert!(!f.on_text("b"));
        assert_eq!(f.text().len(), MAX_LEN);
    }
}
