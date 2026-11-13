# Omni iOS: app shell, notifications and widgets

The iOS app (`ios/OmniLive`, display name **Omni**) is a full-screen web view of
Omni plus three native pieces: push registration, a Notification Service
Extension, and Home Screen / Lock Screen widgets. The wire contract lives with
the server routes under `/api/ios-controls`; this note covers the iOS design
decisions. Setup and Control Center are in `docs/ios-app.md`.

## Targets

| Target | Bundle | Sources | Role |
|---|---|---|---|
| `OmniLive` (app) | `com.micthiesen.OmniLive` | `Shared`, `SharedControls`, `DesignKit`, `App` | Web shell, settings sheet, APNs registration, notification routing, `omni://` links |
| `OmniLiveControls` (WidgetKit) | `.controls` | `Shared`, `SharedControls`, `DesignKit`, `Controls`, `Widgets` | Live Slot control, On Air widget, `WidgetPushHandler` |
| `OmniNotificationService` | `.notification-service` | `Shared`, `NotificationService` | Sender avatars, action category, widget reloads |
| `OmniLiveTests` | | `Tests` | Contract and policy unit tests (hosted by the app) |
| `OmniWidgetSnapshotTests` | | `Shared`, `DesignKit`, `Widgets`, `SnapshotTests` | Hostless; renders every widget family to PNG |

`Shared` is Foundation-only (models, signed API client, settings, deep links,
policies, caches) so the service extension and the Mac static tests compile it
too. iOS 26.0 is the deployment target (WidgetKit push needs it).

## App shell

- One long-lived `WKWebView` (`WebShell`) owned by `AppModel`, so page state and
  history survive app switches. Canvas `#0a0b0d` behind everything: no white
  flash, launch screen color `LaunchBackground`.
- iPhone is portrait and truly edge to edge: a user script adds
  `viewport-fit=cover`, which the web phone layout already answers with
  `env(safe-area-inset-*)`. iPad keeps automatic insets because the desktop
  layout does not use safe-area padding.
- Pull to refresh, back/forward swipes, a 2 pt Signal load bar. Main-frame
  navigation to any other host, `target=_blank`, and non-web schemes open
  outside the app. Page loads time out after 12 s into an Instrument
  ErrorState ("Can't reach Omni", Try again, Settings, Details), and a failed
  page retries by itself whenever the app becomes active.
- Settings: a slim pill on the trailing edge at 40 % height (clear of the web
  top bar and tab bar on both devices) or a two-finger long press. The sheet
  opens automatically when no token is configured.

## Routing

`omni://open?u=<url>&a=<app url>` (`OmniDeepLink`) is the widget URL for
single-target families. Values are escaped with the strict RFC 3986 unreserved
set so embedded `&`, `?` and `#` survive. `OmniRoute.decide` loads URLs on the
Omni host in the web view; anything else goes to
`UIApplication.open`, trying the app-scheme URL first (`twitch://...`) and the
https universal link if that fails, and an in-app Safari sheet if iOS refuses
both. Only `twitch`, `youtube`, `vnd.youtube` and `kick` app schemes are accepted
in `a=`, so a link can never dial, text or loop back into `omni://`. A
notification tap routes `omni.url` the same way. Routes that arrive before the
scene is active (cold launch) are queued and replayed on activation.

Medium and larger widget rows are `Button(intent: OpenStreamIntent)`, whose
`perform()` returns `OpenURLIntent(watchUrl)`. That is the path that can open the
Kick, Twitch or YouTube app through its universal link without launching Omni.
It always uses the https watch URL, because an extension cannot check whether a
custom scheme has a handler.

## Push

- iPhone asks for alert, sound and badge permission on first activation; iPad
  only when its alert toggle is turned on. Every device registers for remote
  notifications.
- `PUT /device` sends `alertsEnabled = toggle && permission granted`. A device
  that would silently drop alerts must not stop Omni's Pushover fallback.
  `apnsToken` is an explicit `null` until APNs answers. The last accepted body
  is kept in the App Group; the app re-sends on change and once a day, with
  1/2/4 s backoff (no retry on a 4xx other than 408/429). The device name is
  cut to 200 UTF-16 units on a character boundary, the server's rule. The
  widget push token is registered by the extension's `WidgetPushHandler` and,
  on the same change-or-daily cadence, by the app from
  `WidgetCenter.currentPushInfo` on every activation. Debug builds report
  `sandbox`, Release builds `production`.
