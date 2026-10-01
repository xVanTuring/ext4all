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
            // opt-in; read-only mounts benefit as well (reads bypass the
            // extension)
            let kernelIO = UserDefaults.standard.bool(forKey: Ext4FileSystem.kernelIODefaultsKey)
            let volume: Ext4Volume =
                kernelIO
                ? Ext4KernelIOVolume(mount: mount, info: info, resource: device)
                : Ext4Volume(mount: mount, info: info, bsdName: device.bsdName)
            loaded.withLock { $0 = volume }
            containerStatus = .ready
            Log.fs.info(
                "loaded \(device.bsdName, privacy: .public) \(mount.isReadOnly ? "read-only" : "read-write", privacy: .public)\(kernelIO ? ", kernel offloaded I/O" : "", privacy: .public)"
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
        if opts.contains(where: { $0 == "--rdonly" || $0 == "-r" || $0 == "--read-only" }) { return true }
        let o = mountOptions(opts)
        return o.contains("ro") || o.contains("rdonly")
    }

    /// Defaults key (in the extension's container) that opts volumes into
    /// kernel offloaded I/O. It is read when a volume is loaded, because
    /// the volume's class decides which FSKit protocols it implements and
    /// `-o` mount options only arrive later, at activation.
    static let kernelIODefaultsKey = "KernelOffloadedIO"

    /// With kernel offloaded I/O available, `-o nokoio` switches it off for
    /// one mount (and `-o koio` on); otherwise `defaultOn` decides.
    static func wantsKernelIO(_ opts: [String], defaultOn: Bool) -> Bool {
        let o = mountOptions(opts)
        if o.contains("nokoio") { return false }
        if o.contains("koio") { return true }
        return defaultOn
    }

    /// The comma separated words of every `-o` option (`-o a,b` or `-oa,b`).
    static func mountOptions(_ opts: [String]) -> Set<String> {
        var out = Set<String>()
        for (i, o) in opts.enumerated() {
            var list: Substring?
            if o == "-o", i + 1 < opts.count {
                list = Substring(opts[i + 1])
            } else if o.hasPrefix("-o") && o.count > 2 {
                list = o.dropFirst(2)
            }
            for part in list?.split(separator: ",") ?? [] {
                out.insert(String(part))
            }
        }
        return out
    }
}
