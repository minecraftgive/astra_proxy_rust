//! Reusable presentation only; callers supply all domain data and actions.
mod card;
mod chart;
mod confirm;
mod delay_chip;
mod empty;
mod layout;
mod prompt;
mod sparkline;
mod status_dot;
mod toast;
mod token_text;

pub use card::Card;
pub use chart::{Chart, ChartSeries};
pub use confirm::Confirm;
pub use delay_chip::{DelayChip, ExitProbe};
pub use empty::Empty;
pub use layout::{surface, Metric, NavItem, NoticeBar, Pill, Tone};
pub use prompt::{PromptEvent, PromptHost};
pub use sparkline::Sparkline;
pub use status_dot::{Status, StatusDot};
pub use toast::{Toast, ToastHost, ToastKind};
pub use token_text::TokenText;

#[cfg(test)]
mod tests;
