use crate::glue;

use std::fs::{self, OpenOptions};
use std::io::Error;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

const EVENT_PATH: &str = "/dev/input";

// Every keyboard with the light gets it, whether rkvm has taken the device over or not.
// A remapper such as keyd sits between rkvm and the real keyboard, and only the real
// one has a bulb attached.
pub fn set_caps_lock(on: bool) -> Result<usize, Error> {
    let value = match on {
        true => glue::libevdev_led_value_LIBEVDEV_LED_ON,
        false => glue::libevdev_led_value_LIBEVDEV_LED_OFF,
    };

    let mut count = 0;

    for entry in fs::read_dir(EVENT_PATH)? {
        let path = entry?.path();

        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or(false, |name| name.starts_with("event"));

        if !named {
            continue;
        }

        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };

        let mut evdev = MaybeUninit::uninit();
        let ret = unsafe { glue::libevdev_new_from_fd(file.as_raw_fd(), evdev.as_mut_ptr()) };
        if ret < 0 {
            continue;
        }

        let evdev = match NonNull::new(unsafe { evdev.assume_init() }) {
            Some(evdev) => evdev,
            None => continue,
        };

        let supported = unsafe {
            glue::libevdev_has_event_code(evdev.as_ptr(), glue::EV_LED, glue::LED_CAPSL)
        };

        if supported == 1 {
            let ret =
                unsafe { glue::libevdev_kernel_set_led_value(evdev.as_ptr(), glue::LED_CAPSL, value) };

            if ret == 0 {
                count += 1;
            } else {
                tracing::debug!(path = ?path, "Failed to set caps lock LED: {}", -ret);
            }
        }

        unsafe { glue::libevdev_free(evdev.as_ptr()) };
    }

    Ok(count)
}