- Categories are registered once by the app: `OMNI_LINK` ("Open"),
  `OMNI_LINK_KICK`, `OMNI_LINK_TWITCH`, `OMNI_LINK_YOUTUBE` (exact server
  `urlTitle`s) and `OMNI_PLAIN`. The service extension only picks among them,
  since categories registered at delivery time are not guaranteed to apply.
- With `omni.sender`, the service extension downloads the avatar (https only,
  8 s overall deadline, 2 MB cap enforced while chunks arrive, cached once as
  a 288 px source in the App Group), donates an incoming
  `INSendMessageIntent` with `conversationIdentifier = sender.id`, and
  updates the content from it. The system then titles the notification with
  the sender's name, so the server title ("Destiny is LIVE!") moves into the
  subtitle (`CommunicationText`), or in front of the body if a subtitle exists. Any
  failure, or the expiry callback, delivers the original content. The handler
  runs exactly once.

## Widgets

**On Air** (`com.micthiesen.OmniLive.on-air`): small, medium, large, extra
large, circular, rectangular, inline.

- Timeline: one entry; On Air reloads after 15 min while anyone is live, else
  30 min. Widget pushes and `omni.refreshWidgets` reload sooner. Uptime is
  `Text(.timer)`, so it ticks without reloads. Relevance is 1 with a primary
  streamer live.
- The last good payload is cached in the App Group; a failed fetch shows it with
  a warn diamond and "as of 14:05" once it is older than 15 min,
  and drops it past 3 h so old live data never looks current.
  With no copy at all the widget says what is wrong ("Connect Omni",
  "Omni is out of reach"). It is never blank.
- Avatars: only as many as the family draws (4, 3, 6, 8, 1), three
  downloads at a time. Each URL is downloaded once and cached as a 288 px
  source; every family derives its 3x-point-size thumbnail from that file. The ~30 MB extension budget
  holds well under 2 MB of decoded images.

### Visual language (Instrument on iOS)

- Graphite container (`#111316` to `#0a0b0d`), text `#eceef1` / `#a9b0ba` /
  `#858d98`, rows and hairlines rather than boxes. Geist for text and Geist Mono
  for every number (`monospacedDigit`). Nothing below 11 pt.
- Live crimson only for live state, always with a shape: the hero's ring, the
  dot, and the word LIVE. List rows sit under an "On air" header and drop the
  ring, so six live rows do not become a wall of red. Signal lime marks only the
  intelligence headline (XL) and the app's primary button.
- Light comes from live things: a static radial glow behind the hero, its color
  being the avatar's average color pulled 40 % toward crimson, computed in the
  provider. No blur.
- Platform color fills only the small platform glyph (YouTube `#ff0033`, Twitch
  `#a970ff`, Kick `#53fc18`). Missing avatars become monograms on a muted
  gradient from the id hash, with a faint tier edge.
- Accented (tinted) Home Screen: the system removes the container; avatars use
  `.desaturated`, and the ring, dot, LIVE and journey glyphs are
  `widgetAccentable`. Lock Screen families are monochrome text with an
  `AccessoryWidgetBackground` ring.

## Fonts

`ios/OmniLive/Resources/Fonts` holds Geist and Geist Mono (SIL OFL 1.1, `OFL.txt`)
as TTF. They come from the web's vendored woff2 files via
`scripts/ios-live/convert-fonts.sh`: CoreText decodes the woff2 on the Mac and
`woff2ttf.swift` writes the same tables back out as an sfnt, with no glyph
changes. The output is byte-for-byte reproducible. The app and widget extension
list them in `UIAppFonts`; snapshot tests register them at runtime. If either
font fails to load, SF Pro and SF Mono take over.

## Verification

```bash
scripts/ios-live/typecheck.sh   # all three targets vs the iOS SDK, Mac static tests,
                                # bash/Swift HMAC agreement, plist lint
scripts/ios-live/test.sh        # unit + snapshot tests on iPhone 17 Pro and iPad Pro 13"
```

Snapshots land in `/tmp/omni-widget-snapshots` (one PNG per family, state and
mode, plus `sheet-on-air.png`). Accented and Lock
Screen renders there are approximations, made by desaturating and tinting.

In Debug builds, `simctl launch <device> com.micthiesen.OmniLive -OmniOpenURL
'omni://open?u=...'` exercises deep-link routing without SpringBoard's
confirmation dialog.

These only work on a signed device: APNs device and widget push tokens, alert
delivery, communication-notification avatars (needs the
`com.apple.developer.usernotifications.communication` capability on the App ID),
`OpenURLIntent` opening the platform app directly from a widget row, and real
accented and vibrant rendering.
