import CoreGraphics
import Foundation

/// Fixture data for previews, the widget gallery and snapshot tests. Times are
/// relative to `now` so ticking timers read sensibly.
enum WidgetFixtures {
    static func ms(_ date: Date) -> Double { date.timeIntervalSince1970 * 1_000 }

    static func streamer(
        _ id: String,
        _ name: String,
        platform: StreamPlatform,
        tier: StreamerTier = .primary,
        viewers: Int?,
        minutesLive: Double,
        title: String?,
        category: String? = nil,
        headline: String? = nil,
        now: Date = Date()
    ) -> OnAirStreamer {
        let watch: URL = switch platform {
        case .kick: URL(string: "https://kick.com/\(id)")!
        case .twitch: URL(string: "https://www.twitch.tv/\(id)")!
        case .youtube, .other: URL(string: "https://www.youtube.com/@\(id)/live")!
        }
        return OnAirStreamer(
            id: id,
            name: name,
            tier: tier,
            platform: platform,
            title: title,
            category: category,
            viewers: viewers,
            peakViewers: viewers.map { Int(Double($0) * 1.08) },
            startedAt: ms(now.addingTimeInterval(-minutesLive * 60)),
            avatarUrl: URL(string: "https://example.invalid/\(id).png"),
            watchUrl: watch,
            appUrl: platform == .twitch ? URL(string: "twitch://stream/\(id)") : nil,
            headline: headline
        )
    }

    static func recent(_ now: Date = Date()) -> [OnAirRecent] {
        [
            OnAirRecent(id: "destiny", name: "Destiny", platform: .kick,
                        endedAt: ms(now.addingTimeInterval(-3 * 3_600)), avatarUrl: nil,
                        watchUrl: URL(string: "https://kick.com/destiny")),
            OnAirRecent(id: "vinesauce", name: "Vinesauce", platform: .twitch,
                        endedAt: ms(now.addingTimeInterval(-9 * 3_600)), avatarUrl: nil,
                        watchUrl: URL(string: "https://www.twitch.tv/vinesauce")),
            OnAirRecent(id: "hasanabi", name: "HasanAbi", platform: .twitch,
                        endedAt: ms(now.addingTimeInterval(-26 * 3_600)), avatarUrl: nil,
                        watchUrl: URL(string: "https://www.twitch.tv/hasanabi")),
            OnAirRecent(id: "lemonade", name: "Lemonade Stand", platform: .youtube,
                        endedAt: ms(now.addingTimeInterval(-50 * 3_600)), avatarUrl: nil,
                        watchUrl: URL(string: "https://www.youtube.com/@lemonade/live")),
        ]
    }

    static func liveMany(_ now: Date = Date()) -> OnAirSnapshot {
        let live = [
            streamer("destiny", "Destiny", platform: .kick, viewers: 12_431, minutesLive: 134,
                     title: "DEBATE NIGHT — reacting to the town hall, then call-ins",
                     category: "Just Chatting",
                     headline: "Viewers up 38% since the town hall clip started", now: now),
            streamer("hasanabi", "HasanAbi", platform: .twitch, viewers: 28_904, minutesLive: 312,
                     title: "news and drama, then Elden Ring with chat", category: "Just Chatting", now: now),
            streamer("vinesauce", "Vinesauce", platform: .twitch, viewers: 3_210, minutesLive: 47,
                     title: "Vinny - Corruptions & Mario Party", category: "Super Mario Party", now: now),
            streamer("lemonade", "Lemonade Stand", platform: .youtube, viewers: 1_870, minutesLive: 82,
                     title: "Lemonade Stand Podcast #412", category: nil, now: now),
            streamer("pxie", "Pxie", platform: .kick, tier: .background, viewers: 940, minutesLive: 21,
                     title: "chill sunday", category: "IRL", now: now),
            streamer("nyanners", "Nyanners", platform: .youtube, tier: .background, viewers: 612,
                     minutesLive: 190, title: "karaoke!!", category: "Music", now: now),
        ]
        return OnAirSnapshot(
            generatedAt: ms(now),
            liveCount: 8,
            totalViewers: live.compactMap(\.viewers).reduce(0, +) + 1_200,
            live: live,
            recent: recent(now)
        )
    }

