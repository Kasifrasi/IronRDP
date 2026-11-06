//! Monitor rectangles in the shared desktop framebuffer.

use ironrdp::pdu::gcc::Monitor;

/// A monitor's rectangle relative to the framebuffer's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorViewport {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

/// A server monitor layout normalized to nonnegative framebuffer coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayout {
    monitors: Vec<MonitorViewport>,
    size: (u16, u16),
}

impl MonitorLayout {
    /// Converts inclusive primary-relative monitor rectangles into framebuffer rectangles.
    pub fn new(_monitors: &[Monitor]) -> anyhow::Result<Self> {
        Ok(Self {
            monitors: Vec::new(),
            size: (0, 0),
        })
    }

    /// Returns monitor rectangles in the server's order.
    pub fn monitors(&self) -> &[MonitorViewport] {
        &self.monitors
    }

    /// Returns the bounding framebuffer dimensions.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }
}
