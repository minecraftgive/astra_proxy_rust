//! Terminal primitives shared by Astra interfaces, independent of application state.
pub mod frame;
pub mod input;
pub mod layout;
pub mod terminal;
pub mod text;
pub mod theme;
pub mod widgets;

pub use frame::{FrameOutput, Screen};
pub use input::TextInput;
pub use layout::{HitMap, RenderedRows, RowGeometry};
pub use terminal::{TerminalOptions, TerminalSession};
pub use widgets::{Panel, SelectableList};
