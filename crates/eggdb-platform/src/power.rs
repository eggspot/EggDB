//! What the machine runs on, for work that can wait while it runs on a battery (D41, D1087):
//! Linux and Android read `/sys/class/power_supply`, Windows asks `GetSystemPowerStatus`, and
//! other systems report nothing, which leaves background work as configured.

use std::path::Path;

/// Where the machine's power comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PowerSource {
    /// A charger or the mains: background work runs as configured.
    Mains,
    /// A battery that is not low: background work is spaced out.
    Battery,
    /// A battery at or below [`LOW_BATTERY_PERCENT`], or the system's battery saver: background
    /// work waits for the mains, unless it must run to stay correct.
    LowBattery,
}

/// The charge, in percent, at and below which a battery counts as low.
pub const LOW_BATTERY_PERCENT: u8 = 20;

/// Classifies a battery's state: discharging at `percent` (when known), with the system's
/// battery saver on or off.
pub fn on_battery(percent: Option<u8>, saver: bool) -> PowerSource {
    if saver || percent.is_some_and(|percent| percent <= LOW_BATTERY_PERCENT) {
        PowerSource::LowBattery
    } else {
        PowerSource::Battery
    }
}

/// What the machine runs on now, or `None` when the system does not say (a server without a
/// battery on most systems, or a system this layer cannot ask).
pub fn power_source() -> Option<PowerSource> {
    os::power_source()
}

/// Reads a Linux `power_supply` class directory, such as `/sys/class/power_supply`: any
/// mains, USB or wireless supply online means the mains; otherwise a discharging battery
/// means the battery, low at its `capacity`.
pub fn power_source_from_sysfs(directory: &Path) -> Option<PowerSource> {
    let entries = std::fs::read_dir(directory).ok()?;
    let mut battery = None;
    for entry in entries.flatten() {
        let supply = entry.path();
        let read = |name: &str| std::fs::read_to_string(supply.join(name)).ok().map(|text| text.trim().to_owned());
        match read("type").as_deref() {
            Some("Mains" | "USB" | "USB_C" | "USB_PD" | "Wireless") => {
                if read("online").as_deref() == Some("1") {
                    return Some(PowerSource::Mains);
                }
            }
            Some("Battery") if read("scope").as_deref() != Some("Device") => {
                if read("status").as_deref() == Some("Discharging") {
                    let percent = read("capacity").and_then(|text| text.parse::<u8>().ok());
                    battery = Some(on_battery(percent, false));
                } else if battery.is_none() {
                    battery = Some(PowerSource::Mains);
                }
            }
            _ => {}
        }
    }
    battery
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod os {
    use super::PowerSource;

    pub(super) fn power_source() -> Option<PowerSource> {
        super::power_source_from_sysfs(std::path::Path::new("/sys/class/power_supply"))
    }
}

#[cfg(windows)]
mod os {
    use super::PowerSource;

    const AC_ONLINE: u8 = 1;
    const NO_SYSTEM_BATTERY: u8 = 128;
    const UNKNOWN_PERCENT: u8 = 255;
    const BATTERY_SAVER_ON: u8 = 1;

    #[repr(C)]
    #[derive(Default)]
    struct SystemPowerStatus {
        ac_line_status: u8,
        battery_flag: u8,
        battery_life_percent: u8,
        system_status_flag: u8,
        battery_life_time: u32,
        battery_full_life_time: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }

    pub(super) fn power_source() -> Option<PowerSource> {
        let mut status = SystemPowerStatus::default();
        // SAFETY: `status` is a live, writable SYSTEM_POWER_STATUS of the documented layout.
        if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
            return None;
        }
        if status.battery_flag == NO_SYSTEM_BATTERY {
            return None;
        }
        if status.ac_line_status == AC_ONLINE {
            return Some(PowerSource::Mains);
        }
        let percent = (status.battery_life_percent != UNKNOWN_PERCENT).then_some(status.battery_life_percent);
        Some(super::on_battery(percent, status.system_status_flag == BATTERY_SAVER_ON))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
mod os {
    use super::PowerSource;

    pub(super) fn power_source() -> Option<PowerSource> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        for (file, text) in files {
            std::fs::write(directory.join(file), format!("{text}\n")).unwrap();
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("eggdb-power-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_charger_online_means_the_mains() {
        let root = scratch("charger");
        supply(&root, "AC", &[("type", "Mains"), ("online", "1")]);
        supply(&root, "BAT0", &[("type", "Battery"), ("status", "Charging"), ("capacity", "10")]);
        assert_eq!(power_source_from_sysfs(&root), Some(PowerSource::Mains));
    }

    #[test]
    fn a_discharging_battery_is_low_at_twenty_percent() {
        let root = scratch("battery");
        supply(&root, "AC", &[("type", "Mains"), ("online", "0")]);
        supply(&root, "BAT0", &[("type", "Battery"), ("status", "Discharging"), ("capacity", "55")]);
        assert_eq!(power_source_from_sysfs(&root), Some(PowerSource::Battery));
        supply(&root, "BAT0", &[("capacity", "20")]);
        assert_eq!(power_source_from_sysfs(&root), Some(PowerSource::LowBattery));
    }

    #[test]
    fn a_mouse_battery_and_no_supplies_say_nothing() {
        let root = scratch("none");
        assert_eq!(power_source_from_sysfs(&root), None);
        supply(&root, "hid-mouse", &[("type", "Battery"), ("scope", "Device"), ("status", "Discharging"), ("capacity", "5")]);
        assert_eq!(power_source_from_sysfs(&root), None);
    }

    #[test]
    fn the_battery_saver_makes_any_charge_low() {
        assert_eq!(on_battery(Some(90), true), PowerSource::LowBattery);
        assert_eq!(on_battery(None, false), PowerSource::Battery);
    }
}