    static func liveOne(_ now: Date = Date()) -> OnAirSnapshot {
        let one = streamer("destiny", "Destiny", platform: .kick, viewers: 12_431, minutesLive: 134,
                           title: "DEBATE NIGHT — reacting to the town hall, then call-ins",
                           category: "Just Chatting",
                           headline: "Viewers up 38% since the town hall clip started", now: now)
        return OnAirSnapshot(generatedAt: ms(now), liveCount: 1, totalViewers: 12_431, live: [one],
                             recent: Array(recent(now).dropFirst()))
    }

    static func liveTwo(_ now: Date = Date()) -> OnAirSnapshot {
        let many = liveMany(now)
        let two = Array(many.live.prefix(2))
        return OnAirSnapshot(generatedAt: ms(now), liveCount: 2,
                             totalViewers: two.compactMap(\.viewers).reduce(0, +), live: two,
                             recent: Array(recent(now).suffix(2)))
    }

    static func longNames(_ now: Date = Date()) -> OnAirSnapshot {
        let live = [
            streamer("longname", "TheExtremelyLongStreamerNameThatNeverEnds", platform: .youtube,
                     viewers: 1_234_567, minutesLive: 1_440,
                     title: "A very long stream title that goes on and on, with emoji 🎮🔥 and more words to wrap",
                     category: "An Unreasonably Long Category Name", headline: nil, now: now),
            streamer("x", "X", platform: .kick, viewers: nil, minutesLive: 3, title: nil, now: now),
            streamer("mid", "Medium Length Name", platform: .twitch, viewers: 99_999, minutesLive: 600,
                     title: "", category: "Games + Demos", now: now),
        ]
        return OnAirSnapshot(generatedAt: ms(now), liveCount: 3, totalViewers: 1_334_566, live: live,
                             recent: [])
    }

    static func nobody(_ now: Date = Date()) -> OnAirSnapshot {
        OnAirSnapshot(generatedAt: ms(now), liveCount: 0, totalViewers: 0, live: [], recent: recent(now))
    }

    static func nobodyNoRecent(_ now: Date = Date()) -> OnAirSnapshot {
        OnAirSnapshot(generatedAt: ms(now), liveCount: 0, totalViewers: 0, live: [], recent: [])
    }

    // MARK: Avatars

    /// Synthetic "photos": soft two-tone portraits with a distinct hue per id.
    static func avatars(for snapshot: OnAirSnapshot, skipping skipped: Set<String> = []) -> [String: AvatarImage] {
        var result: [String: AvatarImage] = [:]
        let palette: [(Double, Double, Double)] = [
            (0.86, 0.42, 0.30), (0.42, 0.30, 0.82), (0.25, 0.62, 0.48),
            (0.90, 0.72, 0.30), (0.30, 0.55, 0.90), (0.80, 0.35, 0.60),
        ]
        for (index, streamer) in snapshot.live.enumerated() where !skipped.contains(streamer.id) {
            if let image = portrait(palette[index % palette.count]) { result[streamer.id] = image }
        }
        for (index, recent) in snapshot.recent.enumerated() where !skipped.contains(recent.id) {
            if result[recent.id] == nil, let image = portrait(palette[(index + 2) % palette.count]) {
                result[recent.id] = image
            }
        }
        return result
    }

    private static func portrait(_ rgb: (Double, Double, Double)) -> AvatarImage? {
        let size = 192
        guard let context = CGContext(
            data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        let (r, g, b) = rgb
        let colors = [
            CGColor(red: r, green: g, blue: b, alpha: 1),
            CGColor(red: r * 0.35, green: g * 0.35, blue: b * 0.35, alpha: 1),
        ] as CFArray
        guard let gradient = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: colors, locations: [0, 1])
        else { return nil }
        context.drawLinearGradient(gradient, start: CGPoint(x: 0, y: CGFloat(size)), end: CGPoint(x: CGFloat(size), y: 0), options: [])
        // Head and shoulders silhouette.
        context.setFillColor(CGColor(red: min(1, r + 0.35), green: min(1, g + 0.3), blue: min(1, b + 0.25), alpha: 0.9))
        context.fillEllipse(in: CGRect(x: 62, y: 92, width: 68, height: 74))
        context.fillEllipse(in: CGRect(x: 26, y: -40, width: 140, height: 110))
        guard let image = context.makeImage() else { return nil }
        let (ar, ag, ab) = AvatarStore.averageColor(of: image)
        return AvatarImage(image: image, red: ar, green: ag, blue: ab)
    }
}
