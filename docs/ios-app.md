# Omni iOS app

Omni is a private iOS/iPadOS app (bundle `com.micthiesen.OmniLive`, display
name "Omni") for Michael's own devices. It has four parts:

- **Notifications.** Every Omni notification is delivered to the app over
  APNs. Pushover is only the fallback for when the app is not set up.
- **Widgets.** "On Air" (top live streamers; tapping a row opens that stream in
  the Kick, Twitch or YouTube app) in every home-screen size and the lock-screen
  accessory families, and "Deliveries" (parcels in transit).
- **Control Center.** One configurable control, added four times as Live
  Slots 1 to 4.
- **The app itself.** A full-screen web view of `http://omni.boris` plus a
  native settings sheet: server URL, token, this device's alerts toggle,
  diagnostics and the live slot controls setup.

The app and its extensions always reach Omni at `http://omni.boris`, on the
LAN or over WireGuard. APNs pushes arrive anywhere; anything that has to fetch
fresh data waits until `omni.boris` is reachable again.

The server side is the `omni-ios-controls` crate. The iOS project is
`ios/OmniLive` (XcodeGen `project.yml`, generated `.xcodeproj` checked in); the
client design decisions are in `docs/ios-widgets-design.md`.

| Target | Bundle ID | Role |
| --- | --- | --- |
| `OmniLive` (product `OmniLive.app`) | `com.micthiesen.OmniLive` | Web view, settings, APNs registration, notification and `omni://` routing |
| `OmniLiveControls` (WidgetKit) | `.controls` | Live Slot control, On Air and Deliveries widgets, widget push handler |
| `OmniNotificationService` | `.notification-service` | Sender avatars, action category, widget reloads |

