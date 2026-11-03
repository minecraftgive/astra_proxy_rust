//! Single-line editing state with a stable serializable representation.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TextInput {
    pub text: String,
    pub cursor: usize,
    pub replace: bool,
    #[serde(default)]
    pub candidate: Option<String>,
    #[serde(default)]
    pub undo: Vec<(String, usize)>,
    #[serde(default)]
    pub redo: Vec<(String, usize)>,
}

impl TextInput {
    pub fn new(text: String) -> Self {
        let cursor = text.chars().count();
        Self {
            text,
            cursor,
            replace: true,
            candidate: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
    pub fn byte(&self) -> usize {
        let raw = self
            .text
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len());
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .chain(std::iter::once(self.text.len()))
            .take_while(|&i| i <= raw)
            .last()
            .unwrap_or(0)
    }
    fn previous_byte(&self) -> usize {
        let current = self.byte();
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .take_while(|&i| i < current)
            .last()
            .unwrap_or(0)
    }
    fn next_byte(&self) -> usize {
        let current = self.byte();
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i > current)
            .unwrap_or(self.text.len())
    }
    fn move_to_byte(&mut self, byte: usize) {
        // Keep the existing character-index wire format for saved TUI drafts.
        self.cursor = self.text[..byte].chars().count();
    }
    pub fn insert(&mut self, text: &str) {
        self.move_to_byte(self.byte());
        self.remember();
        self.candidate = None;
        if self.replace {
            self.text.clear();
            self.cursor = 0;
            self.replace = false;
        }
        let inserted: String = text.chars().filter(|c| !c.is_control()).collect();
        let byte = self.byte();
        self.text.insert_str(byte, &inserted);
        let end = byte + inserted.len();
        let boundary = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= end)
            .unwrap_or(self.text.len());
        self.move_to_byte(boundary);
    }
    pub fn remember(&mut self) {
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
    pub fn key(&mut self, key: KeyEvent) {
        self.move_to_byte(self.byte());
        self.candidate = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('z' | 'y')) {
                let redo = key.code == KeyCode::Char('y');
                let previous = if redo {
                    self.redo.pop()
                } else {
                    self.undo.pop()
                };
                if let Some((text, cursor)) = previous {
                    let current = (self.text.clone(), self.cursor);
                    if redo {
                        self.undo.push(current);
                    } else {
                        self.redo.push(current);
                    }
                    self.text = text;
                    self.cursor = cursor;
                    self.replace = false;
                }
            } else if key.code == KeyCode::Char('u') {
                self.remember();
                self.text.clear();
                self.cursor = 0;
                self.replace = false;
            }
            return;
        }
        match key.code {
            KeyCode::Char(c) => self.insert(&c.to_string()),
            KeyCode::Left => {
                self.replace = false;
                self.move_to_byte(self.previous_byte());
            }
            KeyCode::Right => {
                self.replace = false;
                self.move_to_byte(self.next_byte());
            }
            KeyCode::Home => {
                self.replace = false;
                self.cursor = 0;
            }
            KeyCode::End => {
                self.replace = false;
                self.cursor = self.text.chars().count();
            }
            KeyCode::Backspace | KeyCode::Delete if self.replace => {
                self.remember();
                self.text.clear();
                self.cursor = 0;
                self.replace = false;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.remember();
                let end = self.byte();
                let start = self.previous_byte();
                self.text.replace_range(start..end, "");
                self.move_to_byte(start);
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                self.remember();
                let byte = self.byte();
                self.text.replace_range(byte..self.next_byte(), "");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(input: &mut TextInput, code: KeyCode, modifiers: KeyModifiers) {
        input.key(KeyEvent::new(code, modifiers));
    }
    #[test]
    fn combining_marks_and_joined_emoji_are_single_editing_units() {
        let mut input = TextInput::new("e\u{301}👩‍💻国".into());
        key(&mut input, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(input.cursor, 5);
        key(&mut input, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(input.cursor, 2);
        key(&mut input, KeyCode::Right, KeyModifiers::NONE);
        key(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!((input.text.as_str(), input.cursor), ("e\u{301}国", 2));
        key(&mut input, KeyCode::Home, KeyModifiers::NONE);
        key(&mut input, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input.text, "国");
        key(&mut input, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(input.text, "e\u{301}国");
        input.cursor = 1; // Old drafts could leave a cursor inside a grapheme.
        key(&mut input, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(input.cursor, 2);
    }

    #[test]
    fn unicode_edits_undo_and_redo_preserve_character_cursor() {
        let mut input = TextInput::new("节点🙂".into());
        key(&mut input, KeyCode::Left, KeyModifiers::NONE);
        input.insert("é");
        assert_eq!(
            (input.text.as_str(), input.cursor, input.byte()),
            ("节点é🙂", 3, 8)
        );
        key(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(input.text, "节点🙂");
        key(&mut input, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(input.text, "节点é🙂");
        key(&mut input, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(input.text, "节点🙂");
        key(&mut input, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(input.text, "节点");
    }

    #[test]
    fn serialized_draft_retains_history_and_accepts_legacy_fields() {
        let mut legacy: TextInput =
            serde_json::from_str(r#"{"text":"old","cursor":3,"replace":true}"#).unwrap();
        legacy.insert("新\n值\t");
        assert_eq!(legacy.text, "新值");
        let encoded = serde_json::to_string(&legacy).unwrap();
        let mut restored: TextInput = serde_json::from_str(&encoded).unwrap();
        key(&mut restored, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!((restored.text.as_str(), restored.cursor), ("old", 3));
    }
}
