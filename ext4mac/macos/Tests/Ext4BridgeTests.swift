import Ext4FFI
import XCTest

final class Ext4BridgeTests: XCTestCase {
    override func setUpWithError() throws {
        try XCTSkipUnless(TestImage.available, "e2fsprogs not installed")
    }

    func testVersion() {
        XCTAssertFalse(Ext4Mount.version.isEmpty)
    }

    func testProbe() throws {
        let path = try TestImage.make(label: "probe-me")
        let info = try Ext4Mount.probe(StrictBlockIO(path: path, readOnly: true))
        XCTAssertEqual(info.label, "probe-me")
        XCTAssertEqual(info.support, .readWrite)
        XCTAssertFalse(info.needsRecovery)
        XCTAssertGreaterThan(info.blocks, 0)
        let (_, dump) = try TestImage.run("dumpe2fs", ["-h", path])
        XCTAssertTrue(dump.lowercased().contains(info.uuid.uuidString.lowercased()))
    }

    func testFormat() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ext4kit-format-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let path = dir.appendingPathComponent("blank.img").path
        FileManager.default.createFile(atPath: path, contents: nil)
        let h = try FileHandle(forWritingTo: URL(fileURLWithPath: path))
        try h.truncate(atOffset: 600 << 20)
        try h.close()

        var reports: [(UInt64, UInt64)] = []
        let r = try Ext4Mount.format(
            StrictBlockIO(path: path, readOnly: false), options: ["-L", "swiftfmt", "-E", "root_owner", "-m", "0"],
            uid: 501, gid: 20
        ) { reports.append(($0, $1)) }
        XCTAssertEqual(r.blockSize, 4096)
        XCTAssertEqual(r.blocks, 153600)
        XCTAssertEqual(r.journalBlocks, 4096)
        XCTAssertGreaterThan(reports.count, 2)
        XCTAssertEqual(reports.last?.0, reports.last?.1)
        try TestImage.assertClean(path)

