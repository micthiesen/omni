import Foundation

/// Widget kinds. The control kind lives in `OmniControlDefinition`.
enum OmniWidgetKind {
    static let onAir = "com.micthiesen.OmniLive.on-air"

    /// Maps the server's short names (`omni.refreshWidgets`) to widget kinds.
    static func kind(forServerName name: String) -> String? {
        switch name {
        case "on-air": onAir
        default: nil
        }
    }
}

enum StreamPlatform: String, Codable, Sendable, CaseIterable {
    case kick
    case twitch
    case youtube
    case other

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = StreamPlatform(rawValue: raw) ?? .other
    }

    var displayName: String {
        switch self {
        case .kick: "Kick"
        case .twitch: "Twitch"
        case .youtube: "YouTube"
        case .other: "Stream"
        }
    }
}

enum StreamerTier: String, Codable, Sendable {
    case primary
    case background

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = StreamerTier(rawValue: raw) ?? .background
    }
}

/// Epoch milliseconds to `Date`.
func dateFromMs(_ ms: Double) -> Date {
    Date(timeIntervalSince1970: ms / 1_000)
}

/// Decodes an optional URL string without failing the whole payload on a bad value.
private func lenientURL<K: CodingKey>(_ container: KeyedDecodingContainer<K>, _ key: K) -> URL? {
    guard let raw = try? container.decodeIfPresent(String.self, forKey: key),
          let url = URL(string: raw),
          url.scheme != nil
    else { return nil }
    return url
}

struct OnAirStreamer: Codable, Equatable, Sendable, Identifiable {
    let id: String
    let name: String
    let tier: StreamerTier
    let platform: StreamPlatform
    let title: String?
    let category: String?
    let viewers: Int?
    let peakViewers: Int?
    let startedAt: Double?
    let avatarUrl: URL?
    let watchUrl: URL
    let appUrl: URL?
    let headline: String?

    init(
        id: String,
        name: String,
        tier: StreamerTier,
        platform: StreamPlatform,
        title: String?,
        category: String?,
        viewers: Int?,
        peakViewers: Int?,
        startedAt: Double?,
        avatarUrl: URL?,
        watchUrl: URL,
        appUrl: URL?,
        headline: String?
    ) {
        self.id = id
        self.name = name
        self.tier = tier
        self.platform = platform
        self.title = title
        self.category = category
        self.viewers = viewers
        self.peakViewers = peakViewers
        self.startedAt = startedAt
        self.avatarUrl = avatarUrl
        self.watchUrl = watchUrl
        self.appUrl = appUrl
        self.headline = headline
    }

    private enum CodingKeys: String, CodingKey {
        case id, name, tier, platform, title, category, viewers, peakViewers, startedAt
        case avatarUrl, watchUrl, appUrl, headline
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        tier = try c.decodeIfPresent(StreamerTier.self, forKey: .tier) ?? .background
        platform = try c.decodeIfPresent(StreamPlatform.self, forKey: .platform) ?? .other
        title = try c.decodeIfPresent(String.self, forKey: .title)
        category = try c.decodeIfPresent(String.self, forKey: .category)
        viewers = try c.decodeIfPresent(Int.self, forKey: .viewers)
        peakViewers = try c.decodeIfPresent(Int.self, forKey: .peakViewers)
        startedAt = try c.decodeIfPresent(Double.self, forKey: .startedAt)
        avatarUrl = lenientURL(c, .avatarUrl)
        guard let watch = lenientURL(c, .watchUrl) else {
            throw DecodingError.dataCorruptedError(
                forKey: .watchUrl, in: c, debugDescription: "watchUrl must be an absolute URL"
            )
        }
        watchUrl = watch
        appUrl = lenientURL(c, .appUrl)
        headline = try c.decodeIfPresent(String.self, forKey: .headline)
    }

    var startedDate: Date? { startedAt.map(dateFromMs) }

    /// Viewers now, else the session peak (same fallback as the live slots).
    var displayViewers: Int? { viewers ?? peakViewers }
}

struct OnAirRecent: Codable, Equatable, Sendable, Identifiable {
    let id: String
    let name: String
    let platform: StreamPlatform
    let endedAt: Double?
    let avatarUrl: URL?
    let watchUrl: URL?

    init(
        id: String,
        name: String,
        platform: StreamPlatform,
        endedAt: Double?,
        avatarUrl: URL?,
        watchUrl: URL?
    ) {
        self.id = id
        self.name = name
        self.platform = platform
        self.endedAt = endedAt
        self.avatarUrl = avatarUrl
        self.watchUrl = watchUrl
    }

    private enum CodingKeys: String, CodingKey {
        case id, name, platform, endedAt, avatarUrl, watchUrl
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        platform = try c.decodeIfPresent(StreamPlatform.self, forKey: .platform) ?? .other
        endedAt = try c.decodeIfPresent(Double.self, forKey: .endedAt)
        avatarUrl = lenientURL(c, .avatarUrl)
        watchUrl = lenientURL(c, .watchUrl)
    }

    var endedDate: Date? { endedAt.map(dateFromMs) }
}

struct OnAirSnapshot: Codable, Equatable, Sendable {
    let generatedAt: Double
    let liveCount: Int
    let totalViewers: Int
    let live: [OnAirStreamer]
    let recent: [OnAirRecent]

    init(
        generatedAt: Double,
        liveCount: Int,
        totalViewers: Int,
        live: [OnAirStreamer],
        recent: [OnAirRecent]
    ) {
        self.generatedAt = generatedAt
        self.liveCount = liveCount
        self.totalViewers = totalViewers
        self.live = live
        self.recent = recent
    }

    private enum CodingKeys: String, CodingKey {
        case generatedAt, liveCount, totalViewers, live, recent
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        generatedAt = try c.decode(Double.self, forKey: .generatedAt)
        live = try c.decodeIfPresent([OnAirStreamer].self, forKey: .live) ?? []
        recent = try c.decodeIfPresent([OnAirRecent].self, forKey: .recent) ?? []
        liveCount = try c.decodeIfPresent(Int.self, forKey: .liveCount) ?? live.count
        totalViewers = try c.decodeIfPresent(Int.self, forKey: .totalViewers) ?? 0
    }

    /// Live entries beyond those the server listed (it sends at most 8).
    var unlistedLiveCount: Int { max(0, liveCount - live.count) }

    var hasPrimaryLive: Bool { live.contains { $0.tier == .primary } }

    /// Recently ended streams that are not live again right now.
    var recentOffline: [OnAirRecent] {
        let liveIds = Set(live.map(\.id))
        return recent.filter { !liveIds.contains($0.id) }
    }
}
