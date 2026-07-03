//! Map an egui input event onto the engine-neutral [`wire::InputEvent`], scaling
//! pointer geometry by `pixels_per_point`.
//!
//! egui reports pointer positions and scroll in **logical points**; the sandboxed
//! engine renders (and the shm frame is sized) in **device pixels**. So every
//! coordinate that crosses the socket is multiplied by the frame's
//! `pixels_per_point` here — a `HiDPI` seat at ppp = 2.0 turns a click at logical
//! (100, 50) into a device click at (200, 100), landing on the right element.
//!
//! Events the sandboxed browser has no use for (or keys with no neutral mapping)
//! return `None` — printable characters ride [`wire::InputEvent::Text`] regardless,
//! so dropping an unmapped [`egui::Key`] loses no typing.

use crate::egui::{self, Event, Key};
use crate::wire::{InputEvent, KeyCode, Modifiers, PointerButton};

/// A fallback device-pixel height for one wheel "line" (egui `Line` scroll unit),
/// matching the shared body text size — good enough for best-effort wheel speed.
const LINE_PX: f32 = 14.0;

/// Map one egui event to a forwarded [`InputEvent`], or `None` if it does not
/// forward. `pixels_per_point` is the frame's scale (device px per logical point).
#[must_use]
pub fn map_event(event: &Event, pixels_per_point: f32) -> Option<InputEvent> {
    let ppp = pixels_per_point;
    match event {
        Event::PointerMoved(pos) => Some(InputEvent::PointerMoved {
            x: pos.x * ppp,
            y: pos.y * ppp,
        }),
        Event::PointerButton {
            pos,
            button,
            pressed,
            ..
        } => Some(InputEvent::PointerButton {
            x: pos.x * ppp,
            y: pos.y * ppp,
            button: map_button(*button)?,
            pressed: *pressed,
        }),
        Event::PointerGone => Some(InputEvent::PointerGone),
        Event::MouseWheel { unit, delta, .. } => {
            let scale = match unit {
                egui::MouseWheelUnit::Point => ppp,
                egui::MouseWheelUnit::Line => LINE_PX * ppp,
                egui::MouseWheelUnit::Page => LINE_PX * 20.0 * ppp,
            };
            Some(InputEvent::Scroll {
                delta_x: delta.x * scale,
                delta_y: delta.y * scale,
            })
        }
        Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => Some(InputEvent::Key {
            key: map_key(*key)?,
            pressed: *pressed,
            modifiers: map_modifiers(*modifiers),
        }),
        Event::Text(text) => Some(InputEvent::Text(text.clone())),
        _ => None,
    }
}

const fn map_button(button: egui::PointerButton) -> Option<PointerButton> {
    match button {
        egui::PointerButton::Primary => Some(PointerButton::Primary),
        egui::PointerButton::Secondary => Some(PointerButton::Secondary),
        egui::PointerButton::Middle => Some(PointerButton::Middle),
        egui::PointerButton::Extra1 | egui::PointerButton::Extra2 => None,
    }
}

const fn map_modifiers(m: egui::Modifiers) -> Modifiers {
    let mut bits = 0u8;
    if m.ctrl {
        bits |= Modifiers::CTRL;
    }
    if m.shift {
        bits |= Modifiers::SHIFT;
    }
    if m.alt {
        bits |= Modifiers::ALT;
    }
    if m.command || m.mac_cmd {
        bits |= Modifiers::COMMAND;
    }
    Modifiers(bits)
}

