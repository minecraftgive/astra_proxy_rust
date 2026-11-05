//! A single owner for terminal state, including external-editor handoff and rollback.
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io;

#[derive(Clone, Copy, Debug, Default)]
pub struct TerminalOptions {
    pub mouse: bool,
    pub bracketed_paste: bool,
}

trait Operations {
    fn raw(&mut self, enabled: bool) -> io::Result<()>;
    fn alternate(&mut self, enabled: bool) -> io::Result<()>;
    fn mouse(&mut self, enabled: bool) -> io::Result<()>;
    fn paste(&mut self, enabled: bool) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
}

struct Native;
impl Operations for Native {
    fn raw(&mut self, enabled: bool) -> io::Result<()> {
        if enabled {
            terminal::enable_raw_mode()
        } else {
            terminal::disable_raw_mode()
        }
    }
    fn alternate(&mut self, enabled: bool) -> io::Result<()> {
        if enabled {
            execute!(io::stdout(), EnterAlternateScreen)
        } else {
            execute!(io::stdout(), LeaveAlternateScreen)
        }
    }
    fn mouse(&mut self, enabled: bool) -> io::Result<()> {
        if enabled {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        }
    }
    fn paste(&mut self, enabled: bool) -> io::Result<()> {
        if enabled {
            execute!(io::stdout(), EnableBracketedPaste)
        } else {
            execute!(io::stdout(), DisableBracketedPaste)
        }
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), crossterm::cursor::Show)
    }
}

struct Session<O: Operations> {
    operations: O,
    options: TerminalOptions,
    raw: bool,
    alternate: bool,
    mouse: bool,
    paste: bool,
}

impl<O: Operations> Session<O> {
    fn enter(operations: O, options: TerminalOptions) -> io::Result<Self> {
        let mut session = Self {
            operations,
            options,
            raw: false,
            alternate: false,
            mouse: false,
            paste: false,
        };
        session.resume()?;
        Ok(session)
    }
    fn resume(&mut self) -> io::Result<()> {
        let result = (|| {
            if !self.raw {
                self.raw = true;
                self.operations.raw(true)?;
            }
            if !self.alternate {
                self.alternate = true;
                self.operations.alternate(true)?;
            }
            if self.options.mouse && !self.mouse {
                self.mouse = true;
                self.operations.mouse(true)?;
            }
            if self.options.bracketed_paste && !self.paste {
                self.paste = true;
                self.operations.paste(true)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = self.suspend();
        }
        result
    }
    fn set_mouse(&mut self, enabled: bool) -> io::Result<()> {
        self.options.mouse = enabled;
        // While suspended the next resume applies the option.
        if !self.alternate || self.mouse == enabled {
            return Ok(());
        }
        self.operations.mouse(enabled)?;
        self.mouse = enabled;
        Ok(())
    }
    fn suspend(&mut self) -> io::Result<()> {
        let mut error = None;
        macro_rules! reset {
            ($flag:ident, $method:ident) => {
                if self.$flag {
                    match self.operations.$method(false) {
                        Ok(()) => self.$flag = false,
                        Err(failure) => {
                            error.get_or_insert(failure);
                        }
                    }
                }
            };
        }
        reset!(paste, paste);
        reset!(mouse, mouse);
        if self.alternate || self.raw {
            if let Err(failure) = self.operations.show_cursor() {
                error.get_or_insert(failure);
            }
        }
        reset!(alternate, alternate);
        reset!(raw, raw);
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<O: Operations> Drop for Session<O> {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

pub struct TerminalSession(Session<Native>);
impl TerminalSession {
    pub fn enter(options: TerminalOptions) -> io::Result<Self> {
        Session::enter(Native, options).map(Self)
    }
    /// The caller must detach its event reader before handing the terminal away.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.0.suspend()
    }
    pub fn resume(&mut self) -> io::Result<()> {
        self.0.resume()
    }
    /// Turn mouse capture on or off; off lets the terminal select text again.
    pub fn set_mouse(&mut self, enabled: bool) -> io::Result<()> {
        self.0.set_mouse(enabled)
    }
}

#[cfg(test)]
#[path = "test/terminal_test.rs"]
mod tests;
