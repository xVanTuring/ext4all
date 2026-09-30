import Ext4FFI
import FSKit
import XCTest

final class FSKitLayerTests: XCTestCase {
    func testMountOptionParsing() {
        XCTAssertFalse(Ext4FileSystem.wantsReadOnly(["-o", "noowners"]))
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["-r"]))
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["-o", "nosuid,ro"]))
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["-o", "rdonly"]))
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["-oro"]))
        XCTAssertFalse(Ext4FileSystem.wantsReadOnly(["-o", "rw"]))
        XCTAssertFalse(Ext4FileSystem.wantsReadOnly(["-o"]))
        XCTAssertFalse(Ext4FileSystem.wantsReadOnly([]))
        // the option FSKit itself passes for read-only loads
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["--rdonly"]))
        XCTAssertTrue(Ext4FileSystem.wantsReadOnly(["-f", "--rdonly"]))
        XCTAssertFalse(Ext4FileSystem.wantsReadOnly(["-f"]))

        XCTAssertEqual(Ext4FileSystem.mountOptions(["-o", "a,b", "-oc", "-r"]), ["a", "b", "c"])
        XCTAssertTrue(Ext4FileSystem.wantsKernelIO([], defaultOn: true))
        XCTAssertFalse(Ext4FileSystem.wantsKernelIO([], defaultOn: false))
        XCTAssertFalse(Ext4FileSystem.wantsKernelIO(["-o", "noowners,nokoio"], defaultOn: true))
        XCTAssertTrue(Ext4FileSystem.wantsKernelIO(["-okoio"], defaultOn: false))
        XCTAssertFalse(Ext4FileSystem.wantsKernelIO(["-o", "koio,nokoio"], defaultOn: true), "nokoio wins")
    }

    func testKernelIOAttributeDecision() {
        var a = Ext4Attr()
        a.ino = 20
        a.file_type = UInt8(EXT4_FT_REG)
        a.flags = UInt32(EXT4_FL_EXTENTS)
        XCTAssertTrue(a.supportsKernelIO)
        XCTAssertTrue(FSItem.Attributes(a, parent: 2).inhibitKernelOffloadedIO, "off unless the volume opts in")
        XCTAssertFalse(FSItem.Attributes(a, parent: 2, kernelIO: true).inhibitKernelOffloadedIO)
        var inline = a
        inline.flags |= UInt32(EXT4_FL_INLINE_DATA)
        XCTAssertFalse(inline.supportsKernelIO)
        XCTAssertTrue(FSItem.Attributes(inline, parent: 2, kernelIO: true).inhibitKernelOffloadedIO)
        var blockMapped = a
        blockMapped.flags = 0
        XCTAssertTrue(FSItem.Attributes(blockMapped, parent: 2, kernelIO: true).inhibitKernelOffloadedIO)
        var dir = a
        dir.file_type = UInt8(EXT4_FT_DIR)
        XCTAssertTrue(FSItem.Attributes(dir, parent: 2, kernelIO: true).inhibitKernelOffloadedIO)
    }

    func testExtentPackingSplitsAtLimit() {
        var packed: [(FSExtentType, UInt64, UInt64, UInt64)] = []
        let e = Ext4IOExtent(logical: 8192, physical: 1 << 20, length: 10000, zeroFill: false)
        XCTAssertTrue(
            Ext4KernelIOVolume.pack(e, maxLength: 4096) { t, l, p, n in
                packed.append((t, l, p, n))
                return true
            })
        XCTAssertEqual(packed.map { $0.3 }, [4096, 4096, 1808])
        XCTAssertEqual(packed.map { $0.1 }, [8192, 12288, 16384])
        XCTAssertEqual(packed.map { $0.2 }, [1 << 20, (1 << 20) + 4096, (1 << 20) + 8192])
        XCTAssertTrue(packed.allSatisfy { $0.0 == .data })

        packed.removeAll()
        let hole = Ext4IOExtent(logical: 0, physical: 999, length: 8192, zeroFill: true)
        XCTAssertFalse(
            Ext4KernelIOVolume.pack(hole, maxLength: 4096) { t, l, p, n in
                packed.append((t, l, p, n))
                return false  // packer full after the first extent
            })
        XCTAssertEqual(packed.count, 1)
        XCTAssertEqual(packed[0].0, .zeroFill)
        XCTAssertEqual(packed[0].2, 0, "zero-fill extents carry no device offset")

        XCTAssertTrue(Ext4KernelIOVolume.succeeded(nil))
        XCTAssertFalse(Ext4KernelIOVolume.succeeded(POSIXError(.EIO)))
    }

    /// The I/O path of a live item never changes, even when an inline data
    /// file grows into extents.
    func testKernelIOPathIsStablePerItem() throws {
        try XCTSkipUnless(TestImage.available, "e2fsprogs not installed")
        // mke2fs -d stores small files as inline data
        let src = FileManager.default.temporaryDirectory.appendingPathComponent("inline-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: src, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: src) }
        try Data("tiny".utf8).write(to: src.appendingPathComponent("small"))
        let path = try TestImage.make(
            options: ["-t", "ext4", "-b", "4096", "-O", "inline_data", "-I", "256", "-d", src.path])
        let mount = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let volume = Ext4Volume(mount: mount, info: try mount.volumeInfo(), bsdName: "disk99s1", kernelIO: true)
        XCTAssertTrue(volume.kernelIO)

        let small = try mount.lookup(2, Data("small".utf8))
        XCTAssertFalse(small.supportsKernelIO, "inline data file")
        let item = volume.items.item(for: small.ino, parent: 2)
        XCTAssertTrue(try volume.attributes(item).inhibitKernelOffloadedIO)
        // grow past the inline capacity: the engine converts to extents
        _ = try mount.write(small.ino, offset: 0, data: Data(count: 100_000))
        XCTAssertTrue(try mount.stat(small.ino).supportsKernelIO)
        XCTAssertTrue(try volume.attributes(item).inhibitKernelOffloadedIO, "decided once per item")
        // a fresh item for the same inode decides again
        volume.items.remove(item)
        let again = volume.items.item(for: small.ino, parent: 2)
        XCTAssertFalse(try volume.attributes(again).inhibitKernelOffloadedIO)

        // a volume without kernel I/O inhibits everything
        let plain = Ext4Volume(mount: mount, info: try mount.volumeInfo(), bsdName: "disk99s1")
        let p = plain.items.item(for: small.ino, parent: 2)
        XCTAssertTrue(try plain.attributes(p).inhibitKernelOffloadedIO)
        try mount.unmount()
        try TestImage.assertClean(path)
    }

    func testItemTypeMapping() {
        let pairs: [(Int32, FSItem.ItemType)] = [
            (EXT4_FT_REG, .file), (EXT4_FT_DIR, .directory), (EXT4_FT_LNK, .symlink), (EXT4_FT_FIFO, .fifo),
            (EXT4_FT_CHR, .charDevice), (EXT4_FT_BLK, .blockDevice), (EXT4_FT_SOCK, .socket),
        ]
        for (raw, t) in pairs {
            XCTAssertEqual(FSItem.ItemType(ext4Type: UInt8(raw)), t)
            XCTAssertEqual(t.ext4Type, UInt8(raw))
        }
        XCTAssertEqual(FSItem.ItemType(ext4Type: 0), .unknown)
        XCTAssertNil(FSItem.ItemType.unknown.ext4Type)
    }

    func testItemTableReusesObjects() {
        let t = ItemTable()
        let a = t.item(for: 12, parent: 2)
        let b = t.item(for: 12, parent: 5)
        XCTAssertTrue(a === b)
        XCTAssertEqual(b.parentIno, 5)
        let root = t.item(for: 2, parent: 2)
        _ = t.item(for: 2, parent: 99)
        XCTAssertEqual(root.parentIno, 2, "root parent is fixed")
        XCTAssertEqual(t.count, 2)
        t.remove(12)
        XCTAssertFalse(t.item(for: 12, parent: 2) === a)
        t.removeAll()
        XCTAssertEqual(t.count, 0)
    }

    func testAttributesConversion() {
        var a = Ext4Attr()
        a.ino = 42
        a.file_type = UInt8(EXT4_FT_REG)
        a.mode = 0o100640
        a.nlink = 3
        a.uid = 1000
        a.gid = 1000
        a.size = 12345
        a.allocated = 16384
        a.mtime = Ext4Time(sec: 100, nsec: 5)
        a.ctime = Ext4Time(sec: 200, nsec: 6)
        a.atime = Ext4Time(sec: 300, nsec: 7)
        a.has_crtime = true
        a.crtime = Ext4Time(sec: 50, nsec: 1)
        a.bsd_flags = 2
        let f = FSItem.Attributes(a, parent: 7)
        XCTAssertEqual(f.type, .file)
        XCTAssertEqual(f.mode, 0o100640)
        XCTAssertEqual(f.linkCount, 3)
        XCTAssertEqual(f.uid, 1000)
        XCTAssertEqual(f.size, 12345)
        XCTAssertEqual(f.allocSize, 16384)
        XCTAssertEqual(f.fileID.rawValue, 42)
        XCTAssertEqual(f.parentID.rawValue, 7)
        XCTAssertEqual(f.modifyTime.tv_sec, 100)
        XCTAssertEqual(f.modifyTime.tv_nsec, 5)
        XCTAssertEqual(f.birthTime.tv_sec, 50)
        XCTAssertEqual(f.flags, 2)
        XCTAssertTrue(f.inhibitKernelOffloadedIO)

        var r = Ext4Attr()
        r.ino = 2
        r.file_type = UInt8(EXT4_FT_DIR)
        let rf = FSItem.Attributes(r, parent: 2)
        XCTAssertEqual(rf.fileID, .rootDirectory)
        XCTAssertEqual(rf.parentID, .parentOfRoot)
        XCTAssertEqual(rf.birthTime.tv_sec, rf.changeTime.tv_sec, "no crtime falls back to ctime")
    }

    func testVolumeHandlersWithoutContext() throws {
        try XCTSkipUnless(TestImage.available, "e2fsprogs not installed")
        let path = try TestImage.make(options: ["-t", "ext4", "-b", "4096"], label: "vol")
        let mount = try Ext4Mount(FileBlockIO(path: path, readOnly: false), readOnly: false)
        let volume = Ext4Volume(mount: mount, info: try mount.volumeInfo(), bsdName: "disk99s1")
        XCTAssertEqual(volume.name.string, "vol")
        XCTAssertEqual(volume.maximumNameLength, 255)
        XCTAssertEqual(volume.maximumLinkCount, 65000)
        XCTAssertTrue(volume.supportedVolumeCapabilities.supportsHardLinks)
        XCTAssertEqual(volume.supportedVolumeCapabilities.caseFormat, .sensitive)
        XCTAssertEqual(volume.requestedMountOptions, [])

        let stats = volume.volumeStatistics
        XCTAssertEqual(stats.fileSystemTypeName, "ext4")
        XCTAssertEqual(stats.blockSize, 4096)
        XCTAssertGreaterThan(stats.freeBlocks, 0)
        XCTAssertLessThanOrEqual(stats.availableBlocks, stats.freeBlocks)

        // FSTaskOptions/FSContext cannot be created outside FSKit, so only
        // handlers without them are exercised here.
        _ = volume.items.item(for: 2, parent: 2)

        // write + read through the handler using an item from the table
        let a = try mount.create(2, Data("f".utf8), type: UInt8(EXT4_FT_REG), perm: 0o644, uid: 0, gid: 0)
        let item = volume.items.item(for: a.ino, parent: 2)
        let wrote = expectation(description: "write")
        volume.write(contents: Data("handler data".utf8), to: item, at: 3) { result, error in
            XCTAssertNil(error)
            XCTAssertNotNil(result)
            wrote.fulfill()
        }
        wait(for: [wrote], timeout: 5)
        XCTAssertEqual(try mount.read(a.ino, offset: 0, length: 100), Data([0, 0, 0]) + Data("handler data".utf8))

        let failed = expectation(description: "negative offset")
        volume.write(contents: Data("x".utf8), to: item, at: -1) { _, error in
            XCTAssertEqual(posixCode(error!), .EINVAL)
            failed.fulfill()
        }
        wait(for: [failed], timeout: 5)

        let synced = expectation(description: "sync")
        volume.synchronize(flags: .wait) { error in
            XCTAssertNil(error)
            synced.fulfill()
        }
        wait(for: [synced], timeout: 5)

        let reclaimed = expectation(description: "reclaim")
        volume.reclaimItem(item) { error in
            XCTAssertNil(error)
            reclaimed.fulfill()
        }
        wait(for: [reclaimed], timeout: 5)
        // Outside the FSKit daemon the item was never handed to the kernel,
        // so tryReclaim may decline; either way the table stays consistent.
        XCTAssertLessThanOrEqual(volume.items.count, 2)

        // handlers that hand out items reply while holding the volume lock
        // (so reclaim cannot run between the table lookup and the reply);
        // the others reply after releasing it
        volume.run(
            "locked", replyUnderLock: true,
            { (_: Int?, _) in
                XCTAssertFalse(volume.opLock.try(), "reply must run under the lock")
            }
        ) { 1 }
        volume.run(
            "unlocked",
            { (_: Int?, _) in
                XCTAssertTrue(volume.opLock.try(), "reply must run after unlocking")
                volume.opLock.unlock()
            }
        ) { 1 }
        volume.run(
            "failing", replyUnderLock: true,
            { (v: Int?, e) in
                XCTAssertNil(v)
                XCTAssertNotNil(e)
                XCTAssertFalse(volume.opLock.try())
            }
        ) { throw POSIXError(.EIO) }
        XCTAssertTrue(volume.opLock.try(), "lock released afterwards")
        volume.opLock.unlock()

        let checked = try volume.quickCheck()
        XCTAssertTrue(checked.contains { $0.contains("ext4 volume \"vol\"") }, "\(checked)")

        let unmounted = expectation(description: "unmount")
        volume.unmount { unmounted.fulfill() }
        wait(for: [unmounted], timeout: 10)
        // clean on disk right after unmount, before deactivation
        try TestImage.assertClean(path)
        // FSKit reclaims items after unmount: must still succeed
        let late = volume.items.item(for: 2, parent: 2)
        let lateReclaim = expectation(description: "late reclaim")
        volume.reclaimItem(late) { error in
            XCTAssertNil(error)
            lateReclaim.fulfill()
        }
        wait(for: [lateReclaim], timeout: 5)
        XCTAssertTrue(volume.mount.isCurrentlyReadOnly)
        XCTAssertGreaterThan(volume.volumeStatistics.totalBlocks, 0)
        let deactivated = expectation(description: "deactivate")
        volume.deactivateVolume(options: []) { error in
            XCTAssertNil(error)
            deactivated.fulfill()
        }
        wait(for: [deactivated], timeout: 5)
        try TestImage.assertClean(path)
    }
}