#[allow(
    clippy::too_many_lines,
    reason = "a flat key-name table is clearer than a clever mapping"
)]
const fn map_key(key: Key) -> Option<KeyCode> {
    Some(match key {
        Key::Enter => KeyCode::Enter,
        Key::Escape => KeyCode::Escape,
        Key::Backspace => KeyCode::Backspace,
        Key::Tab => KeyCode::Tab,
        Key::Space => KeyCode::Space,
        Key::Delete => KeyCode::Delete,
        Key::Insert => KeyCode::Insert,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
        Key::ArrowUp => KeyCode::ArrowUp,
        Key::ArrowDown => KeyCode::ArrowDown,
        Key::ArrowLeft => KeyCode::ArrowLeft,
        Key::ArrowRight => KeyCode::ArrowRight,
        Key::A => KeyCode::A,
        Key::B => KeyCode::B,
        Key::C => KeyCode::C,
        Key::D => KeyCode::D,
        Key::E => KeyCode::E,
        Key::F => KeyCode::F,
        Key::G => KeyCode::G,
        Key::H => KeyCode::H,
        Key::I => KeyCode::I,
        Key::J => KeyCode::J,
        Key::K => KeyCode::K,
        Key::L => KeyCode::L,
        Key::M => KeyCode::M,
        Key::N => KeyCode::N,
        Key::O => KeyCode::O,
        Key::P => KeyCode::P,
        Key::Q => KeyCode::Q,
        Key::R => KeyCode::R,
        Key::S => KeyCode::S,
        Key::T => KeyCode::T,
        Key::U => KeyCode::U,
        Key::V => KeyCode::V,
        Key::W => KeyCode::W,
        Key::X => KeyCode::X,
        Key::Y => KeyCode::Y,
        Key::Z => KeyCode::Z,
        Key::Num0 => KeyCode::Num0,
        Key::Num1 => KeyCode::Num1,
        Key::Num2 => KeyCode::Num2,
        Key::Num3 => KeyCode::Num3,
        Key::Num4 => KeyCode::Num4,
        Key::Num5 => KeyCode::Num5,
        Key::Num6 => KeyCode::Num6,
        Key::Num7 => KeyCode::Num7,
        Key::Num8 => KeyCode::Num8,
        Key::Num9 => KeyCode::Num9,
        Key::F1 => KeyCode::F1,
        Key::F2 => KeyCode::F2,
        Key::F3 => KeyCode::F3,
        Key::F4 => KeyCode::F4,
        Key::F5 => KeyCode::F5,
        Key::F6 => KeyCode::F6,
        Key::F7 => KeyCode::F7,
        Key::F8 => KeyCode::F8,
        Key::F9 => KeyCode::F9,
        Key::F10 => KeyCode::F10,
        Key::F11 => KeyCode::F11,
        Key::F12 => KeyCode::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egui::{pos2, vec2, Modifiers as EMods, PointerButton as EButton};

    #[test]
    fn pointer_position_scales_by_pixels_per_point() {
        let ev = Event::PointerMoved(pos2(100.0, 50.0));
        assert_eq!(
            map_event(&ev, 2.0),
            Some(InputEvent::PointerMoved { x: 200.0, y: 100.0 })
        );
        // At ppp = 1.0 the coordinate is unchanged.
        assert_eq!(
            map_event(&ev, 1.0),
            Some(InputEvent::PointerMoved { x: 100.0, y: 50.0 })
        );
    }

    #[test]
    fn a_click_scales_and_maps_the_button() {
        let ev = Event::PointerButton {
            pos: pos2(10.0, 20.0),
            button: EButton::Secondary,
            pressed: true,
            modifiers: EMods::default(),
        };
        assert_eq!(
            map_event(&ev, 1.5),
            Some(InputEvent::PointerButton {
                x: 15.0,
                y: 30.0,
                button: PointerButton::Secondary,
                pressed: true,
            })
        );
    }

    #[test]
    fn wheel_delta_scales_by_unit_and_ppp() {
        let ev = Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 3.0),
            modifiers: EMods::default(),
        };
        assert_eq!(
            map_event(&ev, 2.0),
            Some(InputEvent::Scroll {
                delta_x: 0.0,
                delta_y: 6.0
            })
        );
    }

    #[test]
    fn a_key_with_modifiers_maps_across() {
        let ev = Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: EMods {
                ctrl: true,
                ..Default::default()
            },
        };
        let mapped = map_event(&ev, 1.0);
        assert!(
            matches!(
                &mapped,
                Some(InputEvent::Key { key: KeyCode::A, pressed: true, modifiers })
                    if modifiers.has(Modifiers::CTRL) && !modifiers.has(Modifiers::SHIFT)
            ),
            "Ctrl+A did not map as expected: {mapped:?}"
        );
    }

    #[test]
    fn text_forwards_verbatim_and_unmapped_events_drop() {
        assert_eq!(
            map_event(&Event::Text("hi".to_owned()), 2.0),
            Some(InputEvent::Text("hi".to_owned()))
        );
        // A zoom gesture is not forwarded.
        assert_eq!(map_event(&Event::Zoom(1.1), 1.0), None);
    }
}
