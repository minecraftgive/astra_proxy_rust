//! Convert terminal events at the frontend boundary.
pub fn key(event: crossterm::event::KeyEvent) -> astra_client::input::Key {
    use astra_client::input::{Key, KeyCode as C, KeyEventKind as K, KeyModifiers};
    use crossterm::event::{KeyCode as T, KeyEventKind};
    let code = match event.code {
        T::Backspace => C::Backspace,
        T::Enter => C::Enter,
        T::Left => C::Left,
        T::Right => C::Right,
        T::Up => C::Up,
        T::Down => C::Down,
        T::Home => C::Home,
        T::End => C::End,
        T::PageUp => C::PageUp,
        T::PageDown => C::PageDown,
        T::Tab => C::Tab,
        T::BackTab => C::BackTab,
        T::Delete => C::Delete,
        T::Insert => C::Insert,
        T::F(n) => C::F(n),
        T::Char(c) => C::Char(c),
        T::Esc => C::Esc,
        T::CapsLock => C::CapsLock,
        T::ScrollLock => C::ScrollLock,
        T::NumLock => C::NumLock,
        T::PrintScreen => C::PrintScreen,
        T::Pause => C::Pause,
        T::Menu => C::Menu,
        T::KeypadBegin => C::KeypadBegin,
        _ => C::Null,
    };
    Key::new_with_kind(
        code,
        KeyModifiers(event.modifiers.bits()),
        match event.kind {
            KeyEventKind::Press => K::Press,
            KeyEventKind::Repeat => K::Repeat,
            KeyEventKind::Release => K::Release,
        },
    )
}
