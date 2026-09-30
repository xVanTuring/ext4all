import Ext4FFI
import FSKit
import Foundation

/// One mounted ext4 volume, implementing FSKit's handler protocols on top
/// of the Rust engine.
final class Ext4Volume: FSVolume, @unchecked Sendable {
    let mount: Ext4Mount
    let items = ItemTable()
    let bsdName: String
    private let capabilities: FSVolume.SupportedCapabilities
    private var blockSize: UInt64

    init(mount: Ext4Mount, info: Ext4VolumeInfo, bsdName: String) {
        self.mount = mount
        self.bsdName = bsdName
        self.blockSize = UInt64(info.blockSize)
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsPersistentObjectIDs = true
        caps.supportsSymbolicLinks = true
        caps.supportsHardLinks = true
        caps.supportsJournal = true
        caps.supportsActiveJournal = !mount.isReadOnly
        caps.supportsSparseFiles = true
        caps.supportsZeroRuns = true
        caps.supportsFastStatFS = true
        caps.supports2TBFiles = true
        caps.supports64BitObjectIDs = true
        caps.supportsHiddenFiles = false
        caps.doesNotSupportRootTimes = false
        caps.doesNotSupportVolumeSizes = false
        caps.doesNotSupportImmutableFiles = false
        caps.doesNotSupportSettingFilePermissions = false
        caps.caseFormat = .sensitive
        self.capabilities = caps
        let label = info.label.isEmpty ? "ext4" : info.label
        super.init(volumeID: FSVolume.Identifier(uuid: info.uuid), volumeName: FSFileName(string: label))
    }

    // MARK: helpers

    /// Run `body` and deliver its result (or error) to `reply`.
    @inline(__always)
    func run<T>(_ what: StaticString, _ reply: (T?, (any Error)?) -> Void, _ body: () throws -> T) {
        do {
            reply(try body(), nil)
        } catch {
            if (error as? POSIXError)?.code != .ENOENT {
                Log.fs.debug("\(what): \(error.localizedDescription, privacy: .public)")
            }
            reply(nil, error)
        }
    }

    func ino(_ item: FSItem) throws -> UInt32 {
        guard let it = item as? Ext4Item else { throw POSIXError(.EINVAL) }
        return it.ino
    }

    func attributes(_ item: FSItem) throws -> FSItem.Attributes {
        guard let it = item as? Ext4Item else { throw POSIXError(.EINVAL) }
        return FSItem.Attributes(try mount.stat(it.ino), parent: it.parentIno)
    }

    func attributes(ino: UInt32, parent: UInt32) throws -> FSItem.Attributes {
        FSItem.Attributes(try mount.stat(ino), parent: parent)
    }

    func freeSpace() -> FSFreeSpace {
        guard let s = try? mount.statfs() else { return .noUpdate }
        let f = FSFreeSpace()
        f.populate(bytes: s.avail_blocks * UInt64(s.block_size))
        return f
    }

    static func unwrap<T>(_ v: T?) throws -> T {
        guard let v else { throw POSIXError(.EIO) }
        return v
    }
}

// MARK: - Volume handler

extension Ext4Volume: FSVolume.Handler {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { capabilities }

    var volumeStatistics: FSStatFSResult {
        let r = FSStatFSResult(fileSystemTypeName: "ext4")
        guard let s = try? mount.statfs() else { return r }
        let bs = UInt64(s.block_size)
        r.blockSize = Int(s.block_size)
        r.ioSize = 1 << 20
        r.totalBlocks = s.blocks
        r.freeBlocks = s.free_blocks
        r.availableBlocks = s.avail_blocks
        r.usedBlocks = s.blocks - s.free_blocks
        r.totalBytes = s.blocks * bs
        r.freeBytes = s.free_blocks * bs
        r.availableBytes = s.avail_blocks * bs
        r.usedBytes = (s.blocks - s.free_blocks) * bs
        r.totalFiles = s.files
        r.freeFiles = s.free_files
        r.fileSystemSubType = 2
        return r
    }

