import FSKit
import Foundation

/// `BlockIO` over an FSKit block device resource.
///
/// Uses the resource's raw `read`/`write`, which go straight to the device
/// driver and complete synchronously, so writes reach the device in the
/// order the engine issues them. FSKit offers no way to make the drive
/// flush its own write cache; `metadataFlush` only covers the kernel buffer
/// cache used by `metadataWrite`/`delayedMetadataWrite`, which this module
/// does not use.
final class ResourceBlockIO: BlockIO, @unchecked Sendable {
    private let resource: FSBlockDeviceResource
    let size: UInt64
    let sectorSize: UInt32
    let isReadOnly: Bool
    private let flushState = Locked<FlushState>(.untried)

    private enum FlushState {
        case untried, works, unsupported
    }

    init(_ resource: FSBlockDeviceResource, readOnly: Bool) {
        self.resource = resource
        self.size = resource.blockSize * resource.blockCount
        self.sectorSize = Self.alignment(logical: resource.blockSize, physical: resource.physicalBlockSize)
        self.isReadOnly = readOnly || !resource.isWritable
        Log.fs.debug(
            "\(resource.bsdName, privacy: .public): block size \(resource.blockSize), physical \(resource.physicalBlockSize), transfers aligned to \(self.sectorSize)"
        )
    }

    /// Transfer alignment: the physical sector size when it is a sane power
    /// of two (writes must be physical-sector aligned), never less than the
    /// logical block size or 512.
    static func alignment(logical: UInt64, physical: UInt64) -> UInt32 {
        var a = max(logical, 512)
        if physical > a && physical <= 65536 && physical & (physical - 1) == 0 && physical % a == 0 {
            a = physical
        }
        return UInt32(min(a, 65536))
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
        if Self.verifyWrites {
            var back = [UInt8](repeating: 0, count: buffer.count)
            try back.withUnsafeMutableBytes { try read(at: offset, into: $0) }
            if !back.elementsEqual(buffer) {
                let first = zip(back, buffer).enumerated().first { $0.element.0 != $0.element.1 }?.offset ?? -1
                Log.fs.error("read after write at \(offset)+\(buffer.count) differs from byte \(first)")
            }
        }
    }

    /// Diagnostic for drives that return stale data: read every write back
    /// at once and log differences (defaults key `VerifyWrites`). Slow.
    static let verifyWrites = UserDefaults.standard.bool(forKey: "VerifyWrites")

    func flush() throws {
        if isReadOnly { return }
        // Raw writes have already completed at the driver. Ask FSKit to
        // flush as well in case it helps, but a resource without a buffer
        // cache (it reports kIOReturnNoDevice) is not an error.
        let state = flushState.withLock { $0 }
        if state == .unsupported { return }
        do {
            try resource.metadataFlush()
            if state == .untried { flushState.withLock { $0 = .works } }
        } catch {
            if state == .untried {
                Log.fs.info(
                    "metadata flush unavailable (\(error.localizedDescription, privacy: .public)); relying on synchronous raw writes"
                )
                flushState.withLock { $0 = .unsupported }
            } else {
                // it worked before: a real failure now
                throw error
            }
        }
    }
}
