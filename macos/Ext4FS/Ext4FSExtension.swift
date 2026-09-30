import ExtensionFoundation
import FSKit
import Foundation

@main
struct Ext4FSExtension: UnaryFileSystemExtension {
    let fileSystem = Ext4FileSystem()
}