    // PathConf
    var maximumLinkCount: Int { 65000 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { Int(blockSize) - 64 }
    var maximumFileSize: UInt64 { (UInt64(1) << 32) * blockSize - 1 }

    var enableOpenUnlinkEmulation: Bool { false }

    var requestedMountOptions: FSVolume.MountOptions {
        mount.isReadOnly ? [.readOnly] : []
    }

    func activateVolume(
        options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void
    ) {
        run("activate", reply) {
            let root = items.item(for: Ext4Mount.rootIno, parent: Ext4Mount.rootIno)
            _ = try mount.stat(Ext4Mount.rootIno)
            return try Self.unwrap(FSActivateResult(rootItem: root))
        }
    }

    func deactivateVolume(
        options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void
    ) {
        // normally unmount() already committed; this is a safety net
        do {
            try mount.unmount()
        } catch {
            Log.fs.error("deactivate: \(error.localizedDescription, privacy: .public)")
        }
        items.removeAll()
        reply(nil)
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        Log.fs.info("mount \(self.bsdName, privacy: .public)")
        reply(nil)
    }

    func unmount(replyHandler reply: @escaping @Sendable () -> Void) {
        do {
            try mount.unmount()
            Log.fs.info("unmounted \(self.bsdName, privacy: .public) cleanly")
        } catch {
            Log.fs.error("unmount \(self.bsdName, privacy: .public): \(error.localizedDescription, privacy: .public)")
        }
        reply()
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        do {
            try mount.sync()
            reply(nil)
        } catch {
            reply(error)
        }
    }

    func lookupItem(
        named name: FSFileName, in directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void
    ) {
        run("lookup", reply) {
            let dir = try ino(directory)
            let a = try mount.lookup(dir, name.data)
            var parent = dir
            if name.data == Data("..".utf8) {
                parent = (try? mount.lookup(a.ino, Data("..".utf8)).ino) ?? a.ino
            } else if name.data == Data(".".utf8) {
                parent = (directory as? Ext4Item)?.parentIno ?? dir
            }
            let item = items.item(for: a.ino, parent: parent)
            return try Self.unwrap(
                FSLookupItemResult(
                    foundItem: item, itemName: name, itemAttributes: FSItem.Attributes(a, parent: parent)))
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        guard let it = item as? Ext4Item else {
            reply(nil)
            return
        }
        items.remove(it.ino)
        do {
            try mount.reclaim(it.ino)
            reply(nil)
        } catch {
            Log.fs.error("reclaim \(it.ino): \(error.localizedDescription, privacy: .public)")
            reply(error)
        }
    }

    /// Owner for new items: explicit attributes, else the caller.
    private func owner(_ attrs: FSItem.SetAttributesRequest, _ context: FSContext) -> (UInt32, UInt32) {
        let uid = attrs.isValid(.uid) ? attrs.uid : UInt32(clamping: context.effectiveUserID)
        let gid = attrs.isValid(.gid) ? attrs.gid : UInt32(clamping: context.effectiveGroupID)
        return (uid, gid)
    }

    /// Apply attributes other than type/mode/owner requested at creation.
    private func applyCreationExtras(_ ino: UInt32, _ attrs: FSItem.SetAttributesRequest) throws {
        var req = Ext4SetAttr()
        if attrs.isValid(.accessTime) {
            req.valid |= UInt32(EXT4_SET_ATIME)
            req.atime = Ext4Time(attrs.accessTime)
        }
        if attrs.isValid(.modifyTime) {
            req.valid |= UInt32(EXT4_SET_MTIME)
            req.mtime = Ext4Time(attrs.modifyTime)
        }
        if attrs.isValid(.birthTime) {
            req.valid |= UInt32(EXT4_SET_CRTIME)
            req.crtime = Ext4Time(attrs.birthTime)
        }
        if attrs.isValid(.flags) && attrs.flags != 0 {
            req.valid |= UInt32(EXT4_SET_BSD_FLAGS)
            req.bsd_flags = attrs.flags
        }
        if req.valid != 0 {
            _ = try mount.setAttr(ino, req)
        }
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, in directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void
    ) {
        run("create", reply) {
            let dir = try ino(directory)
            guard let t = type.ext4Type, type != .symlink else { throw POSIXError(.EINVAL) }
            let defaultPerm: UInt16 = type == .directory ? 0o755 : 0o644
            let perm = newAttributes.isValid(.mode) ? UInt16(newAttributes.mode & 0o7777) : defaultPerm
            let (uid, gid) = owner(newAttributes, context)
            var a = try mount.create(dir, name.data, type: t, perm: perm, uid: uid, gid: gid)
            try applyCreationExtras(a.ino, newAttributes)
            a = try mount.stat(a.ino)
            newAttributes.consumedAttributes = [.type, .mode, .uid, .gid, .accessTime, .modifyTime, .birthTime, .flags]
            let item = items.item(for: a.ino, parent: dir)
            return try Self.unwrap(
                FSCreateItemResult(
                    newItem: item, newItemName: name, newItemAttributes: FSItem.Attributes(a, parent: dir),
                    directoryAttributes: try attributes(directory), freeSpace: freeSpace()))
        }
    }

    func createSymbolicLink(
        named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void
    ) {
        run("symlink", reply) {
            let dir = try ino(directory)
            let (uid, gid) = owner(newAttributes, context)
            var a = try mount.symlink(dir, name.data, target: contents.data, uid: uid, gid: gid)
            try applyCreationExtras(a.ino, newAttributes)
            a = try mount.stat(a.ino)
            newAttributes.consumedAttributes = [.type, .uid, .gid, .accessTime, .modifyTime, .birthTime, .flags]
            let item = items.item(for: a.ino, parent: dir)
            return try Self.unwrap(
                FSCreateSymlinkResult(
                    newItem: item, newItemName: name, newItemAttributes: FSItem.Attributes(a, parent: dir),
                    directoryAttributes: try attributes(directory), freeSpace: freeSpace()))
        }
    }

    func createLink(
        to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void
    ) {
        run("link", reply) {
            let dir = try ino(directory)
            let target = try ino(item)
            let a = try mount.link(target, to: dir, name: name.data)
            return try Self.unwrap(
                FSCreateLinkResult(
                    linkName: name, linkAttributes: FSItem.Attributes(a, parent: dir),
                    directoryAttributes: try attributes(directory), freeSpace: freeSpace()))
        }
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?,
        context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void
    ) {
        run("rename", reply) {
            let src = try ino(sourceDirectory)
            let dst = try ino(destinationDirectory)
            try mount.rename(src, sourceName.data, dst, destinationName.data)
            (item as? Ext4Item)?.parentIno = dst
            let over: FSItem.Attributes? = overItem.flatMap { try? attributes($0) }
            return try Self.unwrap(
                FSRenameItemResult(
                    newName: destinationName, renamedItemAttributes: try attributes(item),
                    sourceDirectoryAttributes: try attributes(sourceDirectory),
                    destinationDirectoryAttributes: try attributes(destinationDirectory), overItemAttributes: over,
                    freeSpace: freeSpace()))
        }
    }

    func removeItem(
        _ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void
    ) {
        run("remove", reply) {
            let dir = try ino(directory)
            try mount.remove(dir, name.data)
            // the inode stays valid until reclaim
            let itemAttrs = (try? attributes(item)) ?? FSItem.Attributes()
            return try Self.unwrap(
                FSRemoveItemResult(
                    itemAttributes: itemAttrs, directoryAttributes: try attributes(directory), freeSpace: freeSpace()))
        }
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void
    ) {
        run("getattr", reply) {
            try Self.unwrap(FSGetAttributesResult(attributes: try attributes(item)))
        }
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void
    ) {
        run("setattr", reply) {
            let i = try ino(item)
            var req = Ext4SetAttr()
            var consumed: FSItem.Attribute = []
            if newAttributes.isValid(.mode) {
                req.valid |= UInt32(EXT4_SET_MODE)
                req.mode = UInt16(newAttributes.mode & 0o7777)
                consumed.insert(.mode)
            }
            if newAttributes.isValid(.uid) {
                req.valid |= UInt32(EXT4_SET_UID)
                req.uid = newAttributes.uid
                consumed.insert(.uid)
            }
            if newAttributes.isValid(.gid) {
                req.valid |= UInt32(EXT4_SET_GID)
                req.gid = newAttributes.gid
                consumed.insert(.gid)
            }
            if newAttributes.isValid(.size) {
                req.valid |= UInt32(EXT4_SET_SIZE)
                req.size = newAttributes.size
                consumed.insert(.size)
            }
            if newAttributes.isValid(.accessTime) {
                req.valid |= UInt32(EXT4_SET_ATIME)
                req.atime = Ext4Time(newAttributes.accessTime)
                consumed.insert(.accessTime)
            }
            if newAttributes.isValid(.modifyTime) {
                req.valid |= UInt32(EXT4_SET_MTIME)
                req.mtime = Ext4Time(newAttributes.modifyTime)
                consumed.insert(.modifyTime)
            }
            if newAttributes.isValid(.changeTime) {
                req.valid |= UInt32(EXT4_SET_CTIME)
                req.ctime = Ext4Time(newAttributes.changeTime)
                consumed.insert(.changeTime)
            }
            if newAttributes.isValid(.birthTime) {
                req.valid |= UInt32(EXT4_SET_CRTIME)
                req.crtime = Ext4Time(newAttributes.birthTime)
                consumed.insert(.birthTime)
            }
            if newAttributes.isValid(.flags) {
                req.valid |= UInt32(EXT4_SET_BSD_FLAGS)
                req.bsd_flags = newAttributes.flags
                consumed.insert(.flags)
            }
            let a = try mount.setAttr(i, req)
            newAttributes.consumedAttributes = consumed
            let parent = (item as? Ext4Item)?.parentIno ?? i
            return try Self.unwrap(
                FSSetAttributesResult(attributes: FSItem.Attributes(a, parent: parent), freeSpace: freeSpace()))
        }
    }

    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void
    ) {
        run("readdir", reply) {
            let dir = try ino(directory)
            let version = try mount.stat(dir).directoryVersion
            let wantAttrs = attributes != nil
            try mount.readDir(dir, cookie: cookie.rawValue, skipDots: wantAttrs, wantAttrs: wantAttrs) { e in
                let attrs = e.attr.map { FSItem.Attributes($0, parent: dir) }
                return packer.packEntry(
                    name: FSFileName(data: e.name),
                    itemType: FSItem.ItemType(ext4Type: e.fileType),
                    itemID: FSItem.Identifier(rawValue: UInt64(e.ino)) ?? .invalid,
                    nextCookie: FSDirectoryCookie(e.nextCookie),
                    attributes: attrs)
            }
            return try Self.unwrap(FSEnumerateDirectoryResult(verifier: version))
        }
    }

    func readSymbolicLink(
        _ item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void
    ) {
        run("readlink", reply) {
            let target = try mount.readLink(try ino(item))
            return try Self.unwrap(
                FSReadSymlinkResult(contents: FSFileName(data: target), symlinkAttributes: try attributes(item)))
        }
    }
}

// MARK: - File data

extension Ext4Volume: FSVolume.ReadWriteHandler {
    func read(
        from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer,
        replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void
    ) {
        run("read", reply) {
            guard offset >= 0 else { throw POSIXError(.EINVAL) }
            let i = try ino(item)
            let n = try buffer.withUnsafeMutableBytes { raw -> Int in
                let len = min(length, raw.count)
                guard len > 0 else { return 0 }
                return try mount.read(
                    i, offset: UInt64(offset), into: UnsafeMutableRawBufferPointer(rebasing: raw[0..<len]))
            }
            return try Self.unwrap(FSReadFileResult(bytesRead: n, itemAttributes: try attributes(item)))
        }
    }

