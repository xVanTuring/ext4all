import Ext4FFI
import Foundation

/// Converts an errno returned by the engine into a Swift error.
@inline(__always)
func ext4Check(_ rc: Int32) throws {
    if rc != 0 {
        throw POSIXError(POSIXErrorCode(rawValue: rc) ?? .EIO)
    }
}

func errnoValue(of error: Error) -> Int32 {
    if let p = error as? POSIXError {
        return p.code.rawValue
    }
    let ns = error as NSError
    if ns.domain == NSPOSIXErrorDomain {
        return Int32(ns.code)
    }
    return EIO
}

/// Holds a `BlockIO` for the lifetime of the engine's use of it.
private final class DeviceBox {
    let io: BlockIO
    init(_ io: BlockIO) { self.io = io }
}

private func box(_ ctx: UnsafeMutableRawPointer?) -> DeviceBox {
    Unmanaged<DeviceBox>.fromOpaque(ctx!).takeUnretainedValue()
}

/// Build C device callbacks for `io`. The returned ops retain `io` until the
/// engine calls `release` (or `releaseOps` is called for probe-only use).
private func makeOps(_ io: BlockIO) -> Ext4DeviceOps {
    let ctx = Unmanaged.passRetained(DeviceBox(io)).toOpaque()
    return Ext4DeviceOps(
        ctx: ctx,
        read: { ctx, offset, buf, len in
            do {
                try box(ctx).io.read(at: offset, into: UnsafeMutableRawBufferPointer(start: buf, count: Int(len)))
                return 0
            } catch {
                return errnoValue(of: error)
            }
        },
        write: { ctx, offset, buf, len in
            do {
                try box(ctx).io.write(at: offset, from: UnsafeRawBufferPointer(start: buf, count: Int(len)))
                return 0
            } catch {
                return errnoValue(of: error)
            }
        },
        flush: { ctx in
            do {
                try box(ctx).io.flush()
                return 0
            } catch {
                return errnoValue(of: error)
            }
        },
        release: { ctx in
            Unmanaged<DeviceBox>.fromOpaque(ctx!).release()
        },
        size: io.size,
        sector_size: io.sectorSize,
        read_only: io.isReadOnly
    )
}

private func releaseOps(_ ops: Ext4DeviceOps) {
    Unmanaged<DeviceBox>.fromOpaque(ops.ctx).release()
}

/// What `probe` found on a device.
public struct Ext4VolumeInfo: Equatable, Sendable {
    public enum Support: Equatable, Sendable {
        case readWrite, readOnly, unsupported
    }

    public var label: String
    public var uuid: UUID
    public var blockSize: UInt32
    public var blocks: UInt64
    public var support: Support
    public var needsRecovery: Bool
    public var hasJournal: Bool
    /// 0 = ext2, 1 = ext3, 2 = ext4.
    public var subtype: Int

    init(_ p: Ext4ProbeInfo) {
        var labelBytes = withUnsafeBytes(of: p.label) { Array($0) }
        if let nul = labelBytes.firstIndex(of: 0) {
            labelBytes.removeSubrange(nul...)
        }
        label = String(decoding: labelBytes, as: UTF8.self)
        uuid = withUnsafeBytes(of: p.uuid) { raw in
            UUID(uuid: raw.load(as: uuid_t.self))
        }
        blockSize = p.block_size
        blocks = p.blocks
        switch p.support {
        case EXT4_SUPPORT_READ_WRITE: support = .readWrite
        case EXT4_SUPPORT_READ_ONLY: support = .readOnly
        default: support = .unsupported
        }
        needsRecovery = p.needs_recovery
        hasJournal = p.has_journal
        subtype = Int(p.subtype)
    }
}

/// Swift view of a directory entry.
public struct Ext4DirEntry {
    public var name: Data
    public var ino: UInt32
    public var fileType: UInt8
    public var nextCookie: UInt64
    public var attr: Ext4Attr?
}

