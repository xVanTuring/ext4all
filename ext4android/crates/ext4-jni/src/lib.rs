//! JNI entry points of `libext4android.so`, declared in
//! `tech.xvanturing.ext4android.jni.Native`.
//!
//! Conventions:
//! - ext4 names are arbitrary bytes: paths cross as the encoded strings of
//!   [`names`], never as plain Java strings of the names;
//! - structured results are little-endian byte arrays (see
//!   [`docs::encode_entries`], [`volumes::info`]);
//! - errors and panics never cross the boundary: file system errors become
//!   `FileNotFoundException` (missing, not a directory) or `Ext4Exception`
//!   (an IOException with the Linux errno), panics a `RuntimeException`.

mod docs;
mod errors;
mod files;
#[cfg(target_os = "android")]
mod logger;
mod names;
mod ops;
mod sample;
mod selftest;
// usbdevfs is Linux only; the probe is tested on any host
#[cfg(any(target_os = "linux", target_os = "android", test))]
mod usb;
mod volumes;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JClass, JLongArray, JString};
use jni::strings::JNIString;
use jni::sys::{JNI_VERSION_1_6, jboolean, jint, jlong};
use jni::{Env, EnvUnowned, jni_str};
use std::ffi::c_void;
use std::path::Path;

/// A failed call: file system errors are thrown as Java exceptions, JNI
/// errors are passed on.
enum Failure {
    Fs(ext4_core::Error),
    Jni(jni::errors::Error),
}

impl From<ext4_core::Error> for Failure {
    fn from(e: ext4_core::Error) -> Self {
        Failure::Fs(e)
    }
}

impl From<jni::errors::Error> for Failure {
    fn from(e: jni::errors::Error) -> Self {
        Failure::Jni(e)
    }
}

fn throw(env: &mut Env<'_>, e: &ext4_core::Error) -> jni::errors::Result<()> {
    match e {
        ext4_core::Error::NotFound | ext4_core::Error::NotDir => env.throw_new(
            jni_str!("java/io/FileNotFoundException"),
            JNIString::from(e.to_string()),
        ),
        // "<errno> <message>": Ext4Exception splits it
        _ => env.throw_new(
            jni_str!("tech/xvanturing/ext4android/jni/Ext4Exception"),
            JNIString::from(format!("{} {e}", errors::linux_errno(e))),
        ),
    }
}

/// Run `f`; a file system error is thrown and `T::default()` (null, 0)
/// returned.
fn call<'local, T: Default>(
    env: &mut Env<'local>,
    f: impl FnOnce(&mut Env<'local>) -> Result<T, Failure>,
) -> jni::errors::Result<T> {
    match f(env) {
        Ok(v) => Ok(v),
        Err(Failure::Fs(e)) => {
            log::debug!("call failed: {e}");
            throw(env, &e)?;
            Ok(T::default())
        }
        Err(Failure::Jni(e)) => Err(e),
    }
}

fn string(env: &mut Env<'_>, s: &JString<'_>) -> Result<String, Failure> {
    Ok(s.try_to_string(env)?)
}

fn volume(id: jint) -> Result<std::sync::Arc<volumes::Volume>, Failure> {
    Ok(volumes::get(id as u32)?)
}

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

