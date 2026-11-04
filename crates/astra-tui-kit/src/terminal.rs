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
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[derive(Clone)]
    struct Fake {
        calls: Rc<RefCell<Vec<String>>>,
        failure: Option<&'static str>,
    }
    impl Fake {
        fn step(&mut self, name: &str, enabled: bool) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("{name}:{enabled}"));
            if enabled && self.failure == Some(name) {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        }
    }
    impl Operations for Fake {
        fn raw(&mut self, on: bool) -> io::Result<()> {
            self.step("raw", on)
        }
        fn alternate(&mut self, on: bool) -> io::Result<()> {
            self.step("alternate", on)
        }
        fn mouse(&mut self, on: bool) -> io::Result<()> {
            self.step("mouse", on)
        }
        fn paste(&mut self, on: bool) -> io::Result<()> {
            self.step("paste", on)
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.step("cursor", false)
        }
    }
    #[test]
    fn initialization_failure_rolls_back_each_completed_step() {
        for failure in ["raw", "alternate", "mouse", "paste"] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            assert!(Session::enter(
                Fake {
                    calls: calls.clone(),
                    failure: Some(failure)
                },
                TerminalOptions {
                    mouse: true,
                    bracketed_paste: true
                }
            )
            .is_err());
            let calls = calls.borrow();
            assert!(calls.contains(&format!("{failure}:false")));
            assert_eq!(calls.last().unwrap(), "raw:false");
        }
    }
    #[test]
    fn suspend_resume_uses_one_owner_and_cleanup_is_idempotent() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut session = Session::enter(
            Fake {
                calls: calls.clone(),
                failure: None,
            },
            TerminalOptions::default(),
        )
        .unwrap();
        session.suspend().unwrap();
        let count = calls.borrow().len();
        session.suspend().unwrap();
        assert_eq!(calls.borrow().len(), count);
        session.resume().unwrap();
        assert_eq!(calls.borrow().last().unwrap(), "alternate:true");
        drop(session);
        assert_eq!(calls.borrow().last().unwrap(), "raw:false");
    }
    #[test]
    fn mouse_capture_toggles_live_and_survives_suspend() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut session = Session::enter(
            Fake {
                calls: calls.clone(),
                failure: None,
            },
            TerminalOptions {
                mouse: true,
                bracketed_paste: false,
            },
        )
        .unwrap();
        session.set_mouse(false).unwrap();
        assert_eq!(calls.borrow().last().unwrap(), "mouse:false");
        let count = calls.borrow().len();
        session.set_mouse(false).unwrap();
        assert_eq!(calls.borrow().len(), count, "no repeated toggles");
        session.suspend().unwrap();
        session.set_mouse(true).unwrap();
        assert!(!calls.borrow()[count..].contains(&"mouse:true".to_string()));
        session.resume().unwrap();
        assert_eq!(calls.borrow().last().unwrap(), "mouse:true");
    }
}
