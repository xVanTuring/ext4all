import FSKit
import Foundation

/// `BlockIO` over an FSKit block device resource.
final class ResourceBlockIO: BlockIO, @unchecked Sendable {
    private let resource: FSBlockDeviceResource
    let size: UInt64
    let sectorSize: UInt32
    let isReadOnly: Bool

    init(_ resource: FSBlockDeviceResource, readOnly: Bool) {
        self.resource = resource
        self.size = resource.blockSize * resource.blockCount
        self.sectorSize = UInt32(max(resource.blockSize, 512))
        self.isReadOnly = readOnly || !resource.isWritable
    }

    func read(at offset: UInt64, into buffer: UnsafeMutableRawBufferPointer) throws {
        let n = try resource.read(into: buffer, startingAt: off_t(offset), length: buffer.count)
        if n != buffer.count {
            Log.fs.error("short read at \(offset): \(n) of \(buffer.count)")
            throw POSIXError(.EIO)
        }
    }

    func write(at offset: UInt64, from buffer: UnsafeRawBufferPointer) throws {
        if isReadOnly { throw POSIXError(.EROFS) }
        let n = try resource.write(from: buffer, startingAt: off_t(offset), length: buffer.count)
        if n != buffer.count {
            Log.fs.error("short write at \(offset): \(n) of \(buffer.count)")
            throw POSIXError(.EIO)
        }
    }

    func flush() throws {
        if isReadOnly { return }
        // Raw writes go straight to the device driver; this additionally
        // flushes anything FSKit buffered for the resource.
        try resource.metadataFlush()
    }
}
