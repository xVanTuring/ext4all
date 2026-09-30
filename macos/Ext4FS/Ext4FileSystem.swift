import FSKit
import Foundation

/// The FSKit module: probes block devices for ext2/3/4 and loads volumes.
final class Ext4FileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
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
        containerStatus = .notReady(status: POSIXError(.ENODEV))
        reply(nil)
    }

    func didFinishLoading() {
        Log.fs.debug("didFinishLoading")
    }

    /// `mount -r`, `-o ro` / `-o rdonly` request a read-only mount.
    static func wantsReadOnly(_ options: FSTaskOptions) -> Bool {
        wantsReadOnly(options.taskOptions)
    }

    static func wantsReadOnly(_ opts: [String]) -> Bool {
        for (i, o) in opts.enumerated() {
            if o == "-r" || o == "--read-only" { return true }
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
