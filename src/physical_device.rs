use rusb::{
    devices, ConfigDescriptor, Device, DeviceHandle, Error as RusbError, GlobalContext,
    InterfaceDescriptor, TransferType,
};

use std::thread::sleep;
use std::time::Duration;

/// Control-transfer parameters for the "full mode" feature report.
const SET_REPORT_REQUEST_TYPE: u8 = 0x21; // Host-to-device | Class | Interface
const SET_REPORT_REQUEST: u8 = 0x09; // HID SET_REPORT
const SET_REPORT_VALUE: u16 = 0x0308; // Report type (Feature=3) << 8 | Report ID (8)
const SET_REPORT_INTERFACE: u16 = 2;
const CONTROL_TIMEOUT: Duration = Duration::from_millis(250);
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// How many times to retry opening / resetting the device while libusb is
/// still re-enumerating it after a previous instance crashed or exited.
const OPEN_RETRIES: u32 = 10;
const RETRY_DELAY: Duration = Duration::from_millis(500);

pub struct PhysicalDevice {
    device: Device<GlobalContext>,
    device_handle: DeviceHandle<GlobalContext>,
    endpoint_address: u8,
}

impl PhysicalDevice {
    /// Open the target device, retrying while the USB stack settles.
    ///
    /// Previously this used `.expect(...)`, so a transient `NotFound` /
    /// `Access` / `NoDevice` during re-enumeration after a crash caused an
    /// immediate `SIGABRT` core-dump. Now we retry a bounded number of times
    /// and return a `Result` so the caller (main loop) can decide what to do.
    pub fn new(vid: u16, pid: u16) -> Result<Self, RusbError> {
        let mut last_err = RusbError::NoDevice;

        for attempt in 1..=OPEN_RETRIES {
            match Self::get_target_device(vid, pid) {
                Ok(device) => match device.open() {
                    Ok(device_handle) => {
                        return Ok(PhysicalDevice {
                            endpoint_address: 0,
                            device_handle,
                            device,
                        });
                    }
                    Err(e) => {
                        eprintln!(
                            "open attempt {attempt}/{OPEN_RETRIES} failed: {e}; retrying..."
                        );
                        last_err = e;
                    }
                },
                Err(e) => {
                    eprintln!(
                        "find attempt {attempt}/{OPEN_RETRIES} failed: {e}; retrying..."
                    );
                    last_err = e;
                }
            }
            sleep(RETRY_DELAY);
        }

        Err(last_err)
    }

    /// Detach the kernel driver, claim the HID interface(s), find the IN
    /// interrupt endpoint and reset the device. Any failure is propagated
    /// instead of panicking.
    pub fn init(&mut self) -> Result<&mut Self, RusbError> {
        self.device_handle
            .set_auto_detach_kernel_driver(true)?;

        let configurations = Self::get_configurations(&self.device)?;
        let interface_descriptors = Self::get_hid_interface_descriptors(&configurations);

        for interface_descriptor in interface_descriptors {
            self.device_handle
                .claim_interface(interface_descriptor.interface_number())?;

            for endpoint_descriptor in interface_descriptor.endpoint_descriptors() {
                if endpoint_descriptor.transfer_type() == TransferType::Interrupt
                    && endpoint_descriptor.max_packet_size() == 64
                {
                    self.endpoint_address = endpoint_descriptor.address();
                }
            }
        }

        self.reset()?;
        Ok(self)
    }

    /// Reset the device handle, tolerating a transient `NotFound` by waiting
    /// for re-enumeration and retrying once.
    ///
    /// Fixes the original bug where this referenced a non-existent local
    /// `device` variable and used `?` inside a `-> ()` function (which would
    /// not compile), and the `physical_device.rs:50` `NotFound` core-dump.
    pub fn reset(&mut self) -> Result<(), RusbError> {
        match self.device_handle.reset() {
            Ok(()) => Ok(()),
            Err(RusbError::NotFound) => {
                // Device is mid re-enumeration. Give the kernel a moment and
                // try once more before giving up.
                sleep(RETRY_DELAY);
                self.device_handle.reset()
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_device_responses(&self, buffer: &mut [u8]) -> Result<usize, RusbError> {
        self.device_handle
            .read_interrupt(self.endpoint_address, buffer, READ_TIMEOUT)
    }

    /// Put the tablet into "full" reporting mode by sending the feature report.
    ///
    /// Fixes the original syntax error where a stray `write_interrupt` match
    /// block (referencing undefined `device`/`endpoint`/`report`) was pasted
    /// inline. All report sending now goes through `set_report`, whose result
    /// is logged here rather than panicking.
    pub fn set_full_mode(&mut self) -> Result<&mut Self, RusbError> {
        const REPORTS: [[u8; 8]; 1] = [[0x08, 0x03, 0x00, 0xff, 0xf0, 0x00, 0xff, 0xf0]];
        let reports_as_slices: Vec<&[u8]> = REPORTS.iter().map(|r| &r[..]).collect();
        self.set_report(&reports_as_slices)?;
        Ok(self)
    }

    pub fn set_report(&mut self, reports: &[&[u8]]) -> Result<(), RusbError> {
        for report in reports.iter() {
            self.device_handle.write_control(
                SET_REPORT_REQUEST_TYPE,
                SET_REPORT_REQUEST,
                SET_REPORT_VALUE,
                SET_REPORT_INTERFACE,
                report,
                CONTROL_TIMEOUT,
            )?;
        }

        Ok(())
    }

    fn is_target_device(vid: u16, pid: u16, device: &Device<GlobalContext>) -> bool {
        match device.device_descriptor() {
            Ok(descriptor) => descriptor.vendor_id() == vid && descriptor.product_id() == pid,
            Err(_) => false,
        }
    }

    fn get_target_device(vid: u16, pid: u16) -> Result<Device<GlobalContext>, RusbError> {
        devices()?
            .iter()
            .find(|device| Self::is_target_device(vid, pid, device))
            .ok_or(RusbError::NoDevice)
    }

    fn get_hid_interface_descriptors(
        config_descriptors: &[ConfigDescriptor],
    ) -> Vec<InterfaceDescriptor> {
        config_descriptors
            .iter()
            .flat_map(|config_descriptor| config_descriptor.interfaces())
            .flat_map(|interface| interface.descriptors())
            .filter(|interface_descriptor| {
                interface_descriptor.class_code() == rusb::constants::LIBUSB_CLASS_HID
            })
            .collect()
    }

    fn get_configurations(
        device: &Device<GlobalContext>,
    ) -> Result<Vec<ConfigDescriptor>, RusbError> {
        let device_descriptor = device.device_descriptor()?;
        Ok((0..device_descriptor.num_configurations())
            .filter_map(|n| device.config_descriptor(n).ok())
            .collect())
    }
}
