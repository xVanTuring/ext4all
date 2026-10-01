import Foundation

/// Counts of the file data requests FSKit sends to one volume, logged when
/// it is unmounted. Shows which I/O path (read/write handler or kernel
/// offloaded I/O) and which cache modes the kernel actually uses. Not
/// thread safe: update it with `opLock` held.
struct IOStats {
    struct Counter: Equatable {
        var calls = 0
        var bytes: UInt64 = 0
    }

    private(set) var counters: [String: Counter] = [:]

    mutating func add(_ event: String, bytes: Int = 0) {
        counters[event, default: Counter()].calls += 1
        counters[event, default: Counter()].bytes += UInt64(max(bytes, 0))
    }

    /// Request size buckets, so the summary shows how the kernel splits I/O.
    static func bucket(_ length: Int) -> String {
        switch length {
        case ..<4096: return "<4K"
        case ..<65536: return "4K-64K"
        case ..<1_048_576: return "64K-1M"
        default: return ">=1M"
        }
    }

    var isEmpty: Bool { counters.isEmpty }

    /// One line per event, sorted by name: `event: calls (bytes)`.
    var summary: [String] {
        counters.keys.sorted().map { key in
            let c = counters[key]!
            return c.bytes > 0 ? "\(key): \(c.calls) (\(c.bytes) bytes)" : "\(key): \(c.calls)"
        }
    }
}
