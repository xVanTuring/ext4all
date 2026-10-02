import Foundation

/// Command-line use of the app's keychain entries, for scripts and
/// testing. Run the app's executable directly:
///
///     /Applications/Ext4Kit.app/Contents/MacOS/Ext4Kit add-passphrase
///     /Applications/Ext4Kit.app/Contents/MacOS/Ext4Kit add-key-file NAME < FILE
///     /Applications/Ext4Kit.app/Contents/MacOS/Ext4Kit list
///     /Applications/Ext4Kit.app/Contents/MacOS/Ext4Kit remove ID
///
/// Secrets come from standard input (a terminal prompt hides what is
/// typed); the sandboxed app may not open arbitrary paths itself.
enum KeyCommand {
    static let usage = """
        usage: Ext4Kit add-passphrase            (reads the passphrase from standard input)
               Ext4Kit add-key-file NAME < FILE  (stores the contents of FILE)
               Ext4Kit list
               Ext4Kit remove ID
        """

    static let commands: Set<String> = ["add-passphrase", "add-key-file", "list", "remove", "help"]

    /// Run a command if the arguments name one; returns its exit status,
    /// or nil to start the app normally.
    static func run(_ args: [String], store: SecretStore = KeychainStore.shared) -> Int32? {
        guard args.count >= 2, commands.contains(args[1]) else { return nil }
        do {
            switch args[1] {
            case "add-passphrase":
                let p = readSecret(prompt: "Passphrase: ")
                guard !p.isEmpty else { return fail("empty passphrase") }
                try store.addSecret(p, kind: .passphrase, name: "")
                print("saved")
            case "add-key-file":
                guard args.count == 3 else { return fail(usage) }
                let d = FileHandle.standardInput.readDataToEndOfFile()
                guard !d.isEmpty, d.count <= 8 << 20 else { return fail("a key file holds 1 byte to 8 MiB") }
                try store.addSecret(d, kind: .keyFile, name: args[2])
                print("saved")
            case "list":
                for e in store.entries() {
                    print("\(e.id)\t\(e.kind)\t\(e.name)\t\(e.volume ?? "")")
                }
            case "remove":
                guard args.count == 3 else { return fail(usage) }
                try store.remove(args[2])
            default:
                print(usage)
            }
            return 0
        } catch {
            return fail(error.localizedDescription)
        }
    }

    private static func fail(_ message: String) -> Int32 {
        FileHandle.standardError.write(Data("Ext4Kit: \(message)\n".utf8))
        return 1
    }

    /// One line from standard input, without echo when it is a terminal.
    private static func readSecret(prompt: String) -> Data {
        if isatty(STDIN_FILENO) != 0 {
            var buf = [CChar](repeating: 0, count: 1024)
            guard let p = readpassphrase(prompt, &buf, buf.count, 0) else { return Data() }
            let d = Data(String(cString: p).utf8)
            buf.withUnsafeMutableBytes { _ = memset_s($0.baseAddress, $0.count, 0, $0.count) }
            return d
        }
        var d = FileHandle.standardInput.readDataToEndOfFile()
        if d.last == UInt8(ascii: "\n") {
            d.removeLast()
        }
        return d
    }
}
