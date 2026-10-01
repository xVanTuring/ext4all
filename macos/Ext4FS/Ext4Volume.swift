import Ext4FFI
import FSKit
import Foundation

/// One mounted ext4 volume, implementing FSKit's handler protocols on top
/// of the Rust engine.
class Ext4Volume: FSVolume, @unchecked Sendable {
    let mount: Ext4Mount
    let items = ItemTable()
    let bsdName: String
    let info: Ext4VolumeInfo
    private let capabilities: FSVolume.SupportedCapabilities
    private var blockSize: UInt64
    /// Serializes handlers so that attributes and free space are sampled in
    /// the same critical section as the operation that produced them. Never
    /// held while replying: FSKit may call other handlers synchronously
    /// from inside a reply (activation fetches the root's attributes).
    let opLock = NSLock()
    /// Held from handing out an item until the reply has been sent, and by
    /// reclaim around `FSItem.tryReclaim`, so FSKit's count of returned
    /// items is accurate when reclaim checks it. Recursive, because a reply
    /// may re-enter the volume on the same thread. Lock order: `itemLock`
    /// before `opLock`.
    let itemLock = NSRecursiveLock()
    /// Set by `unmount`; a later `mount` makes the volume writable again.
    private var finished = false
    /// Events already logged once (which I/O paths FSKit actually uses).
    private var noted = Set<String>()

    /// Log `message` the first time `event` happens on this volume; call
    /// with `opLock` held.
    func noteOnce(_ event: String, _ message: @autoclosure () -> String) {
        if noted.insert(event).inserted {
            let text = message()
            Log.fs.info("\(self.bsdName, privacy: .public): \(text, privacy: .public)")
        }
    }
    /// Regular extent-mapped files use kernel offloaded I/O
    /// (`Ext4KernelIOVolume`); all others go through read/write. Can only
    /// be switched off, at activation (`-o nokoio`), before any item is
    /// handed out.
    private(set) var kernelIO: Bool

