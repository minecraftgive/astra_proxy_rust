use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionMode {
    #[default]
    Normal,
    Reduced,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Marquee {
    pub border: bool,
    pub text: bool,
    pub background: bool,
    /// The bottom status line text.
    pub status: bool,
    /// The connection dot and state word, only while connected.
    pub indicator: bool,
    /// Paint marquee cells in 24-bit color whatever the Colors setting says.
    pub true_color: bool,
    /// Hue degrees per second.
    pub speed: f32,
    /// Cells covered by one whole rainbow.
    pub width: f32,
    /// Frames per second while only the marquee moves.
    pub fps: u16,
}
impl Default for Marquee {
    fn default() -> Self {
        Self {
            border: false,
            text: false,
            background: false,
            status: false,
            indicator: false,
            true_color: false,
            speed: 160.0,
            width: 30.0,
            fps: 30,
        }
    }
}
impl Marquee {
    pub fn any(self) -> bool {
        self.border || self.text || self.background || self.status || self.indicator
    }
}
