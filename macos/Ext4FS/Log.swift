import Ext4FFI
import Foundation
import os

enum Log {
    static let subsystem = "tech.xvanturing.ext4.fs"
    static let fs = Logger(subsystem: subsystem, category: "fs")
    static let engine = Logger(subsystem: subsystem, category: "engine")

    /// Route the Rust engine's log output to the unified log (once).
    static let installEngineLogger: Void = {
        ext4_set_log_callback { level, msg in
            guard let msg else { return }
            let s = String(cString: msg)
            switch level {
            case 1: Log.engine.error("\(s, privacy: .public)")
            case 2: Log.engine.warning("\(s, privacy: .public)")
            case 3: Log.engine.info("\(s, privacy: .public)")
            default: Log.engine.debug("\(s, privacy: .public)")
            }
        }
    }()
}