        let info = try Ext4Mount.probe(FileBlockIO(path: path, readOnly: true))
        XCTAssertEqual(info.label, "swiftfmt")
        XCTAssertEqual(info.uuid, r.uuid)
        XCTAssertEqual(info.support, .readWrite)
        let mount = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let root = try mount.stat(Ext4Mount.rootIno)
        XCTAssertEqual(root.uid, 501)
        _ = try mount.create(
            Ext4Mount.rootIno, Data("after-format".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        try mount.unmount()
        try TestImage.assertClean(path)

        XCTAssertThrowsError(
            try Ext4Mount.format(FileBlockIO(path: path, readOnly: false), options: ["-O", "bigalloc"])
        ) { XCTAssertEqual(posixCode($0), .EINVAL) }
        try TestImage.assertClean(path)
    }

    func testProbeReadOnlyAndUnsupported() throws {
        let ext3 = try TestImage.make(options: ["-t", "ext3"])
        XCTAssertEqual(try Ext4Mount.probe(StrictBlockIO(path: ext3, readOnly: true)).support, .readWrite)
        let bigalloc = try TestImage.make(sizeMB: 64, options: ["-t", "ext4", "-O", "bigalloc", "-C", "16384"])
        XCTAssertEqual(try Ext4Mount.probe(StrictBlockIO(path: bigalloc, readOnly: true)).support, .readOnly)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let zero = dir.appendingPathComponent("zero.img").path
        FileManager.default.createFile(atPath: zero, contents: Data(count: 1 << 20))
        XCTAssertThrowsError(try Ext4Mount.probe(StrictBlockIO(path: zero, readOnly: true)))
    }

    func testFullLifecycle() throws {
        let path = try TestImage.make(options: ["-t", "ext4", "-b", "4096"])
        let io = try StrictBlockIO(path: path, readOnly: false)
        let m = try Ext4Mount(io, readOnly: false)
        XCTAssertFalse(m.isReadOnly)
        let root = Ext4Mount.rootIno

        let f = try m.create(root, Data("hello.txt".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 501, gid: 20)
        XCTAssertTrue(f.isRegular)
        XCTAssertEqual(f.uid, 501)
        let payload = Data.pattern(200_000, seed: 7)
        XCTAssertEqual(try m.write(f.ino, offset: 0, data: payload), payload.count)
        XCTAssertEqual(try m.read(f.ino, offset: 0, length: payload.count + 10), payload)
        XCTAssertEqual(try m.read(f.ino, offset: 199_990, length: 100), payload.suffix(10))
        XCTAssertEqual(try m.stat(f.ino).size, UInt64(payload.count))

        let d = try m.create(root, Data("dir".utf8), type: UInt8(EXT4_FT_DIR), perm: 0o755, uid: 0, gid: 0)
        XCTAssertTrue(d.isDirectory)
        let v1 = try m.stat(d.ino).directoryVersion
        for i in 0..<30 {
            _ = try m.create(d.ino, Data("c\(i)".utf8), type: UInt8(EXT4_FT_REG), perm: 0o600, uid: 0, gid: 0)
        }
        XCTAssertNotEqual(try m.stat(d.ino).directoryVersion, v1)

        var names: [String] = []
        try m.readDir(d.ino, cookie: 0, skipDots: false, wantAttrs: false) { e in
            names.append(String(decoding: e.name, as: UTF8.self))
            XCTAssertNil(e.attr)
            return true
        }
        XCTAssertEqual(names.count, 32)
        XCTAssertTrue(names.contains(".") && names.contains(".."))

        var withAttrs = 0
        var resumeCookie: UInt64 = 0
        try m.readDir(d.ino, cookie: 0, skipDots: true, wantAttrs: true) { e in
            XCTAssertNotNil(e.attr)
            withAttrs += 1
            resumeCookie = e.nextCookie
            return withAttrs < 10
        }
        XCTAssertEqual(withAttrs, 10)
        var rest = 0
        try m.readDir(d.ino, cookie: resumeCookie, skipDots: true, wantAttrs: false) { _ in
            rest += 1
            return true
        }
        XCTAssertEqual(rest, 20)

        // xattrs use macOS names
        let key = Data("com.apple.FinderInfo".utf8)
        try m.setXattr(f.ino, key, Data(repeating: 1, count: 32), mode: .any)
        XCTAssertEqual(try m.getXattr(f.ino, key), Data(repeating: 1, count: 32))
        XCTAssertEqual(try m.listXattrs(f.ino), [key])
        XCTAssertThrowsError(try m.setXattr(f.ino, key, Data(), mode: .create)) {
            XCTAssertEqual(posixCode($0), .EEXIST)
        }
        try m.removeXattr(f.ino, key)
        XCTAssertThrowsError(try m.getXattr(f.ino, key)) { XCTAssertEqual(posixCode($0), .ENOATTR) }

        // symlink, link, rename
        let s = try m.symlink(root, Data("link".utf8), target: Data("hello.txt".utf8), uid: 0, gid: 0)
        XCTAssertTrue(s.isSymlink)
        XCTAssertEqual(try m.readLink(s.ino), Data("hello.txt".utf8))
        let l = try m.link(f.ino, to: d.ino, name: Data("hard".utf8))
        XCTAssertEqual(l.nlink, 2)
        try m.rename(root, Data("hello.txt".utf8), d.ino, Data("moved.txt".utf8))
        XCTAssertThrowsError(try m.lookup(root, Data("hello.txt".utf8))) { XCTAssertEqual(posixCode($0), .ENOENT) }
        XCTAssertEqual(try m.lookup(d.ino, Data("moved.txt".utf8)).ino, f.ino)

        // setattr
        var req = Ext4SetAttr()
        req.valid = UInt32(EXT4_SET_MODE | EXT4_SET_SIZE | EXT4_SET_MTIME)
        req.mode = 0o600
        req.size = 10
        req.mtime = Ext4Time(sec: 1_000_000, nsec: 42)
        let after = try m.setAttr(f.ino, req)
        XCTAssertEqual(after.mode & 0o7777, 0o600)
        XCTAssertEqual(after.size, 10)
        XCTAssertEqual(after.mtime.sec, 1_000_000)
        XCTAssertEqual(after.mtime.nsec, 42)

        // remove + reclaim
        try m.remove(d.ino, Data("moved.txt".utf8))
        try m.remove(d.ino, Data("hard".utf8))
        XCTAssertEqual(try m.read(f.ino, offset: 0, length: 10), payload.prefix(10))
        try m.reclaim(f.ino)
        XCTAssertThrowsError(try m.stat(f.ino))
        XCTAssertThrowsError(try m.remove(root, Data("dir".utf8))) { XCTAssertEqual(posixCode($0), .ENOTEMPTY) }

        // sparse helpers
        let g = try m.create(root, Data("sparse".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        try m.fallocate(g.ino, offset: 0, length: 1 << 20, keepSize: false)
        _ = try m.write(g.ino, offset: 65536, data: Data("x".utf8))
        XCTAssertEqual(try m.seek(g.ino, from: 0, data: true), 65536)
        XCTAssertEqual(try m.seek(g.ino, from: 65536, data: false), 65536 + 4096)
        try m.punchHole(g.ino, offset: 65536, length: 4096)

        // volume
        let st = try m.statfs()
        XCTAssertEqual(st.block_size, 4096)
        try m.setLabel("renamed")
        XCTAssertEqual(try m.volumeInfo().label, "renamed")
        try m.sync()
        try m.unmount()
        XCTAssertThrowsError(try m.stat(root)) { XCTAssertEqual(posixCode($0), .EBUSY) }
        XCTAssertGreaterThan(io.flushes, 0)
        try TestImage.assertClean(path)
    }

    func testErrorsMapToPOSIX() throws {
        let path = try TestImage.make()
        let m = try Ext4Mount(StrictBlockIO(path: path, readOnly: false, sectorSize: 512), readOnly: false)
        let root = Ext4Mount.rootIno
        XCTAssertThrowsError(try m.lookup(root, Data("nope".utf8))) { XCTAssertEqual(posixCode($0), .ENOENT) }
        _ = try m.create(root, Data("a".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        XCTAssertThrowsError(
            try m.create(root, Data("a".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        ) { XCTAssertEqual(posixCode($0), .EEXIST) }
        XCTAssertThrowsError(
            try m.create(root, Data(repeating: 0x61, count: 256), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        ) { XCTAssertEqual(posixCode($0), .ENAMETOOLONG) }
        XCTAssertThrowsError(try m.readLink(root)) { XCTAssertEqual(posixCode($0), .EINVAL) }
        try m.unmount()
        try TestImage.assertClean(path)
    }

    func testReadOnlyMount() throws {
        let path = try TestImage.make()
        let m = try Ext4Mount(StrictBlockIO(path: path, readOnly: true), readOnly: true)
        XCTAssertTrue(m.isReadOnly)
        XCTAssertThrowsError(
            try m.create(Ext4Mount.rootIno, Data("x".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        ) { XCTAssertEqual(posixCode($0), .EROFS) }
        XCTAssertTrue(try m.stat(Ext4Mount.rootIno).isDirectory)
    }

    func testConcurrentAccess() throws {
        let path = try TestImage.make(sizeMB: 64)
        let m = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let failures = NSLock()
        var errors: [Error] = []
        DispatchQueue.concurrentPerform(iterations: 8) { t in
            do {
                for i in 0..<25 {
                    let name = Data("t\(t)-\(i)".utf8)
                    let a = try m.create(Ext4Mount.rootIno, name, type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
                    let data = Data.pattern(5000 + i, seed: UInt8(t))
                    _ = try m.write(a.ino, offset: 0, data: data)
                    let back = try m.read(a.ino, offset: 0, length: data.count)
                    if back != data { throw POSIXError(.EIO) }
                }
            } catch {
                failures.lock()
                errors.append(error)
                failures.unlock()
            }
        }
        XCTAssertTrue(errors.isEmpty, "\(errors)")
        var count = 0
        try m.readDir(Ext4Mount.rootIno, cookie: 0, skipDots: true, wantAttrs: false) { _ in
            count += 1
            return true
        }
        XCTAssertEqual(count, 8 * 25 + 1)  // + lost+found
        try m.unmount()
        try TestImage.assertClean(path)
    }

    func testParallelReads() throws {
        let path = try TestImage.make(sizeMB: 64)
        let m = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        var files: [(ino: UInt32, data: Data)] = []
        for i in 0..<4 {
            let a = try m.create(
                Ext4Mount.rootIno, Data("p\(i)".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
            let data = Data.pattern(300_000 + i * 1000, seed: UInt8(i + 1))
            _ = try m.write(a.ino, offset: 0, data: data)
            files.append((a.ino, data))
        }
        let failures = NSLock()
        var errors: [String] = []
        DispatchQueue.concurrentPerform(iterations: 8) { t in
            for k in 0..<50 {
                let f = files[(t + k) % files.count]
                let offset = (t * 7919 + k * 104_729) % (f.data.count + 100)
                var buf = Data(count: 65536)
                do {
                    let n = try buf.withUnsafeMutableBytes { try m.readParallel(f.ino, offset: UInt64(offset), into: $0) }
                    let want = offset < f.data.count ? f.data.subdata(in: offset..<min(offset + 65536, f.data.count)) : Data()
                    if buf.prefix(n) != want {
                        failures.lock()
                        errors.append("inode \(f.ino) at \(offset): \(n) bytes differ")
                        failures.unlock()
                    }
                } catch {
                    failures.lock()
                    errors.append("inode \(f.ino) at \(offset): \(error)")
                    failures.unlock()
                }
            }
        }
        XCTAssertTrue(errors.isEmpty, "\(errors)")
        try m.unmount()
        try TestImage.assertClean(path)
    }

    func testParallelWrites() throws {
        let path = try TestImage.make(sizeMB: 64)
        let m = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let f = try m.create(Ext4Mount.rootIno, Data("w".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        // whole 64 KiB chunks, and odd sizes at odd offsets (the locked path)
        let chunks: [(offset: Int, data: Data)] = (0..<16).map { i in
            i % 4 == 3
                ? (i * 65536 + 100, Data.pattern(1000 + i, seed: UInt8(i)))
                : (i * 65536, Data.pattern(65536, seed: UInt8(i)))
        }
        let failures = NSLock()
        var errors: [String] = []
        DispatchQueue.concurrentPerform(iterations: chunks.count) { i in
            let c = chunks[i]
            do {
                let ticket = try m.reserveWrite(f.ino, offset: UInt64(c.offset), length: c.data.count)
                let n = try m.writeParallel(ticket: ticket, f.ino, offset: UInt64(c.offset), data: c.data)
                if n != c.data.count { throw POSIXError(.EIO) }
            } catch {
                failures.lock()
                errors.append("chunk \(i): \(error)")
                failures.unlock()
            }
        }
        XCTAssertTrue(errors.isEmpty, "\(errors)")
        var want = Data(count: 16 * 65536)
        for c in chunks {
            want.replaceSubrange(c.offset..<c.offset + c.data.count, with: c.data)
        }
        // the last chunk is short and unaligned: the file ends with it
        want = want.prefix(chunks[15].offset + chunks[15].data.count)
        XCTAssertEqual(try m.read(f.ino, offset: 0, length: want.count + 10), want)
        try m.unmount()
        try TestImage.assertClean(path)
    }

    /// Kernel offloaded I/O through the bridge: map for write, write the
    /// device directly at the mapped offsets (as the kernel would),
    /// complete, then read back through the engine and a read mapping.
    func testKernelIOMapping() throws {
        let path = try TestImage.make(options: ["-t", "ext4", "-b", "4096"])
        let m = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let f = try m.create(2, Data("koio".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        var writeMap: [Ext4IOExtent] = []
        try m.mapForIO(f.ino, offset: 0, length: 3 * 4096 + 100, write: true) { e in
            writeMap.append(e)
            return true
        }
        XCTAssertEqual(writeMap.reduce(0) { $0 + $1.length }, 4 * 4096)
        XCTAssertTrue(writeMap.allSatisfy { !$0.zeroFill })
        let payload = Data((0..<(3 * 4096 + 100)).map { UInt8(truncatingIfNeeded: $0 * 7) })
        let dev = try FileHandle(forWritingTo: URL(fileURLWithPath: path))
        for e in writeMap {
            let s = Int(e.logical)
            let t = min(s + Int(e.length), payload.count)
            try dev.seek(toOffset: e.physical)
            try dev.write(contentsOf: payload[s..<t])
        }
        try dev.close()
        // nothing is visible before completion
        XCTAssertEqual(try m.stat(f.ino).size, 0)
        try m.completeWrite(f.ino, offset: 0, length: UInt64(payload.count))
        XCTAssertEqual(try m.stat(f.ino).size, UInt64(payload.count))
        XCTAssertEqual(try m.read(f.ino, offset: 0, length: payload.count + 10), payload)
        var readMap: [Ext4IOExtent] = []
        try m.mapForIO(f.ino, offset: 0, length: 8 * 4096, write: false) { e in
            readMap.append(e)
            return true
        }
        XCTAssertEqual(readMap.filter { !$0.zeroFill }.reduce(0) { $0 + $1.length }, 4 * 4096)
        XCTAssertEqual(readMap.filter { $0.zeroFill }.reduce(0) { $0 + $1.length }, 4 * 4096)
        // stopping early is honoured
        var calls = 0
        try m.mapForIO(f.ino, offset: 0, length: 8 * 4096, write: false) { _ in
            calls += 1
            return false
        }
        XCTAssertEqual(calls, 1)
        // directories cannot be mapped
        XCTAssertThrowsError(try m.mapForIO(2, offset: 0, length: 4096, write: false) { _ in true })
        // a failed kernel write makes nothing visible
        try m.mapForIO(f.ino, offset: 16 * 4096, length: 4096, write: true) { _ in true }
        try m.abortWrite(f.ino, offset: 16 * 4096, length: 4096)
        XCTAssertEqual(try m.stat(f.ino).size, UInt64(payload.count))
        try m.unmount()
        try TestImage.assertClean(path)
    }

    func testTimespecConversion() {
        let t = Ext4Time(sec: -5, nsec: 123)
        let ts = t.timespecValue
        XCTAssertEqual(ts.tv_sec, -5)
        XCTAssertEqual(ts.tv_nsec, 123)
        XCTAssertEqual(Ext4Time(ts).sec, -5)
        XCTAssertEqual(Ext4Time(ts).nsec, 123)
    }
}