/// Create (or replace) the sample image of the debug build.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_createSampleImage<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    path: JString<'caller>,
    size_mib: jint,
) {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            Ok(sample::create(Path::new(&path), size_mib.max(16) as u32)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Copy the open file `fd` (owned by the caller) into the root of the
/// unmounted image as `name`; returns the bytes copied.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_importIntoImage<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    image: JString<'caller>,
    fd: jint,
    name: JString<'caller>,
) -> jlong {
    use std::os::fd::FromRawFd;
    env.with_env(|env| {
        call(env, |env| {
            let image = string(env, &image)?;
            let name = string(env, &name)?;
            // SAFETY: the caller keeps `fd` open for the call; ManuallyDrop
            // leaves closing it to the caller
            let mut src = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
            Ok(sample::import(Path::new(&image), name.as_bytes(), &mut *src)? as jlong)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Mount the ext4 volume of an image file; returns its volume number.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_mountImage<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    path: JString<'caller>,
    read_only: jboolean,
) -> jint {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            Ok(volumes::mount_image(Path::new(&path), read_only)? as jint)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Mount every ext2/3/4 volume of a USB disk through usbdevfs on `fd` (a
/// `UsbDeviceConnection` with `interface` claimed, kept open until all the
/// volumes are unmounted or abandoned); returns their volume numbers.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_mountUsb<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    fd: jint,
    interface: jint,
    endpoint_in: jint,
    endpoint_out: jint,
    read_only: jboolean,
) -> jni::objects::JIntArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let t = usb_msc::usbfs::UsbFs::new(fd, interface as u8, endpoint_in as u8, endpoint_out as u8);
            let ids: Vec<jint> = usb::mount(t, read_only)?.into_iter().map(|id| id as jint).collect();
            let out = env.new_int_array(ids.len())?;
            out.set_region(env, 0, &ids)?;
            Ok(out)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_unmount<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    volume: jint,
) {
    env.with_env(|env| call(env, |_| Ok(volumes::unmount(volume as u32)?)))
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Forget a volume whose disk is gone, writing nothing more.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_abandon<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    volume: jint,
) {
    env.with_env(|env| call(env, |_| Ok(volumes::abandon(volume as u32)?)))
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Label, UUID, sizes and state of a mounted volume (see [`volumes::info`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_volumeInfo<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
) -> JByteArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let info = volumes::info(&*volume(id)?)?;
            Ok(env.byte_array_from_slice(&info)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// MB/s reading the first `mib` MiB of the volume's device directly (see
/// [`volumes::device_read_speed`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_deviceReadSpeed<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    mib: jint,
) -> jni::sys::jdouble {
    env.with_env(|env| {
        call(env, |_| {
            Ok(volumes::device_read_speed(&*volume(id)?, (mib.max(0) as u64) << 20)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// MB/s reading the regular file at an encoded path through the file
/// system, without the documents provider (see
/// [`volumes::file_read_speed`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_fileReadSpeed<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
) -> jni::sys::jdouble {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            Ok(volumes::file_read_speed(&*volume(id)?, &path)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// MB/s writing `mib` MiB to a new file in writes of `chunk_kib` KiB,
/// without the documents provider (see [`volumes::file_write_speed`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_fileWriteSpeed<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    mib: jint,
    chunk_kib: jint,
) -> jni::sys::jdouble {
    env.with_env(|env| {
        call(env, |_| {
            let bytes = (mib.max(0) as u64) << 20;
            let chunk = (chunk_kib.max(1) as usize) << 10;
            Ok(volumes::file_write_speed(&*volume(id)?, bytes, chunk)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// The USB transfers of the volume's disk: `[bytes per transfer, times
/// halved]`, or empty when it is not on a USB disk.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_usbTransfers<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
) -> JLongArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let state: Vec<jlong> = volumes::usb_transfers(&*volume(id)?)
                .map(|(size, halved)| vec![size as jlong, halved as jlong])
                .unwrap_or_default();
            let out = env.new_long_array(state.len())?;
            out.set_region(env, 0, &state)?;
            Ok(out)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// The document at an encoded path, as one entry (see
/// [`docs::encode_entries`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_stat<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
) -> JByteArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let e = volume(id)?.fs.with(|fs| docs::stat(fs, &path))?;
            Ok(env.byte_array_from_slice(&docs::encode_entries(&[e]))?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// The documents in a directory (see [`docs::encode_entries`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_list<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
) -> JByteArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let entries = volume(id)?.fs.with(|fs| docs::list(fs, &path))?;
            Ok(env.byte_array_from_slice(&docs::encode_entries(&entries))?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Open a regular file for a descriptor (emptied first with `truncate`):
/// `[handle, size]`. Every open is paired with `closeFile`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_openFile<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
    truncate: jboolean,
) -> JLongArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let (handle, size) = ops::open(&*volume(id)?, &path, truncate)?;
            let out = env.new_long_array(2)?;
            out.set_region(env, 0, &[handle as jlong, size as jlong])?;
            Ok(out)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// A descriptor from `openFile` was closed; `written` commits.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_closeFile<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    handle: jlong,
    written: jboolean,
) {
    env.with_env(|env| call(env, |_| Ok(volume(id)?.close_file(handle as u64, written)?)))
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Write `len` bytes of `buf` at `offset` through the descriptor `handle`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_write<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    handle: jlong,
    offset: jlong,
    buf: JByteArray<'caller>,
    len: jint,
) {
    env.with_env(|env| {
        call(env, |env| {
            let mut signed = vec![0i8; len.max(0) as usize];
            buf.get_region(env, 0, &mut signed)?;
            // SAFETY: i8 and u8 have the same size and alignment
            let data = unsafe { std::slice::from_raw_parts(signed.as_ptr() as *const u8, signed.len()) };
            let v = volume(id)?;
            let ino = v.file(handle as u64)?.ino;
            Ok(ops::write(&v, ino, offset.max(0) as u64, data)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_fileSize<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    handle: jlong,
) -> jlong {
    env.with_env(|env| {
        call(env, |_| {
            let v = volume(id)?;
            let ino = v.file(handle as u64)?.ino;
            Ok(ops::size(&v, ino)? as jlong)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// fsync: commit what was written so far.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_syncVolume<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
) {
    env.with_env(|env| call(env, |_| Ok(ops::sync(&*volume(id)?)?)))
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// Create a file or directory (a taken name gets a number); one entry
/// record, its name is the new last path component.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_createDocument<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    parent: JString<'caller>,
    name: JString<'caller>,
    directory: jboolean,
) -> JByteArray<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let parent = string(env, &parent)?;
            let name = string(env, &name)?;
            let (entry, _) = ops::create(&*volume(id)?, &parent, &name, directory)?;
            Ok(env.byte_array_from_slice(&docs::encode_entries(&[entry]))?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Delete a document (a directory with everything in it).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_deleteDocument<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
) {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            Ok(ops::delete(&*volume(id)?, &path)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Rename in place; returns the new encoded path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_renameDocument<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
    name: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let name = string(env, &name)?;
            let new = ops::rename(&*volume(id)?, &path, &name)?;
            Ok(JString::from_str(env, new)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Move into another directory; returns the new encoded path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_moveDocument<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
    target: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let target = string(env, &target)?;
            let new = ops::move_to(&*volume(id)?, &path, &target)?;
            Ok(JString::from_str(env, new)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Copy into a directory (a taken name gets a number); returns the copy's
/// encoded path.
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_copyDocument<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    path: JString<'caller>,
    target: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        call(env, |env| {
            let path = string(env, &path)?;
            let target = string(env, &target)?;
            let new = ops::copy(&*volume(id)?, &path, &target)?;
            Ok(JString::from_str(env, new)?)
        })
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Read up to `len` bytes at `offset` through the descriptor `handle` into
/// `buf`; returns the bytes read (0 at the end of the file). Reads in order
/// are read ahead (see [`files`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_tech_xvanturing_ext4android_jni_Native_read<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    id: jint,
    handle: jlong,
    offset: jlong,
    buf: JByteArray<'caller>,
    len: jint,
) -> jint {
    env.with_env(|env| {
        call(env, |env| {
            let mut data = vec![0u8; len.max(0) as usize];
            let v = volume(id)?;
            let n = v.file(handle as u64)?.read(&v, offset.max(0) as u64, &mut data)?;
            // SAFETY: u8 and i8 have the same size and alignment
            let signed = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const i8, n) };
            buf.set_region(env, 0, signed)?;
            Ok(n as jint)
        })
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
