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
        // transfers may be partial: continue until done
        var done = 0
        while done < buffer.count {
            let rest = UnsafeMutableRawBufferPointer(rebasing: buffer[done...])
            let n = try resource.read(into: rest, startingAt: off_t(offset) + off_t(done), length: rest.count)
            if n <= 0 {
                Log.fs.error("read at \(offset + UInt64(done)) returned \(n) of \(rest.count)")
                throw POSIXError(.EIO)
            }
            done += n
        }
    }

    func write(at offset: UInt64, from buffer: UnsafeRawBufferPointer) throws {
        if isReadOnly { throw POSIXError(.EROFS) }
        var done = 0
        while done < buffer.count {
            let rest = UnsafeRawBufferPointer(rebasing: buffer[done...])
            let n = try resource.write(from: rest, startingAt: off_t(offset) + off_t(done), length: rest.count)
            if n <= 0 {
                Log.fs.error("write at \(offset + UInt64(done)) returned \(n) of \(rest.count)")
                throw POSIXError(.EIO)
            }
            done += n
        }
    }

    func flush() throws {
        if isReadOnly { return }
        // Raw writes go straight to the device driver; this additionally
        // flushes anything FSKit buffered for the resource.
        try resource.metadataFlush()
    }
}
