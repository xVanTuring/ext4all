import Ext4FFI
import FSKit
import Foundation

/// An inode as seen by FSKit. Exactly one `Ext4Item` exists per inode while
/// the kernel holds a reference to it (see `ItemTable`).
final class Ext4Item: FSItem {
    let ino: UInt32
    /// Directory the item was last reached through (for `parentID`).
    var parentIno: UInt32

    init(ino: UInt32, parent: UInt32) {
        self.ino = ino
        self.parentIno = parent
        super.init()
    }

    override var description: String {
        "Ext4Item(\(ino))"
    }
}

/// Maps inode numbers to live `Ext4Item` objects so FSKit always sees the
/// same object for the same file.
final class ItemTable: @unchecked Sendable {
    private var items: [UInt32: Ext4Item] = [:]
    private let lock = NSLock()

    func item(for ino: UInt32, parent: UInt32) -> Ext4Item {
        lock.lock()
        defer { lock.unlock() }
        if let it = items[ino] {
            if ino != Ext4Mount.rootIno {
                it.parentIno = parent
            }
            return it
        }
        let it = Ext4Item(ino: ino, parent: parent)
        items[ino] = it
        return it
    }

    func remove(_ ino: UInt32) {
        lock.lock()
        items.removeValue(forKey: ino)
        lock.unlock()
    }

    /// Remove `item` only if it is still the live object for its inode (a
    /// newer lookup may already have replaced it).
    func remove(_ item: Ext4Item) {
        lock.lock()
        if items[item.ino] === item {
            items.removeValue(forKey: item.ino)
        }
        lock.unlock()
    }

    func removeAll() {
        lock.lock()
        items.removeAll()
        lock.unlock()
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return items.count
    }
}

extension FSItem.ItemType {
    init(ext4Type t: UInt8) {
        switch Int32(t) {
        case EXT4_FT_REG: self = .file
        case EXT4_FT_DIR: self = .directory
        case EXT4_FT_LNK: self = .symlink
        case EXT4_FT_FIFO: self = .fifo
        case EXT4_FT_CHR: self = .charDevice
        case EXT4_FT_BLK: self = .blockDevice
        case EXT4_FT_SOCK: self = .socket
        default: self = .unknown
        }
    }

    var ext4Type: UInt8? {
        switch self {
        case .file: return UInt8(EXT4_FT_REG)
        case .directory: return UInt8(EXT4_FT_DIR)
        case .symlink: return UInt8(EXT4_FT_LNK)
        case .fifo: return UInt8(EXT4_FT_FIFO)
        case .charDevice: return UInt8(EXT4_FT_CHR)
        case .blockDevice: return UInt8(EXT4_FT_BLK)
        case .socket: return UInt8(EXT4_FT_SOCK)
        default: return nil
        }
    }
}

extension FSItem.Attributes {
    /// Populate every attribute from an ext4 inode.
    convenience init(_ a: Ext4Attr, parent: UInt32) {
        self.init()
        type = FSItem.ItemType(ext4Type: a.file_type)
        mode = UInt32(a.mode)
        linkCount = a.nlink
        uid = a.uid
        gid = a.gid
        flags = a.bsd_flags
        size = a.size
        allocSize = a.allocated
        fileID = FSItem.Identifier(rawValue: UInt64(a.ino)) ?? .invalid
        let p = a.ino == Ext4Mount.rootIno ? FSItem.Identifier.parentOfRoot.rawValue : UInt64(parent)
        parentID = FSItem.Identifier(rawValue: p) ?? .invalid
        accessTime = a.atime.timespecValue
        modifyTime = a.mtime.timespecValue
        changeTime = a.ctime.timespecValue
        birthTime = a.has_crtime ? a.crtime.timespecValue : a.ctime.timespecValue
        supportsLimitedXAttrs = false
        inhibitKernelOffloadedIO = true
    }
}
