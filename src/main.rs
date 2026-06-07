// Recommended main loop showing how to drive the now-fallible APIs.
// Adjust the module paths / VID:PID to match your project.

mod physical_device;
mod virtual_device;

use physical_device::PhysicalDevice;
use std::thread::sleep;
use std::time::Duration;
use virtual_device::{DeviceDispatcher, RawDataReader};

// TODO: set these to your tablet's real USB IDs (see `lsusb`).
const VID: u16 = 0x08f2;
const PID: u16 = 0x6811;

fn main() {
    // Outer loop: if the device disappears (unplug / USB reset), we tear down
    // and rebuild instead of crashing the whole process.
    loop {
        if let Err(e) = run() {
            eprintln!("driver session ended with error: {e}; restarting in 2s...");
            sleep(Duration::from_secs(2));
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut physical = PhysicalDevice::new(VID, PID)?;
    physical.init()?;
    physical.set_full_mode()?;

    let mut dispatcher = DeviceDispatcher::new()?;
    let mut reader = RawDataReader::new();

    // Set MX002_DEBUG=1 in the environment to dump raw packets for mapping.
    let debug = std::env::var("MX002_DEBUG").is_ok();

    println!("The driver is running.");

    loop {
        match physical.read_device_responses(&mut reader.data) {
            Ok(n) => {
                // ---- TEMP DEBUG: dump the bytes used by RawDataReader ----
                // Shows pen-button byte (9) and tablet-button bytes (11,12)
                // so each physical control's raw id/bit can be identified.
                if debug {
                    let end = 14.min(n);
                    eprintln!(
                        "raw[0..{end}]={:02x?}  pen[9]={:#04x}  tab[11,12]={:#04x},{:#04x}",
                        &reader.data[..end],
                        reader.data[9],
                        reader.data[11],
                        reader.data[12],
                    );
                }
                // ----------------------------------------------------------

                // A transient dispatch error (e.g. a single failed emit) is
                // logged but does not kill the session.
                if let Err(e) = dispatcher.dispatch(&reader) {
                    eprintln!("dispatch error (continuing): {e}");
                }
                dispatcher.syn()?;
            }
            // A read timeout is normal when the pen is idle — just loop.
            Err(rusb::Error::Timeout) => continue,
            // Device went away: bubble up so the outer loop rebuilds it.
            Err(rusb::Error::NoDevice) | Err(rusb::Error::NotFound) => {
                return Err("device disconnected".into());
            }
            Err(e) => {
                eprintln!("read error (continuing): {e}");
                sleep(Duration::from_millis(50));
            }
        }
    }
}