    func write(
        contents: Data, to item: FSItem, at offset: off_t,
        replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void
    ) {
        run("write", reply) {
            guard offset >= 0 else { throw POSIXError(.EINVAL) }
            let n = try mount.write(try ino(item), offset: UInt64(offset), data: contents)
            return try Self.unwrap(
                FSWriteFileResult(bytesWritten: n, itemAttributes: try attributes(item), freeSpace: freeSpace()))
        }
    }
}

// MARK: - Extended attributes

extension Ext4Volume: FSVolume.XattrHandler {
    func getXattr(
        named name: FSFileName, of item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void
    ) {
        run("getxattr", reply) {
            try Self.unwrap(FSGetXattrResult(xattrValue: try mount.getXattr(try ino(item), name.data)))
        }
    }

    func setXattr(
        named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy,
        context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void
    ) {
        run("setxattr", reply) {
            let i = try ino(item)
            switch policy {
            case .delete:
                try mount.removeXattr(i, name.data)
            case .mustCreate:
                try mount.setXattr(i, name.data, value ?? Data(), mode: .create)
            case .mustReplace:
                try mount.setXattr(i, name.data, value ?? Data(), mode: .replace)
            default:
                if let value {
                    try mount.setXattr(i, name.data, value, mode: .any)
                } else {
                    try mount.removeXattr(i, name.data)
                }
            }
            return try Self.unwrap(FSSetXattrResult(freeSpace: freeSpace()))
        }
    }

