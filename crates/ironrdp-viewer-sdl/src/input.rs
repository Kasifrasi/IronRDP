use std::collections::VecDeque;

use ironrdp_client::rdp::{RdpInputEvent, RdpInputSender};
use ironrdp_input::{Database, MouseButton, Operation, Scancode, WheelRotations, synchronize_event};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use sdl3::event::Event;
use sdl3::keyboard::{Mod, Scancode as SdlScancode};
use sdl3::mouse::{MouseButton as SdlButton, MouseWheelDirection};
use tokio::sync::mpsc::error::TrySendError;

use crate::desktop::Viewport;

enum Action {
    Operation(Operation),
    Release,
    Raw(Vec<FastPathInputEvent>),
}

/// Retains ordered input until the client's bounded queue has room.
#[derive(Default)]
pub struct Input {
    database: Database,
    pending: VecDeque<Action>,
}

impl Input {
    pub fn event(&mut self, event: &Event, viewport: Viewport, logical_size: (u32, u32)) -> anyhow::Result<()> {
        match event {
            Event::MouseMotion { x, y, .. } => self.motion(viewport, logical_size, (*x, *y)),
            Event::MouseButtonDown { x, y, mouse_btn, .. } | Event::MouseButtonUp { x, y, mouse_btn, .. } => {
                self.motion(viewport, logical_size, (*x, *y));
                let button = match mouse_btn {
                    SdlButton::Left => Some(MouseButton::Left),
                    SdlButton::Middle => Some(MouseButton::Middle),
                    SdlButton::Right => Some(MouseButton::Right),
                    SdlButton::X1 => Some(MouseButton::X1),
                    SdlButton::X2 => Some(MouseButton::X2),
                    _ => None,
                };
                if let Some(button) = button {
                    self.operation(if matches!(event, Event::MouseButtonDown { .. }) {
                        Operation::MouseButtonPressed(button)
                    } else {
                        Operation::MouseButtonReleased(button)
                    });
                }
            }
            Event::MouseWheel {
                x,
                y,
                mouse_x,
                mouse_y,
                direction,
                ..
            } => {
                self.motion(viewport, logical_size, (*mouse_x, *mouse_y));
                let sign = if *direction == MouseWheelDirection::Flipped {
                    -1.0
                } else {
                    1.0
                };
                for (is_vertical, delta) in [(true, *y), (false, *x)] {
                    if delta.is_finite() && delta != 0.0 {
                        let units = (delta * sign * 120.0).round().clamp(-255.0, 255.0);
                        #[expect(
                            clippy::as_conversions,
                            clippy::cast_possible_truncation,
                            reason = "wheel units are finite and clamped to the RDP range"
                        )]
                        let rotation_units = units as i16;
                        self.operation(Operation::WheelRotations(WheelRotations {
                            is_vertical,
                            rotation_units,
                        }));
                    }
                }
            }
            Event::KeyDown {
                scancode: Some(SdlScancode::Pause),
                repeat: false,
                ..
            } => {
                // MS-RDPBCGR 2.2.8.1.2.2.1 requires the complete Pause sequence on a press.
                self.pending.push_back(Action::Raw(vec![
                    FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1, 0x1D),
                    FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x45),
                    FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1 | KeyboardFlags::RELEASE, 0x1D),
                    FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x45),
                ]));
            }
            Event::KeyDown {
                scancode: Some(key), ..
            }
            | Event::KeyUp {
                scancode: Some(key), ..
            } => {
                if let Some(scancode) = scancode(*key) {
                    self.operation(if matches!(event, Event::KeyDown { .. }) {
                        Operation::KeyPressed(scancode)
                    } else {
                        Operation::KeyReleased(scancode)
                    });
                }
            }
            _ => {}
        }
        anyhow::ensure!(self.pending.len() <= 4096, "RDP input queue is not draining");
        Ok(())
    }

    fn operation(&mut self, operation: Operation) {
        self.pending.push_back(Action::Operation(operation));
    }

    fn motion(&mut self, viewport: Viewport, logical_size: (u32, u32), point: (f32, f32)) {
        if let Some(position) = viewport.position(logical_size, point) {
            if matches!(self.pending.back(), Some(Action::Operation(Operation::MouseMove(_)))) {
                self.pending.pop_back();
            }
            self.operation(Operation::MouseMove(position));
        }
    }

    pub fn release_all(&mut self) {
        self.pending.push_back(Action::Release);
    }

    pub fn synchronize(&mut self, modifiers: Mod) {
        self.pending.push_back(Action::Raw(vec![synchronize_event(
            modifiers.contains(Mod::SCROLLMOD),
            modifiers.contains(Mod::NUMMOD),
            modifiers.contains(Mod::CAPSMOD),
            false,
        )]));
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn flush(&mut self, sender: &RdpInputSender) -> anyhow::Result<()> {
        while !self.pending.is_empty() {
            // Reserve before changing key state, so a full queue cannot lose a key release.
            let permit = match sender.try_reserve() {
                Ok(permit) => permit,
                Err(TrySendError::Full(())) => return Ok(()),
                Err(TrySendError::Closed(())) => anyhow::bail!("RDP input channel closed"),
            };
            let Some(action) = self.pending.pop_front() else { break };
            let events = match action {
                Action::Operation(operation) => self.database.apply([operation]),
                Action::Release => self.database.release_all(),
                Action::Raw(events) => events.into_iter().collect(),
            };
            if !events.is_empty() {
                permit.send(RdpInputEvent::FastPath(events));
            }
        }
        Ok(())
    }
}

