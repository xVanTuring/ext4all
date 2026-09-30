import Foundation

/// A random-access block device used by the ext4 engine.
///
/// Implementations must be thread safe: the engine may call them from any
/// thread (always serialized per volume, but not pinned to one thread).
public protocol BlockIO: AnyObject {
    /// Device size in bytes.
    var size: UInt64 { get }
    /// Required alignment of offsets and lengths.
    var sectorSize: UInt32 { get }
    var isReadOnly: Bool { get }
    func read(at offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws
    func write(at offset: UInt64, from buffer: UnsafeRawBufferPointer) throws
    func flush() throws
}

/// `BlockIO` over a regular file (disk images; used by tests and tools).
public final class FileBlockIO: BlockIO {
    private let fd: Int32
    public let size: UInt64
    public let sectorSize: UInt32
    public let isReadOnly: Bool

    public init(path: String, readOnly: Bool, sectorSize: UInt32 = 512) throws {
        let fd = open(path, readOnly ? O_RDONLY : O_RDWR)
        guard fd >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        var st = stat()
        guard fstat(fd, &st) == 0 else {
            close(fd)
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        self.fd = fd
        self.size = UInt64(st.st_size)
        self.sectorSize = sectorSize
        self.isReadOnly = readOnly
    }

    deinit {
        close(fd)
    }

    public func read(at offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws {
        var done = 0
        while done < buffer.count {
            let n = pread(fd, buffer.baseAddress! + done, buffer.count - done, off_t(offset) + off_t(done))
            if n < 0 { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
            if n == 0 { throw POSIXError(.EIO) }
            done += n
        }
    }

    public func write(at offset: UInt64, from buffer: UnsafeRawBufferPointer) throws {
        if isReadOnly { throw POSIXError(.EROFS) }
        var done = 0
        while done < buffer.count {
            let n = pwrite(fd, buffer.baseAddress! + done, buffer.count - done, off_t(offset) + off_t(done))
            if n < 0 { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
            done += n
        }
    }

    public func flush() throws {
        if !isReadOnly && fsync(fd) != 0 {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
    }
}