    func listXattrs(
        of item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void
    ) {
        run("listxattr", reply) {
            let names = try mount.listXattrs(try ino(item)).map { FSFileName(data: $0) }
            return try Self.unwrap(FSListXattrsResult(xattrNames: names))
        }
    }
}

// MARK: - Volume rename, preallocation, sparse seeking

extension Ext4Volume: FSVolume.RenameHandler {
    func setVolumeName(
        _ name: FSFileName, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSVolumeRenameResult?, (any Error)?) -> Void
    ) {
        run("setVolumeName", reply) {
            guard let s = name.string else { throw POSIXError(.EINVAL) }
            try mount.setLabel(s)
            self.name = name
            return try Self.unwrap(FSVolumeRenameResult(newName: name))
        }
    }
}

extension Ext4Volume: FSVolume.PreallocateHandler {
    func preallocateSpace(
        for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void
    ) {
        run("preallocate", reply) {
            let i = try ino(item)
            var start = UInt64(max(offset, 0))
            if flags.contains(.fromEOF) {
                start += try mount.stat(i).size
            }
            try mount.fallocate(i, offset: start, length: UInt64(length), keepSize: true)
            return try Self.unwrap(
                FSPreallocateResult(
                    bytesAllocated: length, itemAttributes: try attributes(item), freeSpace: freeSpace()))
        }
    }
}

extension Ext4Volume: FSVolume.SeekRegionHandler {
    func seek(
        within item: FSItem, from offset: off_t, region: FSVolume.SeekRegion, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSSeekRegionResult?, (any Error)?) -> Void
    ) {
        run("seek", reply) {
            guard offset >= 0 else { throw POSIXError(.EINVAL) }
            let r = try mount.seek(try ino(item), from: UInt64(offset), data: region == .data)
            return FSSeekRegionResult(returnedOffset: off_t(r))
        }
    }
}
