//! The pure parts of the macOS (Cocoa) window backend, compiled and tested on every operating
//! system: translating `NSEvent` key codes, modifier flags and characters into the GUI's keys,
//! turning a view's size in points and its backing scale factor into device pixels, and placing
//! a frame's image at the top left of a view whose origin is at the bottom left, and mapping
//! accessibility nodes to NSAccessibility roles and frames. The values come from Apple's public
//! AppKit, Core Graphics and NSAccessibility documentation.

use super::{AccessRect, AccessRole, Key, Modifiers};

/// `NSEventModifierFlagShift`.
pub const FLAG_SHIFT: u64 = 1 << 17;
/// `NSEventModifierFlagControl`.
pub const FLAG_CONTROL: u64 = 1 << 18;
/// `NSEventModifierFlagOption`.
pub const FLAG_OPTION: u64 = 1 << 19;
/// `NSEventModifierFlagCommand`.
pub const FLAG_COMMAND: u64 = 1 << 20;

/// `kVK_Return`.
pub const KEY_RETURN: u16 = 0x24;
/// `kVK_Tab`.
pub const KEY_TAB: u16 = 0x30;
/// `kVK_Space`.
pub const KEY_SPACE: u16 = 0x31;
/// `kVK_Delete`, the key labelled Delete that erases backwards.
pub const KEY_DELETE: u16 = 0x33;
/// `kVK_Escape`.
pub const KEY_ESCAPE: u16 = 0x35;
/// `kVK_ANSI_KeypadEnter`.
pub const KEY_KEYPAD_ENTER: u16 = 0x4C;
/// `kVK_Home`.
pub const KEY_HOME: u16 = 0x73;
/// `kVK_ForwardDelete`.
pub const KEY_FORWARD_DELETE: u16 = 0x75;
/// `kVK_F4`.
pub const KEY_F4: u16 = 0x76;
/// `kVK_End`.
pub const KEY_END: u16 = 0x77;
/// `kVK_LeftArrow`.
pub const KEY_LEFT: u16 = 0x7B;
/// `kVK_RightArrow`.
pub const KEY_RIGHT: u16 = 0x7C;
/// `kVK_DownArrow`.
pub const KEY_DOWN: u16 = 0x7D;
/// `kVK_UpArrow`.
pub const KEY_UP: u16 = 0x7E;

/// A point in a view's coordinates (`CGPoint` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgPoint {
    /// The horizontal coordinate, in points.
    pub x: f64,
    /// The vertical coordinate, in points, growing upwards.
    pub y: f64,
}

/// A size in points (`CGSize` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgSize {
    /// The width in points.
    pub width: f64,
    /// The height in points.
    pub height: f64,
}

/// A rectangle in points (`CGRect` and `NSRect` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgRect {
    /// The bottom-left corner.
    pub origin: CgPoint,
    /// The width and height.
    pub size: CgSize,
}

/// The modifiers held, from an `NSEvent`'s `modifierFlags`.
pub fn modifiers_from_flags(flags: u64) -> Modifiers {
    Modifiers {
        shift: flags & FLAG_SHIFT != 0,
        control: flags & FLAG_CONTROL != 0,
        alt: flags & FLAG_OPTION != 0,
        command: flags & FLAG_COMMAND != 0,
    }
}

/// The key a hardware key code stands for when it is not a key that types text.
pub fn key_from_key_code(code: u16) -> Option<Key> {
    Some(match code {
        KEY_TAB => Key::Tab,
        KEY_RETURN | KEY_KEYPAD_ENTER => Key::Enter,
        KEY_ESCAPE => Key::Escape,
        KEY_SPACE => Key::Space,
        KEY_DELETE => Key::Backspace,
        KEY_FORWARD_DELETE => Key::Delete,
        KEY_LEFT => Key::Left,
        KEY_RIGHT => Key::Right,
        KEY_UP => Key::Up,
        KEY_DOWN => Key::Down,
        KEY_HOME => Key::Home,
        KEY_END => Key::End,
        KEY_F4 => Key::F4,
        _ => return None,
    })
}

/// Whether a key's character is read from `charactersIgnoringModifiers` (with Command or
/// Control held, so Cmd+Q arrives as `q` and Ctrl+A as `a` rather than a control code) instead
/// of `characters` (which carries what Option and dead keys composed).
pub fn reads_unmodified_characters(modifiers: Modifiers) -> bool {
    modifiers.command || modifiers.control
}

/// The character keys in an `NSEvent`'s characters: control codes, the space bar (answered by
/// its key code) and the private-use range AppKit gives function keys (`U+F700` to `U+F8FF`)
/// are left out.
pub fn keys_from_characters(text: &str) -> Vec<Key> {
    text.chars()
        .filter(|&c| !c.is_control() && c != ' ' && !('\u{F700}'..='\u{F8FF}').contains(&c))
        .map(Key::Character)
        .collect()
}