All three share the App Group `group.com.micthiesen.OmniLive` (settings,
pending registrations, the widgets' last-good payloads and avatar cache). The
deployment target is iOS 26.0.

## The app

The app is a full-screen web view of the configured server, dark from launch.
On iPhone it is edge to edge and portrait; iPad supports every orientation and
keeps the web desktop layout. Pull to refresh, swipe back and forward, and
links to other sites open in their app or Safari. When `omni.boris` does not
answer within 12 s the app shows "Can't reach Omni" with Try again, and it
retries on its own each time it becomes active.

Settings open from the slim pill on the right edge (about 40 % down), with a
two-finger long press anywhere, from the offline screen, or automatically when
no token is configured. The sheet holds this device's alerts toggle,
notification permission, registration state, server URL and token (**Save,
Test, and Sync**), the Live Slots preview and diagnostics.

Widget taps and notification taps that arrive before the app is active are
queued and handled once it is. `omni://open?u=<url>&a=<app url>` opens Omni
pages in the web view and anything else outside the app, trying the app URL
first (only `twitch`, `youtube`, `vnd.youtube` and `kick` schemes are accepted)
and an in-app Safari sheet if iOS refuses every candidate.

## Notifications

`omni_alerts::Pushover` offers every message to the app route before Pushover.
That covers feature notifications, per-streamer Pushover tokens (sent as the
`live` channel) and ERROR-log alerts. The route sends an `alert` push to every
registered device that has alerts enabled and an APNs token, at most four at a
time:

| Outcome | Result |
| --- | --- |
| APNs not configured, or no alert device | Pushover sends it, unchanged |
| At least one device accepted it (HTTP 200) | Delivered; Pushover is skipped |
| Every device answered with a rejection, or failed before sending | Pushover sends it (WARN `IOSAlerts`) |
| No device accepted it and at least one push got no answer | Uncertain: no Pushover, no resend |

A 429 or 5xx answer is retried once after 250 ms before it counts as a
rejection. 410, `BadDeviceToken`, `DeviceTokenNotForTopic` and `Unregistered`
clear that device's token, so later messages skip the device until the app
registers a new one. A push that fails before any bytes leave Omni (an
unusable signing key or provider token, a bad URL) certainly did not arrive
and counts as a rejection. A push with no answer after it was sent (timeout or
connection failure) may already be on the phone. It is never retried, and the caller sees an error
with no status, exactly like an uncertain Pushover send, so durable
reservations never resend it. Omni does not use acknowledgements or receipts
to decide on a fallback. The route never logs at ERROR, because ERROR-log
alerts take the same route.

The `alert` push uses push type `alert`, topic = bundle ID, priority 10, no
collapse ID, and an `apns-expiration` one hour ahead for the `live` channel
and 24 hours ahead otherwise, so APNs stores it and retries delivery while
the phone is offline:

```json
{
  "aps": {
    "alert": { "title": "Destiny is LIVE!", "body": "stream title" },
    "sound": "default",
    "thread-id": "live:destiny",
    "mutable-content": 1,
    "category": "OMNI_LINK"
  },
  "omni": {
    "channel": "live",
    "url": "https://kick.com/destiny",
    "urlTitle": "Watch on Kick",
    "sender": { "id": "destiny", "name": "Destiny", "avatarUrl": "https://..." },
    "refreshWidgets": ["on-air"]
  }
}
```

The Notification Service Extension turns a push with `sender` into a
communication notification: it downloads the avatar (https only, 8 s overall
deadline, 2 MB cap, cached once in the App Group), donates an incoming
`INSendMessageIntent` keyed by `sender.id`, and keeps the server's title
visible by moving it into the subtitle (the sender's name becomes the title).
It also reloads the widgets named in `refreshWidgets` and picks the category.
Any failure delivers the original notification unchanged.

`thread-id` is the message's `thread_id`, or the channel name. `category` is
`OMNI_LINK` when a URL is set, which gives one action button. Action titles
cannot change per notification, so the app registers `OMNI_LINK` ("Open") plus
`OMNI_LINK_KICK`, `OMNI_LINK_TWITCH` and `OMNI_LINK_YOUTUBE` for the exact
`urlTitle`s "Watch on Kick/Twitch/YouTube", and the extension picks one. Tapping the notification opens the URL. Otherwise it is
`OMNI_PLAIN`. `sender` makes it a communication notification with the
streamer's avatar, which the Notification Service Extension downloads.
`refreshWidgets` lists widget kinds to reload. The payload stays within
APNs's 4 KB limit: the body is cut first, on a character boundary with an
ellipsis, then the title. There are no critical or time-sensitive alerts.

`PushoverMessage` carries `thread_id`, `sender` and `refresh_widgets` for the
app. Pushover's form ignores them.

## Devices

`PUT /api/ios-controls/device` registers the app whenever its token, name,
alerts toggle or notification permission changes, and at least daily when the
app is opened:

```json
{ "deviceId": "uuid", "name": "Michael's iPhone", "kind": "iphone",
  "apnsToken": "hex", "environment": "sandbox", "alertsEnabled": true,
  "appVersion": "1.0 (1)" }
```

The response is `{"registered": true, "alertsEnabled": true, "apnsEnabled": bool}`.
Registration is an idempotent replace keyed by `deviceId` (entity `ios-device`).
A `null` token keeps the device but makes it un-pushable. The app sends
`alertsEnabled: true` only when the toggle is on *and* iOS permission is
granted, so a device that would hide alerts never blocks the Pushover
fallback. Debug builds report `sandbox`, Release builds `production`. Failed
registrations retry with backoff, except for a 4xx other than 408 or 429. The iPhone receives
alerts. The iPad registers with alerts off by default and uses widgets only. At
most 32 devices are stored; a new device beyond that gets 409.

## Widgets

`GET /api/ios-controls/widgets/on-air` returns the live streamers ranked like
the live slots (primary tier first, then current viewers). Up to 8 are listed;
`liveCount` and `totalViewers` cover every live streamer, including background
tier and DGG discoveries. Each entry has the primary binding's `watchUrl`, an
`appUrl` where the https URL is not a universal link (Twitch:
`twitch://stream/<login>`), the cached avatar, and the intelligence headline
when one was produced this session. `recent` lists up to 4 most recently ended
primary streamers for the empty state.

`GET /api/ios-controls/widgets/deliveries` serves Omni's cached Parcel read
through the `DeliveriesSource` port. That port only reads the cache, so the
widget never causes a Parcel API call. It lists active deliveries first, then
up to 3 completed ones, 8 at most. Without `PARCEL_API_KEY`, it reports
`configured: false`.

On the device, each widget keeps the last good payload. While `omni.boris` is
out of reach it shows that copy with "as of HH:mm" once it is 15 minutes old
(Deliveries: 2 hours), and stops showing it after 3 hours (Deliveries: 24
hours) in favour of "Omni is out of reach". On Air reloads every 15 minutes
while anyone is live and every 30 minutes otherwise; uptime ticks on its own.
Rows in medium and larger widgets open the stream directly; small, circular
and inline widgets go through the app, which forwards at once.

`PUT /api/ios-controls/widget-push` stores the widget extension's WidgetKit
push token, one per device (entity `ios-widget-push`):
`{"deviceId", "pushToken", "environment"}`. The extension's `WidgetPushHandler`
sends it when it changes, and the app re-sends it when opened if it changed or
is a day old. After every live-check tick Omni
hashes the On Air content (the set of listed live streamers with title and
platform; never their rank or viewer counts, so close streamers trading places
do not push every tick) and sends a `widgets` push (topic
`<bundle>.push-type.widgets`, body `{"aps":{"content-changed":true}}`,
expiring after one hour) to each token whose last delivered hash differs. Failures follow the control rules
below. iOS treats widget pushes as opportunistic and budgets them.

## Control Center

Each control shows the channel occupying its slot and opens the current stream
when tapped. An empty slot opens the Omni home page. The app sends
`PUT /api/ios-controls/registrations` with its complete control set, and the
control fetches `GET /api/ios-controls/slots/:slot`. After a live-check tick
changes a slot, Omni pushes `controls` (topic `<bundle>.push-type.controls`,
priority 10, expiration 0, body `{"aps":{"content-changed":true}}`) only to the
controls showing that slot. This follows Apple's
[Updating controls locally and remotely](https://developer.apple.com/documentation/widgetkit/updating-controls-locally-and-remotely).

Network, 429 and 5xx failures of control and widget pushes are retried once
right away and then carried to later live-check ticks. A permanent rejection is
not retried for the same state until the app registers again. Invalid tokens
are deleted. APNs failures never fail the LiveCheck task.

## Authentication

Every `/api/ios-controls/*` request carries `Authorization: Omni-HMAC <hex>`,
`X-Omni-Timestamp` (seconds, within 300 s) and a single-use UUID
`X-Omni-Nonce`. The signature is HMAC-SHA256, keyed by
`IOS_CONTROL_AUTH_TOKEN`, over
`ts\nnonce\nMETHOD\n<raw path>\nsha256(body)`. Bodies are capped at 64 KiB.
The secret never crosses the LAN and a captured request cannot be replayed.
Responses are not encrypted. Without the token, every route answers 503.
`GET /api/ios-controls/diagnostics` reports APNs state, control registrations,
devices, alert devices and widget push tokens, without any tokens.

## Server setup

```dotenv
IOS_CONTROL_AUTH_TOKEN=the-same-secret-the-app-is-built-with
IOS_CONTROL_APNS_TEAM_ID=ABCDE12345
IOS_CONTROL_APNS_KEY_ID=KEYID12345
IOS_CONTROL_APNS_KEY_PATH=/data/AuthKey_KEYID.p8
IOS_CONTROL_BUNDLE_ID=com.micthiesen.OmniLive
IOS_CONTROL_HOME_URL=http://omni.boris
```

All four APNs values plus the auth token enable pushes. With only the auth
token, the API works but nothing is pushed, and Pushover keeps sending every
notification. An unreadable key disables pushes with a WARN at boot.

## Apple setup

A paid Apple Developer membership is required: APNs, widget pushes and
Control Center reloads need the Push Notifications capability. A free
Personal Team cannot provision it. The app is installed privately from the
Mac; there is no App Store or TestFlight. A development-signed install
normally remains valid for the provisioning profile's lifetime, typically up
to one year.

In **developer.apple.com/account > Certificates, Identifiers & Profiles**:

1. Confirm the app ID matches the bundle ID. Automatic signing can create it,
   and the extension IDs, on the first device build.
2. Enable the App Group `group.com.micthiesen.OmniLive` for the app and its
   extensions.
3. Enable Push Notifications for the app and the widget extension, and
   Communication Notifications for the app and the notification service
   extension (sender avatars).
4. Under **Keys**, create a key with Apple Push Notifications service access
   and download the `.p8` file. Apple offers the download only once. The same
   key works for development and production APNs.
5. Copy the key to `/home/michael/compose/volumes/omni/AuthKey_KEYID.p8`
   on boris (`/data/AuthKey_KEYID.p8` in the container). Keep it out of Git.

Each device reports its APNs environment (`sandbox` for development-signed
builds), and Omni sends to the matching APNs host.

## Building and installing

The repository owns the Xcode project source, the generated `.xcodeproj`, the
Swift app and its extensions, and the scripts in `scripts/ios-live/`. Run them
from the repository root:

```bash
scripts/ios-live/doctor.sh       # diagnose Xcode, signing, local config, devices
scripts/ios-live/configure.sh --team ABCDE12345   # private build config; prints IOS_CONTROL_AUTH_TOKEN
scripts/ios-live/typecheck.sh    # all three targets, Mac static tests, HMAC signer agreement
scripts/ios-live/build.sh        # signed device build (or: build.sh simulator)
scripts/ios-live/test.sh         # unit + widget snapshot tests on iPhone and iPad simulators
scripts/ios-live/install.sh DEVICE_IDENTIFIER
scripts/ios-live/convert-fonts.sh   # rebuild the bundled Geist TTFs after a web font update
```

The built app is `DerivedData/Build/Products/Debug-iphoneos/OmniLive.app`
(its Home Screen name is "Omni"). Widget snapshots from `test.sh` land in
`/tmp/omni-widget-snapshots`. The doctor signs its diagnostics request with
Omni-HMAC like the app; the token never appears on a command line.

`Config/Local.xcconfig`, build output, tokens and Apple keys are ignored by
Git. On first use, sign into **Xcode > Settings > Accounts** once so Xcode can
create a development certificate. If CoreSimulator is missing, install
`/Applications/Xcode.app/Contents/Resources/Packages/XcodeSystemResources.pkg`
with `sudo installer` and run `xcodebuild -runFirstLaunch`. Pair each device
by cable once (Trust This Computer, Developer Mode), then find its identifier
with `xcrun devicectl list devices`.

After installing, open Omni, confirm the server URL and token in settings,
and allow notifications on the iPhone. The iPad keeps its alerts toggle off.
Add the widgets, and add the Omni Live Stream control four times as Live
Slots 1 to 4. Diagnostics should then show the device, the four controls and
the widget push token.

## Troubleshooting

- **Notifications still arrive through Pushover.** Check diagnostics: APNs
  must be enabled and `alertDeviceCount` at least 1. Omni logs a WARN under
  `IOSAlerts` when every device rejected a push, with each device's reason. A
  `403 InvalidProviderToken` means the Team ID, Key ID or `.p8` is wrong.
- **No notification at all, and no Pushover either.** The push had an unknown
  outcome (WARN `App alert outcome unknown`). It is never resent.
- **A control or widget never changes.** Open Omni so it registers again, and
  check the `IOSControls` WARNs, the `.p8` path, the Team ID, the Key ID and
  the bundle ID.
- **HTTP 401.** The token built into the app does not match
  `IOS_CONTROL_AUTH_TOKEN`. Rerun `scripts/ios-live/configure.sh` with the
  correct token and reinstall.
- **Old data away from home.** Expected without WireGuard. Pushes arrive, but
  fetching `omni.boris` waits until it is reachable.
