import FSKit
import Foundation

/// How the kernel may cache file data, chosen per volume at load from the
/// `DataCache` defaults key (see `Ext4FileSystem.dataCacheDefaultsKey`).
enum DataCachePolicy: String, CaseIterable {
    /// Don't negotiate (`isDataCacheInhibited`): the kernel caches as it
    /// sees fit, as for a volume without `FSVolume.DataCacheHandler`.
    case system
    /// Every read and write goes to the extension.
    case none
    /// Reads are cached, writes go straight to the extension.
    case read
    /// Writes update the cache and are sent to the extension at once.
    case writeThrough
    /// Writes update the cache; the kernel sends them later.
    case writeBack

    init(defaultsValue: String?) {
        self = defaultsValue.flatMap { DataCachePolicy(rawValue: $0) } ?? .system
    }

    /// The coherency to grant for a requested cache mode: the policy,
    /// limited to what the mode permits (a mode without write caching
    /// never gets write caching).
    func grant(for mode: FSVolume.DataCacheMode) -> FSVolume.KernelCacheCoherencyType {
        switch (self, mode) {
        case (.system, _), (.none, _), (_, .none):
            return .noCache
        case (.read, _), (_, .readWithCache):
            return .readCache
        case (.writeThrough, _):
            return .writeThrough
        case (.writeBack, _):
            return .writeBack
        }
    }
}

extension FSVolume.DataCacheMode {
    var name: String {
        switch self {
        case .none: return "none"
        case .readWithCache: return "read"
        case .readWriteWithCache: return "readWrite"
        @unknown default: return "mode\(rawValue)"
        }
    }
}

extension FSVolume.KernelCacheCoherencyType {
    var name: String {
        switch self {
        case .noCache: return "noCache"
        case .readCache: return "readCache"
        case .writeThrough: return "writeThrough"
        case .writeBack: return "writeBack"
        @unknown default: return "coherency\(rawValue)"
        }
    }
}

extension FSVolume.OpenModes {
    var name: String {
        switch (contains(.read), contains(.write)) {
        case (true, true): return "rw"
        case (false, true): return "w"
        case (true, false): return "r"
        default: return "-"
        }
    }
}

// File contents only change through FSKit requests (the zeroing done while
// mapping kernel writes covers bytes the kernel already sees as zeros), so
// a granted coherency never has to be revoked or downgraded and
// `setCacheState` is not needed.
extension Ext4Volume: FSVolume.DataCacheHandler {
    var isDataCacheInhibited: Bool { dataCache == .system }

    func open(
        _ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSOpenItemResult?, (any Error)?) -> Void
    ) {
        let granted = dataCache.grant(for: cacheMode)
        Log.fs.debug("open \(item.description, privacy: .public) \(modes.name) \(cacheMode.name) -> \(granted.name)")
        opLock.lock()
        stats.add("open \(modes.name) \(cacheMode.name) -> \(granted.name)")
        opLock.unlock()
        reply(FSOpenItemResult(grantedCoherency: granted), nil)
    }

    func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void) {
        Log.fs.debug("close \(item.description, privacy: .public)")
        opLock.lock()
        stats.add("close")
        opLock.unlock()
        reply()
    }

    func upgrade(
        _ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSUpgradeItemResult?, (any Error)?) -> Void
    ) {
        let granted = dataCache.grant(for: cacheMode)
        opLock.lock()
        stats.add("upgrade \(cacheMode.name) -> \(granted.name)")
        opLock.unlock()
        reply(FSUpgradeItemResult(grantedCoherency: granted), nil)
    }
}
