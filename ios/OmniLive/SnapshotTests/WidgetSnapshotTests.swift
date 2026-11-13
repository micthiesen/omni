import SwiftUI
import WidgetKit
import XCTest

/// Renders every widget family and state to PNG for design review.
///
/// Output: `/tmp/omni-widget-snapshots` (override with `OMNI_SNAPSHOT_DIR`, passed as
/// `TEST_RUNNER_OMNI_SNAPSHOT_DIR`). Nothing is compared or committed; the files are
/// for looking at. Accented and vibrant modes are approximations: the test applies the
/// rendering-mode environment so the views adapt, then tints/desaturates the result the
/// way the Home Screen and Lock Screen roughly do.
@MainActor
final class WidgetSnapshotTests: XCTestCase {
    enum Mode: String { case full, accented, vibrant }

    /// iPhone 17 Pro widget sizes; extra large is the iPad Pro 13-inch size.
    static func size(_ family: WidgetFamily) -> CGSize {
        switch family {
        case .systemSmall: CGSize(width: 170, height: 170)
        case .systemMedium: CGSize(width: 364, height: 170)
        case .systemLarge: CGSize(width: 364, height: 382)
        case .systemExtraLarge: CGSize(width: 796, height: 376)
        case .accessoryCircular: CGSize(width: 72, height: 72)
        case .accessoryRectangular: CGSize(width: 172, height: 76)
        case .accessoryInline: CGSize(width: 234, height: 26)
        default: CGSize(width: 170, height: 170)
        }
    }

    var outputDirectory: URL {
        let env = ProcessInfo.processInfo.environment["OMNI_SNAPSHOT_DIR"]
        return URL(fileURLWithPath: env ?? "/tmp/omni-widget-snapshots", isDirectory: true)
    }

    override func setUp() async throws {
        OmniFont.registerBundled(in: Bundle(for: WidgetSnapshotTests.self))
        try FileManager.default.createDirectory(at: outputDirectory, withIntermediateDirectories: true)
    }

    func testFontsRegistered() {
        XCTAssertFalse(UIFont.fontNames(forFamilyName: "Geist").isEmpty, "Geist must load from the bundle")
        XCTAssertFalse(UIFont.fontNames(forFamilyName: "Geist Mono").isEmpty, "Geist Mono must load")
    }

    func testRenderOnAir() throws {
        let homeFamilies: [WidgetFamily] = [.systemSmall, .systemMedium, .systemLarge, .systemExtraLarge]
        let lockFamilies: [WidgetFamily] = [.accessoryCircular, .accessoryRectangular, .accessoryInline]
        let states: [(String, OnAirEntry)] = [
            ("live-many", .fixture(WidgetFixtures.liveMany())),
            ("live-one", .fixture(WidgetFixtures.liveOne())),
            ("live-two", .fixture(WidgetFixtures.liveTwo())),
            ("nobody", .fixture(WidgetFixtures.nobody())),
            ("nobody-no-recent", .fixture(WidgetFixtures.nobodyNoRecent())),
            ("stale", .fixture(WidgetFixtures.liveMany(), stale: true)),
            ("long-names", .fixture(WidgetFixtures.longNames())),
            ("no-avatars", .fixture(WidgetFixtures.liveMany(), avatars: false)),
            ("unreachable", OnAirEntry(date: Date(), snapshot: nil, fetchedAt: nil, isStale: false,
                                       avatars: [:], problem: .unreachable)),
        ]
        var sheet: [(String, AnyView, CGSize)] = []
        for (name, entry) in states {
            for family in homeFamilies {
                for mode in [Mode.full, .accented] where mode == .full || name == "live-many" || name == "nobody" {
                    let view = framed(OnAirView(entry: entry, family: family), family: family, mode: mode)
                    let label = "on-air-\(family.omniName)-\(name)-\(mode.rawValue)"
                    try write(view, size: Self.size(family), name: label)
                    if mode == .full || name == "live-many" { sheet.append((label, AnyView(view), Self.size(family))) }
                }
            }
            for family in lockFamilies where ["live-many", "live-one", "nobody", "unreachable"].contains(name) {
                let view = framed(OnAirView(entry: entry, family: family), family: family, mode: .vibrant)
                let label = "on-air-\(family.omniName)-\(name)-vibrant"
                try write(view, size: Self.size(family), name: label)
                sheet.append((label, AnyView(view), Self.size(family)))
            }
        }
        try writeSheet(sheet, name: "sheet-on-air")
    }