/// Device pixels per point from `backingScaleFactor`, and 1 for a value that is not a positive
/// finite number.
pub fn scale_from_backing(factor: f64) -> f32 {
    if factor.is_finite() && factor > 0.0 {
        factor as f32
    } else {
        1.0
    }
}

/// A view's size in device pixels from its size in points and the scale, each side rounded
/// and kept within `0..=u32::MAX`.
pub fn device_size(size: CgSize, scale: f32) -> (u32, u32) {
    let pixels = |points: f64| {
        let scaled = (points * f64::from(scale)).round();
        if scaled.is_finite() && scaled > 0.0 {
            scaled.min(f64::from(u32::MAX)) as u32
        } else {
            0
        }
    };
    (pixels(size.width), pixels(size.height))
}

/// Where a `width` × `height` device-pixel frame is drawn in a view `view_height` points tall
/// whose origin is at the bottom left: at the top left, one device pixel per frame pixel.
pub fn frame_rect(width: u32, height: u32, scale: f32, view_height: f64) -> CgRect {
    let scale = f64::from(scale_from_backing(f64::from(scale)));
    let size = CgSize {
        width: f64::from(width) / scale,
        height: f64::from(height) / scale,
    };
    CgRect {
        origin: CgPoint {
            x: 0.0,
            y: view_height - size.height,
        },
        size,
    }
}

/// How a node is exposed through NSAccessibility: its `AXRole`, and whether its visible text is
/// its value (static text) rather than its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxRole {
    /// The `NSAccessibilityRole` string, such as `AXButton`.
    pub role: &'static str,
    /// Whether the name is reported as the `AXValue`, as static text does.
    pub name_is_value: bool,
}

/// The NSAccessibility role for `role`, `None` for the window, which `NSWindow` already exposes.
pub fn ax_role(role: AccessRole) -> Option<AxRole> {
    let (role, name_is_value) = match role {
        AccessRole::Window => return None,
        AccessRole::Group | AccessRole::TabPanel => ("AXGroup", false),
        AccessRole::Heading => ("AXHeading", true),
        AccessRole::TabList => ("AXTabGroup", false),
        AccessRole::Tab => ("AXRadioButton", false),
        AccessRole::Status => ("AXStaticText", true),
        AccessRole::TextField => ("AXTextField", false),
        AccessRole::Button => ("AXButton", false),
        AccessRole::CheckBox => ("AXCheckBox", false),
        AccessRole::Link => ("AXLink", false),
    };
    Some(AxRole {
        role,
        name_is_value,
    })
}

