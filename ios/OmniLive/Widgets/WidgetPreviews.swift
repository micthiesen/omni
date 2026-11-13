import SwiftUI
import WidgetKit

// Xcode canvas previews for every family. Each timeline steps through the states:
// many live, one live, long names, missing avatars, stale, nobody live, unreachable.
// Accented rendering is available from the canvas's rendering-mode control.

private let onAirStates: [OnAirEntry] = [
    .fixture(WidgetFixtures.liveMany()),
    .fixture(WidgetFixtures.liveOne()),
    .fixture(WidgetFixtures.longNames()),
    .fixture(WidgetFixtures.liveMany(), avatars: false),
    .fixture(WidgetFixtures.liveMany(), stale: true),
    .fixture(WidgetFixtures.nobody()),
    .fixture(WidgetFixtures.nobodyNoRecent()),
    OnAirEntry(date: .now, snapshot: nil, fetchedAt: nil, isStale: false, avatars: [:], problem: .unreachable),
]

#Preview("On Air small", as: .systemSmall) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air medium", as: .systemMedium) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air large", as: .systemLarge) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air extra large", as: .systemExtraLarge) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air circular", as: .accessoryCircular) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air rectangular", as: .accessoryRectangular) { OnAirWidget() } timeline: { for e in onAirStates { e } }
#Preview("On Air inline", as: .accessoryInline) { OnAirWidget() } timeline: { for e in onAirStates { e } }
