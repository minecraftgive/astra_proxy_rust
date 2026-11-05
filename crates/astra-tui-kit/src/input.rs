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
#[path = "test/input_test.rs"]
mod tests;
