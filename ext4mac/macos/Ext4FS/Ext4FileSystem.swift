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
    /// The device handed to `loadResource`, kept even when it holds no
    /// ext4 file system: formatting works on it.
    let device = Locked<FSBlockDeviceResource?>(nil)
    /// Why the device could not be loaded as ext4 (a placeholder volume
    /// was handed out instead).
    let loadFailure = Locked<(any Error)?>(nil)
    /// Keys for LUKS volumes and fscrypt directories.
    let unlocker = Unlocker(store: KeychainStore.shared, defaults: .standard)

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
        let io = ResourceBlockIO(device, readOnly: true)
        do {
            if let luks = try Ext4Mount.luksProbe(io) {
                reply(probeLuks(luks, io: io, bsdName: device.bsdName), nil)
                return
            }
        } catch {
            Log.fs.info(
                "probe \(device.bsdName, privacy: .public): damaged LUKS header (\(error.localizedDescription, privacy: .public))"
            )
        }
        do {
            let info = try Ext4Mount.probe(io)
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

    /// A LUKS volume is usable when its key is remembered (named after the
    /// ext4 inside) or when the user's secrets may open it (loading tries
    /// them); otherwise it is only recognized and not mounted.
    func probeLuks(_ luks: Ext4LuksVolume, io: BlockIO, bsdName: String) -> FSProbeResult {
        let container = FSContainerIdentifier(uuid: UUID(uuidString: luks.uuid) ?? UUID())
        let name = luks.label.isEmpty ? "LUKS" : luks.label
        let result: FSProbeResult
        if !luks.supported {
            result = .recognized(name: name, containerID: container)
        } else if let key = unlocker.rememberedLuksKey(io: io, volume: luks),
            let inner = try? Ext4Mount.luksProbeInner(io, key: key), inner.support != .unsupported
        {
            result = .usable(name: inner.label.isEmpty ? name : inner.label, containerID: container)
        } else if unlocker.mayUnlock(luks) {
            result = .usable(name: name, containerID: container)
        } else {
            result = .recognized(name: name, containerID: container)
        }
        Log.fs.info(
            "probe \(bsdName, privacy: .public): LUKS\(luks.version) \(luks.uuid, privacy: .public) \(String(describing: result), privacy: .public)"
        )
        return result
    }

    /// Mount the device: ext4 directly, or the ext4 inside a LUKS volume
    /// (whose key must be remembered or come from the user's secrets).
    /// Returns the mount and whether it is a LUKS volume.
    func mountDevice(_ io: BlockIO, readOnly: Bool, bsdName: String) throws -> (Ext4Mount, Bool) {
        guard let luks = try Ext4Mount.luksProbe(io) else {
            return (try Ext4Mount(io, readOnly: readOnly), false)
        }
        guard luks.supported else {
            Log.fs.error("\(bsdName, privacy: .public): LUKS volume with an unsupported cipher")
            throw LuksUnavailable(error: POSIXError(.ENOTSUP))
        }
        guard let key = try unlocker.luksKey(io: io, volume: luks) else {
            Log.fs.error(
                "\(bsdName, privacy: .public): LUKS volume \(luks.uuid, privacy: .public) is locked; add its passphrase in Ext4Kit"
            )
            throw LuksUnavailable(error: POSIXError(.EACCES))
        }
        return (try Ext4Mount(luks: io, key: key, readOnly: readOnly), true)
    }

    /// A LUKS volume that cannot be opened (no key opens it, or its cipher
    /// is unsupported).
    struct LuksUnavailable: Error {
        let error: POSIXError
    }

    func loadResource(
        resource: FSResource, options: FSTaskOptions,
        replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void
    ) {
        guard let device = resource as? FSBlockDeviceResource else {
            reply(nil, POSIXError(.EINVAL))
            return
        }
        Log.fs.info(
            "loadResource \(device.bsdName, privacy: .public) options \(options.taskOptions, privacy: .public)")
        self.device.withLock { $0 = device }
        loadFailure.withLock { $0 = nil }
        let readOnly = Ext4FileSystem.wantsReadOnly(options)
        do {
            let io = ResourceBlockIO(device, readOnly: readOnly)
            let (mount, isLuks) = try mountDevice(io, readOnly: readOnly, bsdName: device.bsdName)
            let info = try mount.volumeInfo()
            // keys go in before FSKit sees any name
            if info.encrypt {
                unlocker.unlockFscrypt(mount, info: info)
            }
            // opt-in; read-only mounts benefit as well (reads bypass the
            // extension). Never for LUKS: the kernel would read ciphertext.
            let kernelIO = !isLuks && UserDefaults.standard.bool(forKey: Ext4FileSystem.kernelIODefaultsKey)
            let parallelReads = UserDefaults.standard.bool(forKey: Ext4FileSystem.parallelReadsDefaultsKey)
            let volume: Ext4Volume =
                kernelIO
                ? Ext4KernelIOVolume(mount: mount, info: info, resource: device, parallelReads: parallelReads)
                : Ext4Volume(mount: mount, info: info, bsdName: device.bsdName, parallelReads: parallelReads)
            loaded.withLock { $0 = volume }
            containerStatus = .ready
            Log.fs.info(
                "loaded \(device.bsdName, privacy: .public) \(mount.isReadOnly ? "read-only" : "read-write", privacy: .public)\(kernelIO ? ", kernel offloaded I/O" : "", privacy: .public)\(parallelReads ? ", parallel reads" : "", privacy: .public)"
            )
            reply(volume, nil)
        } catch let locked as LuksUnavailable {
            // No stand-in here: FSKit would keep the device loaded (and
            // busy, even for ejecting) after the mount fails, so a mount
            // retried once a passphrase is added would not reach us.
            Log.fs.error(
                "load \(device.bsdName, privacy: .public) failed: \(locked.error.localizedDescription, privacy: .public)"
            )
            self.device.withLock { $0 = nil }
            containerStatus = .notReady(status: locked.error)
            reply(nil, locked.error)
        } catch {
            // FSKit also loads a device before formatting it, so loading
            // succeeds with a stand-in that fails activation (and checks)
            // with this error
            Log.fs.error(
                "load \(device.bsdName, privacy: .public) failed: \(error.localizedDescription, privacy: .public)")
            loadFailure.withLock { $0 = error }
            containerStatus = .blocked(status: error)
            reply(Ext4PlaceholderVolume(bsdName: device.bsdName, failure: error), nil)
        }
    }

    func unloadResource(
        resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void
    ) {
        loaded.withLock { $0 = nil }
        device.withLock { $0 = nil }
        loadFailure.withLock { $0 = nil }
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
        let loadError = loadFailure.withLock { $0 }
        DispatchQueue.global(qos: .userInitiated).async {
            var failure: (any Error)?
            if let loadError {
                task.logMessage("ext4: no usable ext4 file system: \(loadError.localizedDescription)")
                failure = loadError
            } else if let volume {
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
                failure = POSIXError(.ENXIO)
            }
            progress.completedUnitCount = 1
            task.didComplete(error: failure)
        }
        return progress
    }

    /// Create a new ext4 file system (`newfs_fskit -t ext4 [mke2fs
    /// options] /dev/diskN`). Options are parsed by the engine; a bare
    /// `-E root_owner` gives the root directory to the requesting user.
    func startFormat(task: FSTask, options: FSTaskOptions) throws -> Progress {
        let args = options.taskOptions
        let target = device.withLock { $0 }
        Log.fs.info(
            "startFormat \(target?.bsdName ?? "(no device)", privacy: .public) options \(args, privacy: .public)")
        guard let target else { throw fs_errorForPOSIXError(ENXIO) }
        if let volume = loaded.withLock({ $0 }) {
            // an existing ext4 file system was loaded: never format under
            // an active volume; an inactive one is shut down first
            guard volume.releaseIfInactive() else {
                Log.fs.error("startFormat: \(target.bsdName, privacy: .public) is in use")
                throw fs_errorForPOSIXError(EBUSY)
            }
            loaded.withLock { $0 = nil }
        }
        let progress = Progress(totalUnitCount: 100)
        let (uid, gid) = (getuid(), getgid())
        // clear signatures of the previous file system (wipefs)
        wipe(target) { [self] wipeError in
            if let wipeError {
                Log.fs.info("wipe before format: \(wipeError.localizedDescription, privacy: .public)")
            }
            DispatchQueue.global(qos: .userInitiated).async { [self] in
                var failure: (any Error)?
                do {
                    let io = ResourceBlockIO(target, readOnly: false)
                    let r = try Ext4Mount.format(io, options: args, uid: uid, gid: gid) { done, total in
                        progress.completedUnitCount = Int64(done * 100 / max(total, 1))
                    }
                    let size = r.blocks * UInt64(r.blockSize)
                    task.logMessage(
                        "ext4: \(r.blocks) blocks of \(r.blockSize) bytes (\(size >> 20) MiB), \(r.inodes) inodes, \(r.groups) groups, journal of \(r.journalBlocks) blocks, UUID \(r.uuid.uuidString)"
                    )
                    Log.fs.info("formatted \(target.bsdName, privacy: .public): \(r.blocks) blocks, \(r.inodes) inodes")
                    loadFailure.withLock { $0 = nil }
                    containerStatus = .ready
                } catch {
                    Log.fs.error(
                        "format \(target.bsdName, privacy: .public) failed: \(error.localizedDescription, privacy: .public)"
                    )
                    task.logMessage("ext4: format failed: \(error.localizedDescription)")
                    failure = error
                }
                progress.completedUnitCount = 100
                task.didComplete(error: failure)
            }
        }
        return progress
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
    /// Defaults key that lets file data reads run in parallel
    /// (`Ext4Volume.parallelReads`); read when a volume is loaded.
    static let parallelReadsDefaultsKey = "ParallelReads"

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
