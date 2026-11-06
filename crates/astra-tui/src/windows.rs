pub use astra_client::windows::*;
#[cfg(windows)]
mod console;
#[cfg(all(windows, test))]
use console::attach_console_io;
#[cfg(windows)]
pub use console::{prepare_console, report_startup_error, ConsoleMode};
#[cfg(all(windows, test))]
use std::process::Command;
#[cfg(all(windows, test))]
#[path = "test/windows_test.rs"]
mod tests;