/// A mounted ext4 volume. All methods are thread safe (the engine
/// serializes operations internally).
public final class Ext4Mount: @unchecked Sendable {
    private var handle: OpaquePointer?
    public let isReadOnly: Bool

    public static let rootIno: UInt32 = 2

    public static var version: String {
        String(cString: ext4_version())
    }

    /// Inspect a device without mounting it.
    public static func probe(_ io: BlockIO) throws -> Ext4VolumeInfo {
        var ops = makeOps(io)
        defer { releaseOps(ops) }
        var info = Ext4ProbeInfo()
        try ext4Check(ext4_probe(&ops, &info))
        return Ext4VolumeInfo(info)
    }

    public init(_ io: BlockIO, readOnly: Bool, commitIntervalSeconds: UInt32 = 5) throws {
        var ops = makeOps(io)
        var opts = Ext4MountOptions(
            read_only: readOnly || io.isReadOnly,
            cache_blocks: 0,
            commit_interval_secs: commitIntervalSeconds,
            defer_unlinked: true
        )
        var h: OpaquePointer?
        // on failure the engine has already released the device
        try ext4Check(ext4_mount(&ops, &opts, &h))
        handle = h
        isReadOnly = ext4_is_read_only(h)
    }

    deinit {
        if let h = handle {
            ext4_close(h)
        }
    }

    public func volumeInfo() throws -> Ext4VolumeInfo {
        var info = Ext4ProbeInfo()
        try ext4Check(ext4_volume_info(handle, &info))
        return Ext4VolumeInfo(info)
    }

    public func statfs() throws -> Ext4StatFs {
        var s = Ext4StatFs()
        try ext4Check(ext4_statfs(handle, &s))
        return s
    }

    public func sync() throws {
        try ext4Check(ext4_sync(handle))
    }

    /// Commit everything and mark the volume clean.
    public func unmount() throws {
        try ext4Check(ext4_unmount(handle))
    }

    /// Commit and mark clean, but keep the volume open read-only so late
    /// reclaims and attribute requests still work (FSKit unmount).
    public func finish() throws {
        try ext4Check(ext4_finish(handle))
    }

    /// Make a finished volume writable again.
    public func remount() throws {
        try ext4Check(ext4_remount(handle))
    }

    /// Whether the volume currently refuses writes (read-only mount, a
    /// failed commit, or after `finish`).
    public var isCurrentlyReadOnly: Bool {
        ext4_is_read_only(handle)
    }

    public func setLabel(_ label: String) throws {
        let d = Data(label.utf8)
        try d.withUnsafeBytes { b in
            try ext4Check(ext4_set_label(handle, b.bindMemory(to: UInt8.self).baseAddress, d.count))
        }
    }

    public func stat(_ ino: UInt32) throws -> Ext4Attr {
        var a = Ext4Attr()
        try ext4Check(ext4_stat(handle, ino, &a))
        return a
    }

    private func withName<T>(_ name: Data, _ body: (UnsafePointer<UInt8>?, Int) throws -> T) rethrows -> T {
        try name.withUnsafeBytes { b in
            try body(b.bindMemory(to: UInt8.self).baseAddress, name.count)
        }
    }

    public func lookup(_ dir: UInt32, _ name: Data) throws -> Ext4Attr {
        var a = Ext4Attr()
        try withName(name) { p, n in try ext4Check(ext4_lookup(handle, dir, p, n, &a)) }
        return a
    }

    private final class DirContext {
        let body: (Ext4DirEntry) -> Bool
        init(_ body: @escaping (Ext4DirEntry) -> Bool) { self.body = body }
    }

