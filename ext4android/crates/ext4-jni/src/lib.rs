//! JNI entry points of `libext4android.so`, declared in
//! `tech.xvanturing.ext4android.jni.Native`.
//!
//! Conventions:
//! - names are passed as byte arrays, never as Java strings: ext4 names are
//!   arbitrary bytes;
//! - errors and panics never cross the boundary: they become Java
//!   exceptions.

#[cfg(target_os = "android")]
mod logger;
mod selftest;
// usbdevfs is Linux only; the probe is tested on any host
#[cfg(any(target_os = "linux", target_os = "android", test))]
mod usb;

use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JClass, JString};
use jni::sys::{JNI_VERSION_1_6, jint};
use std::ffi::c_void;

/// Runs once when `System.loadLibrary` loads this library.
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(_vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    #[cfg(target_os = "android")]
    logger::init();
    log::info!("{} loaded", version());
    JNI_VERSION_1_6
}

fn version() -> String {
    format!("ext4android {}", env!("CARGO_PKG_VERSION"))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_version<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| JString::from_str(env, version()))
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Format, write, remount and read back a small in-memory volume.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_selfTest<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        let text = match selftest::run() {
            Ok(report) => report,
            Err(e) => {
                log::error!("self test failed: {e}");
                format!("failed: {e}")
            }
        };
        JString::from_str(env, text)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Experiment M0: drive a USB disk through usbdevfs on the descriptor of a
/// `UsbDeviceConnection` whose interface is claimed; read-only. Returns a
/// text report.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_usbProbe<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    fd: jint,
    interface: jint,
    endpoint_in: jint,
    endpoint_out: jint,
) -> JString<'caller> {
    env.with_env(|env| {
        let t = usb_msc::usbfs::UsbFs::new(fd, interface as u8, endpoint_in as u8, endpoint_out as u8);
        let report = usb::probe(t);
        log::info!("USB probe:\n{report}");
        JString::from_str(env, report)
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}
