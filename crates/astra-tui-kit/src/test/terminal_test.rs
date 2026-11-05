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
