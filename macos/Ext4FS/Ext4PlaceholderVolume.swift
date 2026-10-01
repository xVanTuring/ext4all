import FSKit
import Foundation

/// Stands in for a device that holds no usable ext4 file system. FSKit
/// loads the resource before formatting it (with the same `-f` option as
/// before mounting), so loading must succeed; everything else fails with
/// the reason the real load failed.
final class Ext4PlaceholderVolume: FSVolume, FSVolume.Handler, @unchecked Sendable {
    /// Why the device could not be loaded as ext4.
    let failure: any Error

    init(bsdName: String, failure: any Error) {
        self.failure = failure
        super.init(volumeID: FSVolume.Identifier(uuid: UUID()), volumeName: FSFileName(string: bsdName))
    }

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities { FSVolume.SupportedCapabilities() }
    var volumeStatistics: FSStatFSResult { FSStatFSResult(fileSystemTypeName: "ext4") }
    var maximumLinkCount: Int { 65000 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }

    func activateVolume(
        options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func deactivateVolume(
        options: FSDeactivateOptions = [], replyHandler reply: @escaping @Sendable ((any Error)?) -> Void
    ) {
        reply(nil)
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        reply(failure)
    }

    func unmount(replyHandler reply: @escaping @Sendable () -> Void) {
        reply()
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        reply(nil)
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func lookupItem(
        named name: FSFileName, in directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        reply(nil)
    }

    func readSymbolicLink(
        _ item: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func createItem(
        named name: FSFileName, type: FSItem.ItemType, in directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func createSymbolicLink(
        named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func createLink(
        to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func removeItem(
        _ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func renameItem(
        _ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName,
        to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?,
        context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }

    func enumerateDirectory(
        _ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext,
        replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void
    ) {
        reply(nil, failure)
    }
}
