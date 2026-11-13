import AppIntents
import SwiftUI
import WidgetKit

/// Why a widget has nothing fresh to show.
enum WidgetProblem: Equatable, Sendable {
    case notConfigured
    case unreachable
}

struct OnAirEntry: TimelineEntry {
    let date: Date
    let snapshot: OnAirSnapshot?
    let fetchedAt: Date?
    let isStale: Bool
    let avatars: [String: AvatarImage]
    let problem: WidgetProblem?

    var relevance: TimelineEntryRelevance? {
        TimelineEntryRelevance(score: OnAirPolicy.relevanceScore(for: snapshot))
    }

    static func fixture(_ snapshot: OnAirSnapshot, stale: Bool = false, avatars: Bool = true) -> OnAirEntry {
        OnAirEntry(
            date: Date(),
            snapshot: snapshot,
            fetchedAt: stale ? Date().addingTimeInterval(-47 * 60) : Date(),
            isStale: stale,
            avatars: avatars ? WidgetFixtures.avatars(for: snapshot) : [:],
            problem: nil
        )
    }
}

/// `TimelineProvider` callbacks are not `Sendable`; WidgetKit calls them once.
struct UncheckedCompletion<T>: @unchecked Sendable {
    let call: (T) -> Void
}

struct OnAirProvider: TimelineProvider {
    static let cacheName = "on-air"

    func placeholder(in _: Context) -> OnAirEntry {
        .fixture(WidgetFixtures.liveMany())
    }

    func getSnapshot(in context: Context, completion: @escaping (OnAirEntry) -> Void) {
        if context.isPreview {
            completion(.fixture(WidgetFixtures.liveMany()))
            return
        }
        let done = UncheckedCompletion(call: completion)
        let family = context.family
        Task { done.call(await Self.entry(family: family)) }
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<OnAirEntry>) -> Void) {
        let done = UncheckedCompletion(call: completion)
        let family = context.family
        Task {
            let entry = await Self.entry(family: family)
            let next = Date().addingTimeInterval(OnAirPolicy.refreshInterval(for: entry.snapshot))
            done.call(Timeline(entries: [entry], policy: .after(next)))
        }
    }

    static func entry(family: WidgetFamily) async -> OnAirEntry {
        let (cached, error) = await SnapshotStore.resolve(cacheName, maxAge: OnAirPolicy.hardExpiry) { try await OmniAPIClient().onAir() }
        let now = Date()
        let problem: WidgetProblem? = switch error {
        case nil: nil
        case OmniAPIError.notConfigured?: .notConfigured
        default: .unreachable
        }
        let avatars = await loadAvatars(for: cached?.value, family: family)
        return OnAirEntry(
            date: now,
            snapshot: cached?.value,
            fetchedAt: cached?.fetchedAt,
            isStale: OnAirPolicy.isStale(fetchedAt: cached?.fetchedAt, now: now),
            avatars: avatars,
            problem: problem
        )
    }

    /// Only the avatars this family draws, at 3x their largest point size.
    private static func loadAvatars(for snapshot: OnAirSnapshot?, family: WidgetFamily) async -> [String: AvatarImage] {
        guard let snapshot else { return [:] }
        let budget = OnAirPolicy.avatarBudget(familyRawName: family.omniName)
        guard budget > 0 else { return [:] }
        var requests: [(id: String, url: URL)] = snapshot.live.compactMap { s in s.avatarUrl.map { (s.id, $0) } }
        if snapshot.live.count < budget {
            requests += snapshot.recent.compactMap { r in r.avatarUrl.map { (r.id, $0) } }
        }
        return await AvatarStore.avatars(for: Array(requests.prefix(budget)), maxPixels: family.avatarPixels)
    }
}

extension WidgetFamily {
    /// Stable names for pure policy code (which has no WidgetKit dependency).
    var omniName: String {
        switch self {
        case .systemSmall: "systemSmall"
        case .systemMedium: "systemMedium"
        case .systemLarge: "systemLarge"
        case .systemExtraLarge: "systemExtraLarge"
        case .accessoryCircular: "accessoryCircular"
        case .accessoryRectangular: "accessoryRectangular"
        case .accessoryInline: "accessoryInline"
        default: "other"
        }
    }

    /// 3x the largest avatar point size drawn in this family.
    var avatarPixels: Int {
        switch self {
        case .systemExtraLarge: 288
        case .systemSmall, .systemMedium: 192
        default: 120
        }
    }
}

/// Opens one stream from a widget row.
///
/// Rows in medium and larger widgets are `Button(intent:)`, which runs this intent in
/// the extension; returning `OpenURLIntent` with the https watch URL lets iOS open the
/// platform app through its universal link without first launching Omni. Families that
/// are a single tap target use `widgetURL(omni://open?...)` instead, which always goes
/// through the app (it forwards immediately, trying the app-scheme URL first).
struct OpenStreamIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Stream"
    static let isDiscoverable = false

    @Parameter(title: "URL")
    var url: URL

    init() {
        url = URL(string: "http://omni.boris")!
    }

    init(url: URL) {
        self.url = url
    }

    func perform() async throws -> some IntentResult & OpensIntent {
        .result(opensIntent: OpenURLIntent(url))
    }
}

enum OmniWidgetLinks {
    static var server: URL {
        OmniSettings.shared.serverURL ?? URL(string: "http://omni.boris")!
    }

    static var livePage: URL { server.appending(path: "live") }

    /// The single-target URL for a streamer, forwarded by the app.
    static func open(_ streamer: OnAirStreamer) -> URL {
        OmniDeepLink.openURL(target: streamer.watchUrl, appURL: streamer.appUrl)
    }

    static func open(page: URL) -> URL {
        OmniDeepLink.openURL(target: page)
    }
}

struct OnAirWidget: Widget {
    var body: some WidgetConfiguration {
        StaticConfiguration(kind: OmniWidgetKind.onAir, provider: OnAirProvider()) { entry in
            OnAirWidgetEntryView(entry: entry)
        }
        .configurationDisplayName("On Air")
        .description("Who's live right now. Tap a stream to open it in its app.")
        .supportedFamilies([
            .systemSmall, .systemMedium, .systemLarge, .systemExtraLarge,
            .accessoryCircular, .accessoryRectangular, .accessoryInline,
        ])
        .pushHandler(OmniWidgetPushHandler.self)
    }
}

struct OnAirWidgetEntryView: View {
    @Environment(\.widgetFamily) private var family
    let entry: OnAirEntry

    var body: some View {
        OnAirView(entry: entry, family: family)
    }
}
