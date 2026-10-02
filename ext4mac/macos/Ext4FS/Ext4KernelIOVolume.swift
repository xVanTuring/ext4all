import Ext4FFI
import FSKit
import Foundation

/// A volume whose regular extent-mapped files use kernel offloaded I/O:
/// FSKit asks for block mappings and the kernel transfers file data
/// directly to and from the device. Directories, symbolic links, inline
/// data and block-mapped (ext2/ext3) files keep using the read/write
/// handler. Opt-in: see `Ext4FileSystem.wantsKernelIO`.
///
/// Writes: `blockmapFile` with `.write` allocates missing blocks as
/// unwritten extents (they read as zeros until the write completes, also
/// after a crash); partly covered new blocks are zeroed first so no stale
/// device contents become visible. `completeIO` converts the range to
/// written and grows the file.
final class Ext4KernelIOVolume: Ext4Volume, FSVolume.KernelOffloadedIOHandler, @unchecked Sendable {
    let resource: FSBlockDeviceResource

    init(
        mount: Ext4Mount, info: Ext4VolumeInfo, resource: FSBlockDeviceResource, parallelReads: Bool = false,
        parallelWrites: Bool = false
    ) {
        self.resource = resource
        super.init(
            mount: mount, info: info, bsdName: resource.bsdName, kernelIO: true, parallelReads: parallelReads,
            parallelWrites: parallelWrites)
    }

    /// Largest extent length the packer accepts (`UINT32_MAX`), rounded
    /// down to whole blocks.
    private var maxExtentLength: UInt64 {
        let bs = UInt64(info.blockSize)
        return UInt64(UInt32.max) / bs * bs
    }

    /// Pack one engine extent, splitting it at the packer's length limit.
    /// Returns false once the packer is full.
    static func pack(
        _ e: Ext4IOExtent, maxLength: UInt64, _ packOne: (FSExtentType, UInt64, UInt64, UInt64) -> Bool
    ) -> Bool {
        var logical = e.logical
        var physical = e.physical
        var rest = e.length
        while rest > 0 {
            let n = min(rest, maxLength)
            let type: FSExtentType = e.zeroFill ? .zeroFill : .data
            if !packOne(type, logical, e.zeroFill ? 0 : physical, n) {
                return false
            }
            logical += n
            physical += n
            rest -= n
        }
        return true
    }

    func blockmapFile(
        _ file: FSItem, offset: off_t, length: Int, flags: FSBlockmapFlags, operationID: FSOperationID,
        packer: FSExtentPacker, replyHandler reply: @escaping @Sendable (FSBlockmapResult?, (any Error)?) -> Void
    ) {
        run("blockmap", reply) {
            guard offset >= 0, length >= 0 else { throw POSIXError(.EINVAL) }
            let i = try ino(file)
            let write = flags.contains(.write)
            noteOnce(
                write ? "blockmap-w" : "blockmap-r", "first kernel \(write ? "write" : "read") mapping (inode \(i))")
            Log.fs.debug("blockmap \(write ? "write" : "read", privacy: .public) \(i) \(offset)+\(length)")
            stats.add(write ? "blockmap write" : "blockmap read", bytes: length)
            stats.add("blockmap \(write ? "write" : "read") \(IOStats.bucket(length))")
            let resource = self.resource
            let limit = maxExtentLength
            try mount.mapForIO(i, offset: UInt64(offset), length: UInt64(length), write: write) { e in
                Self.pack(e, maxLength: limit) { type, logical, physical, n in
                    packer.packExtent(
                        resource: resource, type: type, logicalOffset: off_t(logical), physicalOffset: off_t(physical),
                        length: Int(n))
                }
            }
            return try Self.unwrap(FSBlockmapResult(freeSpace: write ? freeSpace() : .noUpdate))
        }
    }

    /// Whether a completion status reports success. The SDK declares the
    /// status as non-optional but documents `nil` for success; an error
    /// with code 0 is treated as success as well.
    static func succeeded(_ status: (any Error)?) -> Bool {
        guard let status else { return true }
        return (status as NSError).code == 0
    }

    // `status` is implicitly unwrapped on purpose and never force
    // unwrapped: FSKit passes nil on success although the Objective-C
    // header does not mark it nullable.
    func completeIO(
        for file: FSItem, offset: off_t, length: Int, status: (any Error)!, flags: FSCompleteIOFlags,
        operationID: FSOperationID, replyHandler reply: @escaping @Sendable (FSCompleteIOResult?, (any Error)?) -> Void
    ) {
        run("completeIO", reply) {
            let i = try ino(file)
            let ok = Self.succeeded(status)
            noteOnce("complete", "first kernel I/O completion (inode \(i))")
            let kind = flags.contains(.write) ? "write" : "read"
            stats.add(ok ? "complete \(kind)" : "complete \(kind) failed", bytes: length)
            Log.fs.debug(
                "complete \(kind, privacy: .public) \(i) \(offset)+\(length) \(ok ? "ok" : "failed", privacy: .public)")
            if flags.contains(.write) && offset >= 0 && length > 0 {
                if ok {
                    try mount.completeWrite(i, offset: UInt64(offset), length: UInt64(length))
                } else {
                    // the blocks stay unwritten (read as zeros)
                    try mount.abortWrite(i, offset: UInt64(offset), length: UInt64(length))
                }
            }
            if !ok {
                Log.fs.error(
                    "kernel I/O on \(i) at \(offset)+\(length) failed: \(status?.localizedDescription ?? "", privacy: .public)"
                )
            }
            return try Self.unwrap(FSCompleteIOResult(itemAttributes: try attributes(file)))
        }
    }

    // Lookup and creation do not supply extents up front (optional; the
    // kernel asks with `blockmapFile` when it needs them).

    func lookupItem(
        named name: FSFileName, in directory: FSItem, packer: FSExtentPacker, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSLookupItemKOIOResult?, (any Error)?) -> Void
    ) {
        run("lookup", handsOutItem: true, reply) {
            let (item, attributes) = try lookupParts(named: name, in: directory)
            return try Self.unwrap(FSLookupItemKOIOResult(foundItem: item, itemName: name, itemAttributes: attributes))
        }
    }

    func createFile(
        named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
        packer: FSExtentPacker, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateFileKOIOResult?, (any Error)?) -> Void
    ) {
        run("create", handsOutItem: true, reply) {
            let (item, attributes) = try createParts(
                named: name, type: .file, in: directory, newAttributes, context)
            return try Self.unwrap(
                FSCreateFileKOIOResult(
                    newItem: item, newItemName: name, newItemAttributes: attributes,
                    directoryAttributes: try self.attributes(directory), freeSpace: freeSpace()))
        }
    }

    func preallocateSpace(
        for file: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, packer: FSExtentPacker,
        context: FSContext, replyHandler reply: @escaping @Sendable (FSPreallocateKOIOResult?, (any Error)?) -> Void
    ) {
        run("preallocate", reply) {
            let (allocated, attributes) = try preallocateParts(file, length: length)
            return try Self.unwrap(
                FSPreallocateKOIOResult(
                    bytesAllocated: allocated, itemAttributes: attributes,
                    freeSpace: allocated > 0 ? freeSpace() : .noUpdate))
        }
    }
}