    init(mount: Ext4Mount, info: Ext4VolumeInfo, bsdName: String, kernelIO: Bool = false) {
        self.mount = mount
        self.bsdName = bsdName
        self.info = info
        self.kernelIO = kernelIO
        self.blockSize = UInt64(info.blockSize)
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsPersistentObjectIDs = true
        caps.supportsSymbolicLinks = true
        caps.supportsHardLinks = true
        caps.supportsJournal = info.hasJournal
        caps.supportsActiveJournal = info.hasJournal && !mount.isReadOnly
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

    /// Run `body` under the volume lock, release it, and deliver the result
    /// (or error) to `reply`. Handlers that hand an item to FSKit (lookup,
    /// create, activate) pass `handsOutItem`: FSKit counts an item as
    /// returned when the reply is sent, so `itemLock` stays held until
    /// then and reclaim cannot run in between (`FSItem.tryReclaim`).
    @inline(__always)
    func run<T>(
        _ what: StaticString, handsOutItem: Bool = false, _ reply: (T?, (any Error)?) -> Void,
        _ body: () throws -> T
    ) {
        if handsOutItem {
            itemLock.lock()
        }
        defer {
            if handsOutItem {
                itemLock.unlock()
            }
        }
        let result: Result<T, any Error>
        opLock.lock()
        do {
            result = .success(try body())
        } catch {
            result = .failure(error)
        }
        opLock.unlock()
        switch result {
        case .success(let v):
            reply(v, nil)
        case .failure(let error):
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
        return attrs(try mount.stat(it.ino), parent: it.parentIno)
    }

    func attributes(ino: UInt32, parent: UInt32) throws -> FSItem.Attributes {
        attrs(try mount.stat(ino), parent: parent)
    }

    /// FSKit attributes for an inode, honouring the volume's I/O mode. The
    /// I/O path of a live item is decided once (an inline data file that
    /// grows into extents keeps using read/write until it is reclaimed).
    func attrs(_ a: Ext4Attr, parent: UInt32) -> FSItem.Attributes {
        guard kernelIO else { return FSItem.Attributes(a, parent: parent) }
        var useKernelIO = a.supportsKernelIO
        if let item = items.existing(a.ino) {
            if let decided = item.kernelIO {
                useKernelIO = decided
            } else {
                item.kernelIO = useKernelIO
            }
        }
        return FSItem.Attributes(a, parent: parent, kernelIO: useKernelIO)
    }

    /// Current free space; call with `opLock` held.
    func freeSpace() -> FSFreeSpace {
        guard let s = try? mount.statfs() else { return .noUpdate }
        let f = FSFreeSpace()
        let (bytes, overflow) = s.avail_blocks.multipliedReportingOverflow(by: UInt64(s.block_size))
        f.populate(bytes: overflow ? UInt64.max : bytes)
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
        let typeName = ["ext2", "ext3", "ext4"][min(max(info.subtype, 0), 2)]
        let r = FSStatFSResult(fileSystemTypeName: typeName)
        opLock.lock()
        let stat = try? mount.statfs()
        opLock.unlock()
        guard let s = stat else { return r }
        // saturating arithmetic: the values come from disk
        func bytes(_ blocks: UInt64) -> UInt64 {
            let (v, o) = blocks.multipliedReportingOverflow(by: UInt64(s.block_size))
            return o ? UInt64.max : v
        }
        let free = min(s.free_blocks, s.blocks)
        let avail = min(s.avail_blocks, free)
        r.blockSize = Int(s.block_size)
        r.ioSize = 1 << 20
        r.totalBlocks = s.blocks
        r.freeBlocks = free
        r.availableBlocks = avail
        r.usedBlocks = s.blocks - free
        r.totalBytes = bytes(s.blocks)
        r.freeBytes = bytes(free)
        r.availableBytes = bytes(avail)
        r.usedBytes = bytes(s.blocks - free)
        r.totalFiles = s.files
        r.freeFiles = min(s.free_files, s.files)
        r.fileSystemSubType = info.subtype
        return r
    }

    // PathConf
    var maximumLinkCount: Int { 65000 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { Int(blockSize) - 64 }
    /// Logical blocks 0 ..< 2^32-1 with extents.
    var maximumFileSize: UInt64 { ((UInt64(1) << 32) - 1) * blockSize }

    var enableOpenUnlinkEmulation: Bool { false }

    var requestedMountOptions: FSVolume.MountOptions {
        mount.isReadOnly ? [.readOnly] : []
    }

    func activateVolume(
        options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void
    ) {
        run("activate", handsOutItem: true, reply) {
            if kernelIO {
                kernelIO = Ext4FileSystem.wantsKernelIO(options.taskOptions, defaultOn: true)
                Log.fs.info("kernel offloaded I/O \(self.kernelIO ? "on" : "off (-o nokoio)", privacy: .public)")
            }
            let root = items.item(for: Ext4Mount.rootIno, parent: Ext4Mount.rootIno)
            _ = try mount.stat(Ext4Mount.rootIno)
            return try Self.unwrap(FSActivateResult(rootItem: root))
        }
    }

    func deactivateVolume(
        options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void
    ) {
        // unmount() already committed and marked the volume clean; release
        // the engine and the device now (a safety net if unmount was skipped)
        opLock.lock()
        do {
            try mount.unmount()
        } catch {
            Log.fs.error("deactivate: \(error.localizedDescription, privacy: .public)")
        }
        items.removeAll()
        opLock.unlock()
        reply(nil)
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        var failure: (any Error)?
        opLock.lock()
        if finished {
            // mounted again after an unmount without re-activation; if the
            // engine cannot become writable again (e.g. after a failed
            // commit) the mount fails rather than pretending to be writable
            do {
                try mount.remount()
                finished = false
            } catch {
                Log.fs.error("remount: \(error.localizedDescription, privacy: .public)")
                failure = error
            }
        }
        opLock.unlock()
        if failure == nil {
            Log.fs.info("mount \(self.bsdName, privacy: .public)")
        }
        reply(failure)
    }

    func unmount(replyHandler reply: @escaping @Sendable () -> Void) {
        // FSKit reclaims all items after unmount, so keep the engine open
        // (read-only) until deactivation.
        opLock.lock()
        do {
            try mount.finish()
            Log.fs.info("unmounted \(self.bsdName, privacy: .public) cleanly")
        } catch {
            Log.fs.error("unmount \(self.bsdName, privacy: .public): \(error.localizedDescription, privacy: .public)")
        }
        finished = true
        opLock.unlock()
        reply()
    }

    /// Applications' fsync arrives here (observed as wait | 0x10000), and
    /// macOS `cp` issues a waiting and a non-waiting sync for every copied
    /// file; closing a file does not sync. So a sync must be cheap: a
    /// waiting one commits to the journal (durable; the blocks reach their
    /// home locations at a later checkpoint), a non-waiting one only asks
    /// the commit thread to commit soon.
    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        let wait = flags.rawValue & (FSSyncFlags.wait.rawValue | FSSyncFlags.dWait.rawValue) != 0
        guard wait else {
            mount.requestCommit()
            reply(nil)
            return
        }
        do {
            try mount.commit()
            reply(nil)
        } catch {
            reply(error)
        }
    }

    func lookupItem(
        named name: FSFileName, in directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void
    ) {
        run("lookup", handsOutItem: true, reply) {
            let (item, attributes) = try lookupParts(named: name, in: directory)
            return try Self.unwrap(FSLookupItemResult(foundItem: item, itemName: name, itemAttributes: attributes))
        }
    }

    /// Lookup shared by the plain and kernel offloaded I/O handlers; call
    /// with `opLock` held.
    func lookupParts(named name: FSFileName, in directory: FSItem) throws -> (Ext4Item, FSItem.Attributes) {
        let dir = try ino(directory)
        let a = try mount.lookup(dir, name.data)
        var parent = dir
        if name.data == Data("..".utf8) {
            parent = (try? mount.lookup(a.ino, Data("..".utf8)).ino) ?? a.ino
        } else if name.data == Data(".".utf8) {
            parent = (directory as? Ext4Item)?.parentIno ?? dir
        }
        return (items.item(for: a.ino, parent: parent), attrs(a, parent: parent))
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        guard let it = item as? Ext4Item else {
            reply(nil)
            return
        }
        var failure: (any Error)?
        // While itemLock is held no lookup or create can be between handing
        // this item out and replying, so FSKit's count is final.
        itemLock.lock()
        opLock.lock()
        let reclaimed = it.tryReclaim { [self] in
            items.remove(it)
            do {
                try mount.reclaim(it.ino)
            } catch {
                failure = error
            }
        }
        opLock.unlock()
        itemLock.unlock()
        if reclaimed, let failure {
            Log.fs.error("reclaim \(it.ino): \(failure.localizedDescription, privacy: .public)")
            reply(failure)
        } else {
            reply(nil)
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
        run("create", handsOutItem: true, reply) {
            let (item, attributes) = try createParts(named: name, type: type, in: directory, newAttributes, context)
            return try Self.unwrap(
                FSCreateItemResult(
                    newItem: item, newItemName: name, newItemAttributes: attributes,
                    directoryAttributes: try self.attributes(directory), freeSpace: freeSpace()))
        }
    }

    /// Creation shared by the plain and kernel offloaded I/O handlers; call
    /// with `opLock` held.
    func createParts(
        named name: FSFileName, type: FSItem.ItemType, in directory: FSItem,
        _ newAttributes: FSItem.SetAttributesRequest, _ context: FSContext
    ) throws -> (Ext4Item, FSItem.Attributes) {
        let dir = try ino(directory)
        guard let t = type.ext4Type, type != .symlink else { throw POSIXError(.EINVAL) }
        let defaultPerm: UInt16 = type == .directory ? 0o755 : 0o644
        let perm = newAttributes.isValid(.mode) ? UInt16(newAttributes.mode & 0o7777) : defaultPerm
        let (uid, gid) = owner(newAttributes, context)
        var a = try mount.create(dir, name.data, type: t, perm: perm, uid: uid, gid: gid)
        try applyCreationExtras(a.ino, newAttributes)
        a = try mount.stat(a.ino)
        newAttributes.consumedAttributes = [.type, .mode, .uid, .gid, .accessTime, .modifyTime, .birthTime, .flags]
        return (items.item(for: a.ino, parent: dir), attrs(a, parent: dir))
    }

    func createSymbolicLink(
        named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void
    ) {
        run("symlink", handsOutItem: true, reply) {
            let dir = try ino(directory)
            let (uid, gid) = owner(newAttributes, context)
            var a = try mount.symlink(dir, name.data, target: contents.data, uid: uid, gid: gid)
            try applyCreationExtras(a.ino, newAttributes)
            a = try mount.stat(a.ino)
            newAttributes.consumedAttributes = [.type, .uid, .gid, .accessTime, .modifyTime, .birthTime, .flags]
            let item = items.item(for: a.ino, parent: dir)
            return try Self.unwrap(
                FSCreateSymlinkResult(
                    newItem: item, newItemName: name, newItemAttributes: attrs(a, parent: dir),
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
                    linkName: name, linkAttributes: attrs(a, parent: dir),
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
                FSSetAttributesResult(attributes: attrs(a, parent: parent), freeSpace: freeSpace()))
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
            do {
                try mount.readDir(dir, cookie: cookie.rawValue, skipDots: wantAttrs, wantAttrs: wantAttrs) { e in
                    let entryAttrs = e.attr.map { self.attrs($0, parent: dir) }
                    return packer.packEntry(
                        name: FSFileName(data: e.name),
                        itemType: FSItem.ItemType(ext4Type: e.fileType),
                        itemID: FSItem.Identifier(rawValue: UInt64(e.ino)) ?? .invalid,
                        nextCookie: FSDirectoryCookie(e.nextCookie),
                        attributes: entryAttrs)
                }
            } catch let e as POSIXError where e.code == .ESTALE {
                // the directory changed format (became an htree) since the
                // cookie was handed out: the enumeration must restart
                throw FSError(.invalidDirectoryCookie)
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
            noteOnce("read", "first read through the extension (inode \(i))")
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
            let i = try ino(item)
            noteOnce("write", "first write through the extension (inode \(i))")
            let n = try mount.write(i, offset: UInt64(offset), data: contents)
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
    /// Block-mapped (ext2/ext3) volumes cannot express preallocated blocks.
    var isPreallocateInhibited: Bool { info.subtype < 2 }

    func preallocateSpace(
        for item: FSItem, at offset: off_t, length: Int, flags: FSVolume.PreallocateFlags, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSPreallocateResult?, (any Error)?) -> Void
    ) {
        run("preallocate", reply) {
            let (allocated, attributes) = try preallocateParts(item, length: length)
            return try Self.unwrap(
                FSPreallocateResult(
                    bytesAllocated: allocated, itemAttributes: attributes,
                    freeSpace: allocated > 0 ? freeSpace() : .noUpdate))
        }
    }

    /// Preallocation shared by the plain and kernel offloaded I/O
    /// handlers; call with `opLock` held. Returns the bytes allocated.
    func preallocateParts(_ item: FSItem, length: Int) throws -> (Int, FSItem.Attributes) {
        let i = try ino(item)
        guard length > 0 else { return (0, try attributes(item)) }
        // F_PREALLOCATE semantics: FSKit always sets `.fromEOF` and the
        // offset is ignored; space is added after the physical end of the
        // file (its allocated size), without changing its size.
        let before = try mount.stat(i)
        let logicalEnd = (before.size + blockSize - 1) / blockSize * blockSize
        let start = max(try mount.allocatedEnd(i), logicalEnd)
        try mount.fallocate(i, offset: start, length: UInt64(length), keepSize: true)
        let after = try mount.stat(i)
        let allocated = Int(clamping: after.allocated &- before.allocated)
        return (allocated, attrs(after, parent: parentOf(item)))
    }
}

extension Ext4Volume {
    func parentOf(_ item: FSItem) -> UInt32 {
        (item as? Ext4Item)?.parentIno ?? Ext4Mount.rootIno
    }

    /// Consistency check run before mounting: the journal has already been
    /// replayed and orphans processed by the engine at load; verify the
    /// volume is readable and summarize it.
    func quickCheck() throws -> [String] {
        opLock.lock()
        defer { opLock.unlock() }
        let v = try mount.volumeInfo()
        guard v.support != .unsupported else { throw POSIXError(.ENOTSUP) }
        let s = try mount.statfs()
        let root = try mount.stat(Ext4Mount.rootIno)
        guard root.isDirectory else { throw POSIXError(.EIO) }
        var entries = 0
        try mount.readDir(Ext4Mount.rootIno, cookie: 0, skipDots: true, wantAttrs: false) { _ in
            entries += 1
            return entries < 100_000
        }
        let kind = ["ext2", "ext3", "ext4"][min(max(v.subtype, 0), 2)]
        return [
            "\(kind) volume \"\(v.label)\" \(v.uuid.uuidString)",
            "\(s.blocks) blocks of \(s.block_size) bytes, \(s.free_blocks) free; \(s.files) inodes, \(s.free_files) free",
            "root directory readable (\(entries) entries)",
            mount.isReadOnly ? "mounting read-only" : "mounting read-write",
        ]
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