    // MARK: Rendering

    /// Recreates what WidgetKit adds around the view: the container background, the
    /// 16 pt content margins and the rounded widget shape, or the Lock Screen style.
    private func framed<V: View>(_ view: V, family: WidgetFamily, mode: Mode) -> AnyView {
        let size = Self.size(family)
        let isAccessory = [.accessoryCircular, .accessoryRectangular, .accessoryInline].contains(family)
        if isAccessory {
            return AnyView(
                view
                    .environment(\.widgetRenderingMode, .vibrant)
                    .foregroundStyle(.white)
                    .grayscale(1)
                    .frame(width: size.width, height: size.height)
                    .background(
                        LinearGradient(colors: [Color(hex: 0x2B3A55), Color(hex: 0x14161F)],
                                       startPoint: .top, endPoint: .bottom)
                    )
            )
        }
        let background: AnyView = switch view {
        case let onAir as OnAirView: AnyView(onAir.backdrop.environment(\.widgetRenderingMode, mode.widgetMode))
        default: AnyView(WidgetBackdrop(glow: nil))
        }
        let content = view
            .environment(\.widgetRenderingMode, mode.widgetMode)
            .padding(16)
            .frame(width: size.width, height: size.height)
        if mode == .accented {
            // Tinted Home Screen: no container background, content monochrome over glass.
            return AnyView(
                content
                    .grayscale(1)
                    .brightness(0.12)
                    .background(
                        LinearGradient(colors: [Color(hex: 0x30405E), Color(hex: 0x1C2333)],
                                       startPoint: .topLeading, endPoint: .bottomTrailing)
                    )
                    .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
            )
        }
        return AnyView(
            content
                .background(background)
                .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
        )
    }

    private func write(_ view: AnyView, size: CGSize, name: String) throws {
        let renderer = ImageRenderer(content: view.frame(width: size.width, height: size.height))
        renderer.scale = 3
        guard let image = renderer.uiImage, let data = image.pngData() else {
            XCTFail("Could not render \(name)")
            return
        }
        try data.write(to: outputDirectory.appending(path: "\(name).png"))
    }

    /// One labelled contact sheet per widget kind, easier to review than 80 files.
    private func writeSheet(_ items: [(String, AnyView, CGSize)], name: String) throws {
        let columns = 3
        let rows = stride(from: 0, to: items.count, by: columns).map { Array(items[$0 ..< min($0 + columns, items.count)]) }
        let sheet = VStack(alignment: .leading, spacing: 24) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(alignment: .top, spacing: 24) {
                    ForEach(Array(row.enumerated()), id: \.offset) { _, item in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(item.0).font(.system(size: 11, design: .monospaced)).foregroundStyle(.gray)
                            item.1.frame(width: item.2.width, height: item.2.height)
                        }
                    }
                }
            }
        }
        .padding(24)
        .background(Color(hex: 0x3A3F47))
        let renderer = ImageRenderer(content: sheet)
        renderer.scale = 2
        guard let image = renderer.uiImage, let data = image.pngData() else {
            XCTFail("Could not render \(name)")
            return
        }
        try data.write(to: outputDirectory.appending(path: "\(name).png"))
    }
}

private extension WidgetSnapshotTests.Mode {
    var widgetMode: WidgetRenderingMode {
        switch self {
        case .full: .fullColor
        case .accented: .accented
        case .vibrant: .vibrant
        }
    }
}
