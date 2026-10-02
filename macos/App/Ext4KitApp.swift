import SwiftUI

@main
enum Main {
    static func main() {
        // `Ext4Kit COMMAND ...` from a terminal manages keychain entries
        if let status = KeyCommand.run(CommandLine.arguments) {
            exit(status)
        }
        Ext4KitApp.main()
    }
}

struct Ext4KitApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
                .frame(minWidth: 560, minHeight: 520)
        }
        .windowResizability(.contentSize)
    }
}
