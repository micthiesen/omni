import Foundation

/// Pure timeline decisions for the On Air widget.
enum OnAirPolicy {
    /// A snapshot older than this shows a subtle "as of HH:mm".
    static let staleAfter: TimeInterval = 15 * 60
    /// Past this, cached live data is no longer shown at all.
    static let hardExpiry: TimeInterval = 3 * 60 * 60

    /// Reload sooner while someone is live; widget pushes cover the edges between.
    static func refreshInterval(for snapshot: OnAirSnapshot?) -> TimeInterval {
        guard let snapshot, !snapshot.live.isEmpty else { return 30 * 60 }
        return 15 * 60
    }

    static func isStale(fetchedAt: Date?, now: Date) -> Bool {
        guard let fetchedAt else { return false }
        return now.timeIntervalSince(fetchedAt) > staleAfter
    }

    /// Smart Stack relevance: a primary streamer on air matters most.
    static func relevanceScore(for snapshot: OnAirSnapshot?) -> Float {
        guard let snapshot, !snapshot.live.isEmpty else { return 0 }
        return snapshot.hasPrimaryLive ? 1 : 0.2
    }

    /// How many avatars a family can show, which bounds downloads and decoded memory.
    static func avatarBudget(familyRawName: String) -> Int {
        switch familyRawName {
        case "systemSmall": 4
        case "systemMedium": 3
        case "systemLarge": 6
        case "systemExtraLarge": 8
        case "accessoryCircular": 1
        default: 0
        }
    }
}

/// Compact numbers and short relative times shared by widgets and tests.
enum OmniFormat {
    /// 999 -> "999", 1_234 -> "1.2K", 12_345 -> "12.3K", 123_456 -> "123K", 1_234_567 -> "1.2M".
    static func compact(_ value: Int, locale: Locale = .current) -> String {
        let style = IntegerFormatStyle<Int>.number.notation(.compactName).locale(locale)
        let magnitude = abs(value)
        let leading = magnitude >= 1_000_000 ? magnitude / 1_000_000 : magnitude / 1_000
        return leading >= 100
            ? value.formatted(style.precision(.fractionLength(0)))
            : value.formatted(style.precision(.fractionLength(0 ... 1)))
    }

    /// "3h ago", "12m ago", "2d ago", "now".
    static func ago(_ date: Date, now: Date) -> String {
        let seconds = max(0, now.timeIntervalSince(date))
        switch seconds {
        case ..<60: return "now"
        case ..<3_600: return "\(Int(seconds / 60))m ago"
        case ..<86_400: return "\(Int(seconds / 3_600))h ago"
        default: return "\(Int(seconds / 86_400))d ago"
        }
    }

    /// "4h 12m", "38m" for uptime when a ticking timer is not wanted.
    static func duration(since start: Date, now: Date) -> String {
        let minutes = max(0, Int(now.timeIntervalSince(start) / 60))
        let hours = minutes / 60
        return hours > 0 ? "\(hours)h \(minutes % 60)m" : "\(minutes)m"
    }
}