/// Maps SDL USB key positions to the PC/AT set-1 scancodes used by RDP.
pub fn scancode(key: SdlScancode) -> Option<Scancode> {
    let (extended, code) = match key {
        SdlScancode::Escape => (false, 0x01),
        SdlScancode::_1 => (false, 0x02),
        SdlScancode::_2 => (false, 0x03),
        SdlScancode::_3 => (false, 0x04),
        SdlScancode::_4 => (false, 0x05),
        SdlScancode::_5 => (false, 0x06),
        SdlScancode::_6 => (false, 0x07),
        SdlScancode::_7 => (false, 0x08),
        SdlScancode::_8 => (false, 0x09),
        SdlScancode::_9 => (false, 0x0A),
        SdlScancode::_0 => (false, 0x0B),
        SdlScancode::Minus => (false, 0x0C),
        SdlScancode::Equals => (false, 0x0D),
        SdlScancode::Backspace => (false, 0x0E),
        SdlScancode::Tab => (false, 0x0F),
        SdlScancode::Q => (false, 0x10),
        SdlScancode::W => (false, 0x11),
        SdlScancode::E => (false, 0x12),
        SdlScancode::R => (false, 0x13),
        SdlScancode::T => (false, 0x14),
        SdlScancode::Y => (false, 0x15),
        SdlScancode::U => (false, 0x16),
        SdlScancode::I => (false, 0x17),
        SdlScancode::O => (false, 0x18),
        SdlScancode::P => (false, 0x19),
        SdlScancode::LeftBracket => (false, 0x1A),
        SdlScancode::RightBracket => (false, 0x1B),
        SdlScancode::Return => (false, 0x1C),
        SdlScancode::LCtrl => (false, 0x1D),
        SdlScancode::A => (false, 0x1E),
        SdlScancode::S => (false, 0x1F),
        SdlScancode::D => (false, 0x20),
        SdlScancode::F => (false, 0x21),
        SdlScancode::G => (false, 0x22),
        SdlScancode::H => (false, 0x23),
        SdlScancode::J => (false, 0x24),
        SdlScancode::K => (false, 0x25),
        SdlScancode::L => (false, 0x26),
        SdlScancode::Semicolon => (false, 0x27),
        SdlScancode::Apostrophe => (false, 0x28),
        SdlScancode::Grave => (false, 0x29),
        SdlScancode::LShift => (false, 0x2A),
        SdlScancode::Backslash | SdlScancode::NonUsHash => (false, 0x2B),
        SdlScancode::Z => (false, 0x2C),
        SdlScancode::X => (false, 0x2D),
        SdlScancode::C => (false, 0x2E),
        SdlScancode::V => (false, 0x2F),
        SdlScancode::B => (false, 0x30),
        SdlScancode::N => (false, 0x31),
        SdlScancode::M => (false, 0x32),
        SdlScancode::Comma => (false, 0x33),
        SdlScancode::Period => (false, 0x34),
        SdlScancode::Slash => (false, 0x35),
        SdlScancode::RShift => (false, 0x36),
        SdlScancode::KpMultiply => (false, 0x37),
        SdlScancode::LAlt => (false, 0x38),
        SdlScancode::Space => (false, 0x39),
        SdlScancode::CapsLock => (false, 0x3A),
        SdlScancode::F1 => (false, 0x3B),
        SdlScancode::F2 => (false, 0x3C),
        SdlScancode::F3 => (false, 0x3D),
        SdlScancode::F4 => (false, 0x3E),
        SdlScancode::F5 => (false, 0x3F),
        SdlScancode::F6 => (false, 0x40),
        SdlScancode::F7 => (false, 0x41),
        SdlScancode::F8 => (false, 0x42),
        SdlScancode::F9 => (false, 0x43),
        SdlScancode::F10 => (false, 0x44),
        SdlScancode::NumLockClear => (false, 0x45),
        SdlScancode::ScrollLock => (false, 0x46),
        SdlScancode::Kp7 => (false, 0x47),
        SdlScancode::Kp8 => (false, 0x48),
        SdlScancode::Kp9 => (false, 0x49),
        SdlScancode::KpMinus => (false, 0x4A),
        SdlScancode::Kp4 => (false, 0x4B),
        SdlScancode::Kp5 => (false, 0x4C),
        SdlScancode::Kp6 => (false, 0x4D),
        SdlScancode::KpPlus => (false, 0x4E),
        SdlScancode::Kp1 => (false, 0x4F),
        SdlScancode::Kp2 => (false, 0x50),
        SdlScancode::Kp3 => (false, 0x51),
        SdlScancode::Kp0 => (false, 0x52),
        SdlScancode::KpPeriod => (false, 0x53),
        SdlScancode::NonUsBackslash => (false, 0x56),
        SdlScancode::F11 => (false, 0x57),
        SdlScancode::F12 => (false, 0x58),
        SdlScancode::KpEnter => (true, 0x1C),
        SdlScancode::RCtrl => (true, 0x1D),
        SdlScancode::KpDivide => (true, 0x35),
        SdlScancode::PrintScreen => (true, 0x37),
        SdlScancode::RAlt => (true, 0x38),
        SdlScancode::Home => (true, 0x47),
        SdlScancode::Up => (true, 0x48),
        SdlScancode::PageUp => (true, 0x49),
        SdlScancode::Left => (true, 0x4B),
        SdlScancode::Right => (true, 0x4D),
        SdlScancode::End => (true, 0x4F),
        SdlScancode::Down => (true, 0x50),
        SdlScancode::PageDown => (true, 0x51),
        SdlScancode::Insert => (true, 0x52),
        SdlScancode::Delete => (true, 0x53),
        SdlScancode::LGui => (true, 0x5B),
        SdlScancode::RGui => (true, 0x5C),
        SdlScancode::Application => (true, 0x5D),
        _ => return None,
    };
    Some(Scancode::from_u8(extended, code))
}
