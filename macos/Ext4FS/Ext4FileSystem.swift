import FSKit
import Foundation

/// A value guarded by a lock.
final class Locked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()
    init(_ value: T) { self.value = value }
    func withLock<R>(_ body: (inout T) throws -> R) rethrows -> R {
        lock.lock()
        defer { lock.unlock() }
        return try body(&value)
    }
}

/// The FSKit module: probes block devices for ext2/3/4 and loads volumes.
final class Ext4FileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations, FSManageableResourceMaintenanceOperations,
    @unchecked Sendable
{
    /// The volume created by `loadResource` (a unary file system has one).
    let loaded = Locked<Ext4Volume?>(nil)

    override init() {
        _ = Log.installEngineLogger
        super.init()
        Log.fs.info("ext4 module loaded, engine \(Ext4Mount.version, privacy: .public)")
    }

    func probeResource(
        resource: FSResource, replyHandler reply: @escaping @Sendable (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard let device = resource as? FSBlockDeviceResource else {
            reply(.notRecognized, nil)
            return
        }
        do {
            let info = try Ext4Mount.probe(ResourceBlockIO(device, readOnly: true))
            let container = FSContainerIdentifier(uuid: info.uuid)
            Log.fs.info(
                "probe \(device.bsdName, privacy: .public): ext4 \"\(info.label, privacy: .public)\" support=\(String(describing: info.support), privacy: .public)"
            )
            switch info.support {
            case .unsupported:
                reply(.recognized(name: info.label, containerID: container), nil)
            case .readOnly, .readWrite:
                reply(.usable(name: info.label, containerID: container), nil)
            }
        } catch {
            Log.fs.debug(
                "probe \(device.bsdName, privacy: .public): not ext4 (\(error.localizedDescription, privacy: .public))")
            reply(.notRecognized, nil)
        }
    }

    func loadResource(
        resource: FSResource, options: FSTaskOptions,
        replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void
    ) {
        guard let device = resource as? FSBlockDeviceResource else {
            reply(nil, POSIXError(.EINVAL))
            return
        }
        let readOnly = Ext4FileSystem.wantsReadOnly(options)
        do {
            let io = ResourceBlockIO(device, readOnly: readOnly)
            let mount = try Ext4Mount(io, readOnly: readOnly)
            let info = try mount.volumeInfo()
            let volume = Ext4Volume(mount: mount, info: info, bsdName: device.bsdName)
            loaded.withLock { $0 = volume }
            containerStatus = .ready
            Log.fs.info(
                "loaded \(device.bsdName, privacy: .public) \(mount.isReadOnly ? "read-only" : "read-write", privacy: .public)"
            )
            reply(volume, nil)
        } catch {
            Log.fs.error(
                "load \(device.bsdName, privacy: .public) failed: \(error.localizedDescription, privacy: .public)")
            containerStatus = .blocked(status: error)
            reply(nil, error)
        }
    }

    func unloadResource(
        resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void
    ) {
        loaded.withLock { $0 = nil }
        containerStatus = .notReady(status: POSIXError(.ENODEV))
        reply(nil)
    }

    func didFinishLoading() {
        Log.fs.debug("didFinishLoading")
    }

    // MARK: maintenance (the system checks block device volumes before
    // mounting them)

    func startCheck(task: FSTask, options: FSTaskOptions) throws -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let volume = loaded.withLock { $0 }
        DispatchQueue.global(qos: .userInitiated).async {
            var failure: (any Error)?
            if let volume {
                do {
                    for line in try volume.quickCheck() {
                        task.logMessage(line)
                    }
                } catch {
                    task.logMessage("ext4: check failed: \(error.localizedDescription)")
                    failure = error
                }
            } else {
                task.logMessage("ext4: no volume loaded")
            }
            progress.completedUnitCount = 1
            task.didComplete(error: failure)
        }
        return progress
    }

    func startFormat(task: FSTask, options: FSTaskOptions) throws -> Progress {
        throw fs_errorForPOSIXError(ENOTSUP)
    }

    /// FSKit passes `--rdonly` for read-only loads; `mount -r` and
    /// `-o ro` / `-o rdonly` are accepted as well.
    static func wantsReadOnly(_ options: FSTaskOptions) -> Bool {
        wantsReadOnly(options.taskOptions)
    }

    static func wantsReadOnly(_ opts: [String]) -> Bool {
        for (i, o) in opts.enumerated() {
            if o == "--rdonly" || o == "-r" || o == "--read-only" { return true }
            if o == "-o", i + 1 < opts.count {
                let parts = opts[i + 1].split(separator: ",")
                if parts.contains("ro") || parts.contains("rdonly") { return true }
            }
            if o.hasPrefix("-o") && o.count > 2 {
                let parts = o.dropFirst(2).split(separator: ",")
                if parts.contains("ro") || parts.contains("rdonly") { return true }
            }
        }
        return false
    }
}