/// A node's `accessibilityFrameInParentSpace`: `bounds` (logical pixels from the window's top
/// left, which are points) in a view `view_height` points tall whose origin is at the bottom
/// left, relative to the parent element's `parent` bounds when the parent is an element rather
/// than the view.
pub fn frame_in_parent_space(
    bounds: AccessRect,
    parent: Option<AccessRect>,
    view_height: f64,
) -> CgRect {
    let bottom_up = |rect: AccessRect| {
        (
            f64::from(rect.x),
            view_height - f64::from(rect.y) - f64::from(rect.height),
        )
    };
    let (x, y) = bottom_up(bounds);
    let (parent_x, parent_y) = parent.map_or((0.0, 0.0), bottom_up);
    CgRect {
        origin: CgPoint {
            x: x - parent_x,
            y: y - parent_y,
        },
        size: CgSize {
            width: f64::from(bounds.width),
            height: f64::from(bounds.height),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_but_the_window_has_an_ax_role() {
        assert_eq!(ax_role(AccessRole::Window), None);
        let button = ax_role(AccessRole::Button);
        assert_eq!(
            button,
            Some(AxRole {
                role: "AXButton",
                name_is_value: false
            })
        );
        assert_eq!(
            ax_role(AccessRole::Tab).map(|ax| ax.role),
            Some("AXRadioButton")
        );
        assert_eq!(
            ax_role(AccessRole::TabList).map(|ax| ax.role),
            Some("AXTabGroup")
        );
        assert_eq!(
            ax_role(AccessRole::CheckBox).map(|ax| ax.role),
            Some("AXCheckBox")
        );
        assert_eq!(
            ax_role(AccessRole::TextField).map(|ax| ax.role),
            Some("AXTextField")
        );
        assert!(ax_role(AccessRole::Status).is_some_and(|ax| ax.name_is_value));
        assert!(ax_role(AccessRole::Heading).is_some_and(|ax| ax.name_is_value));
    }

    #[test]
    fn frames_flip_to_a_bottom_left_origin_and_become_relative_to_the_parent() {
        let card = AccessRect {
            x: 360.0,
            y: 140.0,
            width: 560.0,
            height: 480.0,
        };
        let input = AccessRect {
            x: 384.0,
            y: 282.0,
            width: 420.0,
            height: 28.0,
        };
        let in_view = frame_in_parent_space(input, None, 800.0);
        assert_eq!(
            in_view,
            CgRect {
                origin: CgPoint { x: 384.0, y: 490.0 },
                size: CgSize {
                    width: 420.0,
                    height: 28.0
                }
            }
        );
        let in_card = frame_in_parent_space(input, Some(card), 800.0);
        assert_eq!(in_card.origin, CgPoint { x: 24.0, y: 310.0 });
        assert_eq!(in_card.size, in_view.size);
    }

    #[test]
    fn modifier_flags_map_to_modifiers() {
        assert_eq!(modifiers_from_flags(0), Modifiers::default());
        assert_eq!(
            modifiers_from_flags(FLAG_COMMAND | FLAG_SHIFT | 0x100),
            Modifiers {
                shift: true,
                control: false,
                alt: false,
                command: true,
            }
        );
        assert_eq!(
            modifiers_from_flags(FLAG_CONTROL | FLAG_OPTION),
            Modifiers {
                shift: false,
                control: true,
                alt: true,
                command: false,
            }
        );
    }

    #[test]
    fn key_codes_map_to_the_gui_keys() {
        let cases = [
            (KEY_TAB, Key::Tab),
            (KEY_RETURN, Key::Enter),
            (KEY_KEYPAD_ENTER, Key::Enter),
            (KEY_ESCAPE, Key::Escape),
            (KEY_SPACE, Key::Space),
            (KEY_DELETE, Key::Backspace),
            (KEY_FORWARD_DELETE, Key::Delete),
            (KEY_LEFT, Key::Left),
            (KEY_RIGHT, Key::Right),
            (KEY_UP, Key::Up),
            (KEY_DOWN, Key::Down),
            (KEY_HOME, Key::Home),
            (KEY_END, Key::End),
            (KEY_F4, Key::F4),
        ];
        for (code, key) in cases {
            assert_eq!(key_from_key_code(code), Some(key));
        }
        assert_eq!(key_from_key_code(0x0C), None);
        assert_eq!(key_from_key_code(u16::MAX), None);
    }

    #[test]
    fn shortcuts_read_the_unmodified_characters() {
        let command = modifiers_from_flags(FLAG_COMMAND);
        let control = modifiers_from_flags(FLAG_CONTROL);
        let option = modifiers_from_flags(FLAG_OPTION | FLAG_SHIFT);
        assert!(reads_unmodified_characters(command));
        assert!(reads_unmodified_characters(control));
        assert!(!reads_unmodified_characters(option));
        assert!(!reads_unmodified_characters(Modifiers::default()));
    }

    #[test]
    fn characters_become_keys_without_control_codes_or_function_keys() {
        assert_eq!(keys_from_characters("q"), [Key::Character('q')]);
        assert_eq!(
            keys_from_characters("é😀"),
            [Key::Character('é'), Key::Character('😀')]
        );
        assert!(keys_from_characters("\u{1}").is_empty());
        assert!(keys_from_characters(" ").is_empty());
        assert!(keys_from_characters("\r\t\u{7F}").is_empty());
        assert!(keys_from_characters("\u{F702}").is_empty());
        assert!(keys_from_characters("").is_empty());
    }

    #[test]
    fn backing_factors_become_scales() {
        assert_eq!(scale_from_backing(2.0), 2.0);
        assert_eq!(scale_from_backing(1.0), 1.0);
        assert_eq!(scale_from_backing(0.0), 1.0);
        assert_eq!(scale_from_backing(-2.0), 1.0);
        assert_eq!(scale_from_backing(f64::NAN), 1.0);
        assert_eq!(scale_from_backing(f64::INFINITY), 1.0);
    }

    #[test]
    fn point_sizes_become_device_pixels() {
        let size = CgSize {
            width: 640.0,
            height: 480.5,
        };
        assert_eq!(device_size(size, 1.0), (640, 481));
        assert_eq!(device_size(size, 2.0), (1280, 961));
        let odd = CgSize {
            width: -3.0,
            height: f64::NAN,
        };
        assert_eq!(device_size(odd, 2.0), (0, 0));
        let huge = CgSize {
            width: 1e300,
            height: 1.0,
        };
        assert_eq!(device_size(huge, 1.0), (u32::MAX, 1));
    }

    #[test]
    fn frames_are_placed_at_the_top_left_one_pixel_per_pixel() {
        assert_eq!(
            frame_rect(1280, 960, 2.0, 500.0),
            CgRect {
                origin: CgPoint { x: 0.0, y: 20.0 },
                size: CgSize {
                    width: 640.0,
                    height: 480.0,
                },
            }
        );
        assert_eq!(
            frame_rect(100, 50, 0.0, 50.0),
            CgRect {
                origin: CgPoint { x: 0.0, y: 0.0 },
                size: CgSize {
                    width: 100.0,
                    height: 50.0,
                },
            }
        );
    }
}