    /// Enumerate `dir` from `cookie`; `body` returns false to stop. The
    /// callback runs with the volume locked: it must not call back into
    /// this mount.
    public func readDir(
        _ dir: UInt32, cookie: UInt64, skipDots: Bool, wantAttrs: Bool,
        _ body: @escaping (Ext4DirEntry) -> Bool
    ) throws {
        let ctx = DirContext(body)
        let raw = Unmanaged.passUnretained(ctx).toOpaque()
        let rc = ext4_readdir(
            handle, dir, cookie, skipDots, wantAttrs,
            { ctx, name, len, ino, ft, next, attr in
                let c = Unmanaged<DirContext>.fromOpaque(ctx!).takeUnretainedValue()
                let entry = Ext4DirEntry(
                    name: Data(bytes: name!, count: Int(len)),
                    ino: ino,
                    fileType: ft,
                    nextCookie: next,
                    attr: attr?.pointee
                )
                return c.body(entry)
            }, raw)
        withExtendedLifetime(ctx) {}
        try ext4Check(rc)
    }

    public func read(_ ino: UInt32, offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws -> Int {
        var n = 0
        try ext4Check(ext4_read(handle, ino, offset, buffer.bindMemory(to: UInt8.self).baseAddress, buffer.count, &n))
        return n
    }

    public func read(_ ino: UInt32, offset: UInt64, length: Int) throws -> Data {
        var d = Data(count: length)
        let n = try d.withUnsafeMutableBytes { try read(ino, offset: offset, into: $0) }
        d.count = n
        return d
    }

    public func write(_ ino: UInt32, offset: UInt64, data: Data) throws -> Int {
        var n = 0
        try data.withUnsafeBytes { b in
            try ext4Check(ext4_write(handle, ino, offset, b.bindMemory(to: UInt8.self).baseAddress, data.count, &n))
        }
        return n
    }

    public func create(
        _ dir: UInt32, _ name: Data, type: UInt8, perm: UInt16, uid: UInt32, gid: UInt32, rdev: UInt32 = 0
    ) throws -> Ext4Attr {
        var a = Ext4Attr()
        try withName(name) { p, n in
            try ext4Check(ext4_create(handle, dir, p, n, type, perm, uid, gid, rdev, &a))
        }
        return a
    }

    public func symlink(_ dir: UInt32, _ name: Data, target: Data, uid: UInt32, gid: UInt32) throws -> Ext4Attr {
        var a = Ext4Attr()
        try withName(name) { p, n in
            try target.withUnsafeBytes { t in
                try ext4Check(
                    ext4_symlink(
                        handle, dir, p, n, t.bindMemory(to: UInt8.self).baseAddress, target.count, uid, gid, &a))
            }
        }
        return a
    }

    public func readLink(_ ino: UInt32) throws -> Data {
        var len = 0
        try ext4Check(ext4_readlink(handle, ino, nil, 0, &len))
        var d = Data(count: len)
        try d.withUnsafeMutableBytes { b in
            try ext4Check(ext4_readlink(handle, ino, b.bindMemory(to: UInt8.self).baseAddress, len, &len))
        }
        return d
    }

    public func link(_ ino: UInt32, to dir: UInt32, name: Data) throws -> Ext4Attr {
        var a = Ext4Attr()
        try withName(name) { p, n in try ext4Check(ext4_link(handle, ino, dir, p, n, &a)) }
        return a
    }

    public func remove(_ dir: UInt32, _ name: Data) throws {
        try withName(name) { p, n in try ext4Check(ext4_remove(handle, dir, p, n)) }
    }

    public func rename(_ srcDir: UInt32, _ srcName: Data, _ dstDir: UInt32, _ dstName: Data, flags: UInt32 = 0) throws {
        try withName(srcName) { sp, sn in
            try withName(dstName) { dp, dn in
                try ext4Check(ext4_rename(handle, srcDir, sp, sn, dstDir, dp, dn, flags))
            }
        }
    }

    public func setAttr(_ ino: UInt32, _ req: Ext4SetAttr) throws -> Ext4Attr {
        var r = req
        var a = Ext4Attr()
        try ext4Check(ext4_setattr(handle, ino, &r, &a))
        return a
    }

    public func reclaim(_ ino: UInt32) throws {
        try ext4Check(ext4_reclaim(handle, ino))
    }

    public func fallocate(_ ino: UInt32, offset: UInt64, length: UInt64, keepSize: Bool) throws {
        try ext4Check(ext4_fallocate(handle, ino, offset, length, keepSize))
    }

    public func punchHole(_ ino: UInt32, offset: UInt64, length: UInt64) throws {
        try ext4Check(ext4_punch_hole(handle, ino, offset, length))
    }

    /// Byte offset past the last allocated block (physical end of file).
    public func allocatedEnd(_ ino: UInt32) throws -> UInt64 {
        var out: UInt64 = 0
        try ext4Check(ext4_allocated_end(handle, ino, &out))
        return out
    }

    /// SEEK_DATA (`data == true`) or SEEK_HOLE.
    public func seek(_ ino: UInt32, from offset: UInt64, data: Bool) throws -> UInt64 {
        var out: UInt64 = 0
        try ext4Check(ext4_seek(handle, ino, offset, data, &out))
        return out
    }

    // MARK: extended attributes (macOS names)

    public func getXattr(_ ino: UInt32, _ name: Data) throws -> Data {
        var len = 0
        try withName(name) { p, n in try ext4Check(ext4_getxattr(handle, ino, p, n, nil, 0, &len)) }
        var d = Data(count: len)
        try withName(name) { p, n in
            try d.withUnsafeMutableBytes { b in
                try ext4Check(ext4_getxattr(handle, ino, p, n, b.bindMemory(to: UInt8.self).baseAddress, len, &len))
            }
        }
        d.count = len
        return d
    }

    public enum XattrMode: UInt32 {
        case any = 0
        case create = 1
        case replace = 2
    }

    public func setXattr(_ ino: UInt32, _ name: Data, _ value: Data, mode: XattrMode) throws {
        try withName(name) { p, n in
            try value.withUnsafeBytes { v in
                try ext4Check(
                    ext4_setxattr(
                        handle, ino, p, n, v.bindMemory(to: UInt8.self).baseAddress, value.count, mode.rawValue))
            }
        }
    }

    public func removeXattr(_ ino: UInt32, _ name: Data) throws {
        try withName(name) { p, n in try ext4Check(ext4_removexattr(handle, ino, p, n)) }
    }

    public func listXattrs(_ ino: UInt32) throws -> [Data] {
        var len = 0
        try ext4Check(ext4_listxattr(handle, ino, nil, 0, &len))
        var d = Data(count: len)
        try d.withUnsafeMutableBytes { b in
            try ext4Check(ext4_listxattr(handle, ino, b.bindMemory(to: UInt8.self).baseAddress, len, &len))
        }
        return d.prefix(len).split(separator: 0, omittingEmptySubsequences: true).map { Data($0) }
    }
}

// MARK: - Attribute helpers

extension Ext4Attr {
    public var isDirectory: Bool { file_type == UInt8(EXT4_FT_DIR) }
    public var isRegular: Bool { file_type == UInt8(EXT4_FT_REG) }
    public var isSymlink: Bool { file_type == UInt8(EXT4_FT_LNK) }

    /// A value that changes whenever a directory's contents change.
    public var directoryVersion: UInt64 {
        var h: UInt64 = 1469598103934665603
        for v in [
            UInt64(bitPattern: mtime.sec), UInt64(mtime.nsec), UInt64(bitPattern: ctime.sec), UInt64(ctime.nsec), size,
        ] {
            h = (h ^ v) &* 1099511628211
        }
        return h == 0 ? 1 : h
    }
}

extension Ext4Time {
    public var timespecValue: timespec {
        timespec(tv_sec: Int(sec), tv_nsec: Int(nsec))
    }

    public init(_ ts: timespec) {
        self.init(sec: Int64(ts.tv_sec), nsec: UInt32(clamping: ts.tv_nsec))
    }
}
