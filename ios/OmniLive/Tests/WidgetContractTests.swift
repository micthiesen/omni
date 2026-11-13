import XCTest
@testable import OmniLive

final class WidgetPayloadDecodingTests: XCTestCase {
    func testDecodesOnAirContract() throws {
        let data = Data(#"""
        {
          "generatedAt": 1760000000000,
          "liveCount": 5,
          "totalViewers": 23456,
          "live": [
            {
              "id": "destiny", "name": "Destiny", "tier": "primary", "platform": "kick",
              "title": "stream title", "category": "Just Chatting", "viewers": 12345,
              "peakViewers": 13400, "startedAt": 1759990000000,
              "avatarUrl": "https://files.kick.com/a.png", "watchUrl": "https://kick.com/destiny",
              "appUrl": null, "headline": "short intelligence headline"
            },
            {
              "id": "vinesauce", "name": "Vinesauce", "tier": "background", "platform": "twitch",
              "title": null, "category": null, "viewers": null, "peakViewers": null,
              "startedAt": null, "avatarUrl": null, "watchUrl": "https://www.twitch.tv/vinesauce",
              "appUrl": "twitch://stream/vinesauce", "headline": null
            }
          ],
          "recent": [
            { "id": "hasan", "name": "HasanAbi", "platform": "twitch", "endedAt": 1759000000000,
              "avatarUrl": null, "watchUrl": "https://www.twitch.tv/hasanabi" }
          ]
        }
        """#.utf8)
        let snapshot = try JSONDecoder().decode(OnAirSnapshot.self, from: data)
        XCTAssertEqual(snapshot.liveCount, 5)
        XCTAssertEqual(snapshot.totalViewers, 23_456)
        XCTAssertEqual(snapshot.unlistedLiveCount, 3)
        XCTAssertTrue(snapshot.hasPrimaryLive)
        let destiny = snapshot.live[0]
        XCTAssertEqual(destiny.platform, .kick)
        XCTAssertEqual(destiny.tier, .primary)
        XCTAssertEqual(destiny.displayViewers, 12_345)
        XCTAssertEqual(destiny.startedDate, Date(timeIntervalSince1970: 1_759_990_000))
        XCTAssertNil(destiny.appUrl)
        XCTAssertEqual(snapshot.live[1].appUrl?.absoluteString, "twitch://stream/vinesauce")
        XCTAssertEqual(snapshot.live[1].tier, .background)
        XCTAssertNil(snapshot.live[1].displayViewers)
        XCTAssertEqual(snapshot.recent.first?.endedDate, Date(timeIntervalSince1970: 1_759_000_000))
    }

    func testOnAirToleratesUnknownEnumsAndBadOptionalURLs() throws {
        let data = Data(#"""
        {"generatedAt": 1, "liveCount": 1, "totalViewers": 0, "live": [
          {"id": "x", "name": "X", "tier": "vip", "platform": "dlive", "watchUrl": "https://x.tv/x",
           "avatarUrl": "not a url", "appUrl": ""}
        ], "recent": []}
        """#.utf8)
        let snapshot = try JSONDecoder().decode(OnAirSnapshot.self, from: data)
        XCTAssertEqual(snapshot.live[0].platform, .other)
        XCTAssertEqual(snapshot.live[0].tier, .background)
        XCTAssertNil(snapshot.live[0].avatarUrl)
        XCTAssertNil(snapshot.live[0].appUrl)
    }

    func testOnAirRoundTripsThroughTheCache() throws {
        let snapshot = WidgetFixtures.liveMany(Date(timeIntervalSince1970: 1_760_000_000))
        let decoded = try JSONDecoder().decode(OnAirSnapshot.self, from: JSONEncoder().encode(snapshot))
        XCTAssertEqual(decoded, snapshot)
    }
}

final class AlertPayloadTests: XCTestCase {
    func testParsesOmniDictionary() {
        let userInfo: [AnyHashable: Any] = [
            "aps": ["alert": ["title": "Destiny is LIVE!", "body": "stream title"], "mutable-content": 1],
            "omni": [
                "channel": "live",
                "url": "https://kick.com/destiny",
                "urlTitle": "Watch on Kick",
                "sender": ["id": "destiny", "name": "Destiny", "avatarUrl": "https://files.kick.com/a.png"],
                "refreshWidgets": ["on-air", "on-air", "mystery"],
            ],
        ]
        let payload = OmniPushPayload.parse(userInfo: userInfo)
        XCTAssertEqual(payload?.channel, "live")
        XCTAssertEqual(payload?.targetURL?.absoluteString, "https://kick.com/destiny")
        XCTAssertEqual(payload?.sender?.id, "destiny")
        XCTAssertEqual(payload?.sender?.avatarURL?.host(), "files.kick.com")
        XCTAssertEqual(payload?.widgetKindsToReload, [OmniWidgetKind.onAir])
    }

    func testPlainPayloadWithNulls() {
        let userInfo: [AnyHashable: Any] = [
            "omni": ["channel": "general", "url": NSNull(), "urlTitle": NSNull(), "sender": NSNull(),
                     "refreshWidgets": [String]()],
        ]
        let payload = OmniPushPayload.parse(userInfo: userInfo)
        XCTAssertNotNil(payload)
        XCTAssertNil(payload?.targetURL)
        XCTAssertNil(payload?.sender)
        XCTAssertEqual(payload?.widgetKindsToReload, [])
        XCTAssertEqual(OmniNotificationCategory.category(for: payload, current: "OMNI_LINK"), "OMNI_PLAIN")
    }

    func testMissingOrMalformedOmniIsNil() {
        XCTAssertNil(OmniPushPayload.parse(userInfo: ["aps": ["alert": "x"]]))
        XCTAssertNil(OmniPushPayload.parse(userInfo: ["omni": "nope"]))
        XCTAssertNil(OmniPushPayload.parse(userInfo: ["omni": ["sender": ["id": 3]]]))
    }

    func testRejectsNonWebTargetsAndInsecureAvatars() {
        let payload = OmniPushPayload.parse(userInfo: [
            "omni": ["url": "javascript:alert(1)",
                     "sender": ["id": "a", "name": "A", "avatarUrl": "http://insecure.example/a.png"]],
        ])
        XCTAssertNil(payload?.targetURL)
        XCTAssertNil(payload?.sender?.avatarURL)
    }

    func testCategoryChoosesTitledVariantOnlyOnExactMatch() {
        func payload(_ title: String?) -> OmniPushPayload {
            OmniPushPayload(channel: "live", url: "https://kick.com/x", urlTitle: title, sender: nil,
                            refreshWidgets: nil)
        }
        XCTAssertEqual(OmniNotificationCategory.category(for: payload("Watch on Kick"), current: ""), "OMNI_LINK_KICK")
        XCTAssertEqual(OmniNotificationCategory.category(for: payload("Watch on Twitch"), current: ""), "OMNI_LINK_TWITCH")
        XCTAssertEqual(OmniNotificationCategory.category(for: payload("Open recipe"), current: ""), "OMNI_LINK")
        XCTAssertEqual(OmniNotificationCategory.category(for: payload(nil), current: ""), "OMNI_LINK")
        XCTAssertEqual(OmniNotificationCategory.category(for: nil, current: "OMNI_LINK"), "OMNI_LINK")
        XCTAssertTrue(OmniNotificationCategory.isLink("OMNI_LINK_YOUTUBE"))
        XCTAssertEqual(OmniNotificationCategory.allIdentifiers.count, 5)
    }
}

final class DeepLinkTests: XCTestCase {
    func testRoundTripsURLsWithReservedCharacters() throws {
        let target = URL(string: "https://www.youtube.com/watch?v=abc&t=10s#x")!
        let app = URL(string: "twitch://stream/vinesauce?x=1&y=2")!
        let link = OmniDeepLink.openURL(target: target, appURL: app)
        XCTAssertEqual(link.scheme, "omni")
        XCTAssertFalse(link.absoluteString.contains("&t="), "inner & must be escaped")
        let parsed = try XCTUnwrap(OmniDeepLink.parse(link))
        XCTAssertEqual(parsed.target, target)
        XCTAssertEqual(parsed.appURL, app)
    }

    func testRejectsForeignOrUnsafeLinks() {
        XCTAssertNil(OmniDeepLink.parse(URL(string: "https://open?u=https%3A%2F%2Fkick.com")!))
        XCTAssertNil(OmniDeepLink.parse(URL(string: "omni://other?u=https%3A%2F%2Fkick.com")!))
        XCTAssertNil(OmniDeepLink.parse(URL(string: "omni://open?u=javascript%3Aalert(1)")!))
        XCTAssertNil(OmniDeepLink.parse(URL(string: "omni://open")!))
    }

    func testOmniPagesLoadInTheWebViewAndStreamsOpenOutside() {
        let server = URL(string: "http://omni.boris")!
        XCTAssertEqual(
            OmniRoute.decide(target: URL(string: "http://omni.boris/live")!, appURL: nil, server: server),
            .webView(URL(string: "http://omni.boris/live")!)
        )
        let kick = URL(string: "https://kick.com/destiny")!
        XCTAssertEqual(OmniRoute.decide(target: kick, appURL: nil, server: server), .external([kick]))
        let twitch = URL(string: "https://www.twitch.tv/vinesauce")!
        let app = URL(string: "twitch://stream/vinesauce")!
        XCTAssertEqual(OmniRoute.decide(target: twitch, appURL: app, server: server), .external([app, twitch]))
    }
}

final class SigningAndRegistrationTests: XCTestCase {
    /// Same vector as `scripts/ios-live/doctor.sh`'s openssl signer (see typecheck.sh).
    func testHMACMatchesOpenSSLVector() {
        let signature = OmniSignature.sign(
            token: "test-token-0123456789abcdef",
            timestamp: "1760000000",
            nonce: "00000000-0000-4000-8000-000000000000",
            method: "PUT",
            path: "/api/ios-controls/device",
            body: Data(#"{"a":1}"#.utf8)
        )
        XCTAssertEqual(signature, "6ae8408129ccfd67b6744008c0f0f6274adf8507f33d67b4a6413e70cdb0600e")
    }

    func testAuthenticateSetsHeaders() {
        var request = URLRequest(url: URL(string: "http://omni.boris/api/ios-controls/diagnostics")!)
        OmniSignature.authenticate(
            &request, body: Data(), token: "test-token-0123456789abcdef",
            now: Date(timeIntervalSince1970: 1_760_000_000), nonce: "n"
        )
        XCTAssertEqual(
            request.value(forHTTPHeaderField: "Authorization"),
            "Omni-HMAC b83ceac7cedcffc93dd6354cb9ae1ec55cc1d57c578d19ba3bb3ed4922ae8983"
        )
        XCTAssertEqual(request.value(forHTTPHeaderField: "X-Omni-Timestamp"), "1760000000")
        XCTAssertEqual(request.value(forHTTPHeaderField: "X-Omni-Nonce"), "n")
    }

    func testDeviceRegistrationBodyMatchesContract() throws {
        let registration = DeviceRegistration(
            deviceId: "6f1c2a10-0000-4000-8000-000000000001", name: "iPhone", kind: .iphone,
            apnsToken: nil, environment: "sandbox", alertsEnabled: true, appVersion: "1.0 (1)"
        )
        let data = try JSONEncoder().encode(registration)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(Set(object.keys), ["deviceId", "name", "kind", "apnsToken", "environment",
                                          "alertsEnabled", "appVersion"])
        XCTAssertTrue(object["apnsToken"] is NSNull, "a missing token is an explicit null")
        XCTAssertEqual(object["kind"] as? String, "iphone")
        XCTAssertEqual(object["alertsEnabled"] as? Bool, true)

        let withToken = DeviceRegistration(
            deviceId: "d", name: "iPad", kind: .ipad, apnsToken: "ab12", environment: "production",
            alertsEnabled: false, appVersion: "1.0 (1)"
        )
        let tokenObject = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(withToken)) as? [String: Any]
        )
        XCTAssertEqual(tokenObject["apnsToken"] as? String, "ab12")
        XCTAssertEqual(tokenObject["kind"] as? String, "ipad")
        XCTAssertEqual(try JSONDecoder().decode(DeviceRegistration.self, from: JSONEncoder().encode(withToken)), withToken)
    }

    func testAlertsDefaultByDeviceKind() {
        XCTAssertTrue(DeviceKind.iphone.alertsEnabledByDefault)
        XCTAssertFalse(DeviceKind.ipad.alertsEnabledByDefault)
    }

    func testAppVersionAndResponse() throws {
        XCTAssertEqual(
            DeviceRegistration.appVersion(info: ["CFBundleShortVersionString": "1.2", "CFBundleVersion": "7"]),
            "1.2 (7)"
        )
        let response = try JSONDecoder().decode(
            DeviceRegistrationResponse.self,
            from: Data(#"{"registered":true,"alertsEnabled":false,"apnsEnabled":true}"#.utf8)
        )
        XCTAssertEqual(response, DeviceRegistrationResponse(registered: true, alertsEnabled: false, apnsEnabled: true))
    }

    func testResyncsOnChangeOrAfterADay() {
        let now = Date(timeIntervalSince1970: 1_760_000_000)
        let a = DeviceRegistration(deviceId: "d", name: "n", kind: .iphone, apnsToken: "aa",
                                   environment: "sandbox", alertsEnabled: true, appVersion: "1")
        let b = DeviceRegistration(deviceId: "d", name: "n", kind: .iphone, apnsToken: "bb",
                                   environment: "sandbox", alertsEnabled: true, appVersion: "1")
        XCTAssertTrue(DeviceRegistration.needsSync(current: a, lastSent: nil, lastSentAt: nil, now: now))
        XCTAssertFalse(DeviceRegistration.needsSync(current: a, lastSent: a, lastSentAt: now.addingTimeInterval(-60), now: now))
        XCTAssertTrue(DeviceRegistration.needsSync(current: b, lastSent: a, lastSentAt: now.addingTimeInterval(-60), now: now))
        XCTAssertTrue(DeviceRegistration.needsSync(current: a, lastSent: a, lastSentAt: now.addingTimeInterval(-90_000), now: now))
    }

    func testWidgetPushBody() throws {
        let body = WidgetPushRegistration(deviceId: "d", pushToken: "abcd", environment: "sandbox")
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(body)) as? [String: String])
        XCTAssertEqual(object, ["deviceId": "d", "pushToken": "abcd", "environment": "sandbox"])
    }
}

final class WidgetPolicyTests: XCTestCase {
    func testOnAirRefreshesFasterWhileLive() {
        XCTAssertEqual(OnAirPolicy.refreshInterval(for: WidgetFixtures.liveMany()), 15 * 60)
        XCTAssertEqual(OnAirPolicy.refreshInterval(for: WidgetFixtures.nobody()), 30 * 60)
        XCTAssertEqual(OnAirPolicy.refreshInterval(for: nil), 30 * 60)
    }

    func testStaleAfterFifteenMinutes() {
        let now = Date(timeIntervalSince1970: 1_760_000_000)
        XCTAssertFalse(OnAirPolicy.isStale(fetchedAt: now.addingTimeInterval(-14 * 60), now: now))
        XCTAssertTrue(OnAirPolicy.isStale(fetchedAt: now.addingTimeInterval(-16 * 60), now: now))
        XCTAssertFalse(OnAirPolicy.isStale(fetchedAt: nil, now: now))
    }

    func testRelevanceBoostsPrimaryLive() {
        XCTAssertEqual(OnAirPolicy.relevanceScore(for: WidgetFixtures.liveMany()), 1)
        XCTAssertEqual(OnAirPolicy.relevanceScore(for: WidgetFixtures.nobody()), 0)
        let backgroundOnly = OnAirSnapshot(
            generatedAt: 0, liveCount: 1, totalViewers: 0,
            live: [WidgetFixtures.streamer("b", "B", platform: .kick, tier: .background, viewers: 1,
                                           minutesLive: 1, title: nil)],
            recent: []
        )
        XCTAssertLessThan(OnAirPolicy.relevanceScore(for: backgroundOnly), 1)
    }

    func testAvatarBudgetBoundsDownloads() {
        XCTAssertEqual(OnAirPolicy.avatarBudget(familyRawName: "accessoryInline"), 0)
        XCTAssertEqual(OnAirPolicy.avatarBudget(familyRawName: "systemExtraLarge"), 8)
    }

    func testCompactNumbers() {
        let locale = Locale(identifier: "en_US")
        XCTAssertEqual(OmniFormat.compact(999, locale: locale), "999")
        XCTAssertEqual(OmniFormat.compact(1_234, locale: locale), "1.2K")
        XCTAssertEqual(OmniFormat.compact(12_345, locale: locale), "12.3K")
        XCTAssertEqual(OmniFormat.compact(123_456, locale: locale), "123K")
        XCTAssertEqual(OmniFormat.compact(1_234_567, locale: locale), "1.2M")
    }

    func testAgo() {
        let now = Date(timeIntervalSince1970: 1_760_000_000)
        XCTAssertEqual(OmniFormat.ago(now.addingTimeInterval(-30), now: now), "now")
        XCTAssertEqual(OmniFormat.ago(now.addingTimeInterval(-12 * 60), now: now), "12m ago")
        XCTAssertEqual(OmniFormat.ago(now.addingTimeInterval(-3 * 3_600), now: now), "3h ago")
        XCTAssertEqual(OmniFormat.ago(now.addingTimeInterval(-50 * 3_600), now: now), "2d ago")
    }
}

final class ReviewFixTests: XCTestCase {
    func testCommunicationTitleMovesIntoSubtitle() {
        let lines = CommunicationText.lines(
            title: "Destiny is LIVE!", subtitle: "", body: "DEBATE NIGHT", senderName: "Destiny"
        )
        XCTAssertEqual(lines, .init(subtitle: "Destiny is LIVE!", body: "DEBATE NIGHT"))
    }

    func testCommunicationTitleJoinsBodyWhenSubtitleTaken() {
        let lines = CommunicationText.lines(
            title: "Destiny changed title", subtitle: "Kick", body: "new title", senderName: "Destiny"
        )
        XCTAssertEqual(lines, .init(subtitle: "Kick", body: "Destiny changed title\nnew title"))
        let empty = CommunicationText.lines(title: "Destiny is LIVE!", subtitle: "Kick", body: "", senderName: "Destiny")
        XCTAssertEqual(empty.body, "Destiny is LIVE!")
    }

    func testCommunicationTitleEqualToSenderIsDropped() {
        let lines = CommunicationText.lines(title: "Destiny", subtitle: "", body: "hi", senderName: "destiny")
        XCTAssertEqual(lines, .init(subtitle: "", body: "hi"))
    }

    func testRetryOnlyTransientFailures() {
        XCTAssertFalse(OmniAPIError.isRetryable(OmniAPIError.http(400, "")))
        XCTAssertFalse(OmniAPIError.isRetryable(OmniAPIError.http(401, "")))
        XCTAssertFalse(OmniAPIError.isRetryable(OmniAPIError.http(409, "")))
        XCTAssertTrue(OmniAPIError.isRetryable(OmniAPIError.http(408, "")))
        XCTAssertTrue(OmniAPIError.isRetryable(OmniAPIError.http(429, "")))
        XCTAssertTrue(OmniAPIError.isRetryable(OmniAPIError.http(503, "")))
        XCTAssertTrue(OmniAPIError.isRetryable(URLError(.timedOut)))
        XCTAssertFalse(OmniAPIError.isRetryable(OmniAPIError.notConfigured))
    }

    func testDeviceNameIsCappedByUTF16Units() {
        let emoji = String(repeating: "😀", count: 150) // 300 UTF-16 units
        let registration = DeviceRegistration(deviceId: "d", name: emoji, kind: .iphone, apnsToken: nil,
                                              environment: "sandbox", alertsEnabled: true, appVersion: "1")
        XCTAssertEqual(registration.name.utf16.count, 200)
        XCTAssertEqual(registration.name.count, 100, "no surrogate pair is split")
        XCTAssertEqual(DeviceRegistration.truncated("short", utf16Limit: 200), "short")
    }

    func testWidgetPushResendsOnChangeOrDaily() {
        let now = Date(timeIntervalSince1970: 1_760_000_000)
        let a = WidgetPushRegistration(deviceId: "d", pushToken: "aa", environment: "sandbox")
        let b = WidgetPushRegistration(deviceId: "d", pushToken: "bb", environment: "sandbox")
        XCTAssertFalse(RegistrationCadence.needsSync(current: a, lastSent: a, lastSentAt: now.addingTimeInterval(-3_600), now: now))
        XCTAssertTrue(RegistrationCadence.needsSync(current: a, lastSent: a, lastSentAt: now.addingTimeInterval(-90_000), now: now))
        XCTAssertTrue(RegistrationCadence.needsSync(current: b, lastSent: a, lastSentAt: now, now: now))
        XCTAssertTrue(RegistrationCadence.needsSync(current: a, lastSent: nil, lastSentAt: nil, now: now))
    }

    func testAppSchemesAreAllowlisted() {
        let target = URL(string: "https://www.twitch.tv/x")!
        for (raw, allowed) in [("twitch://stream/x", true), ("youtube://watch?v=1", true), ("kick://x", true),
                               ("tel:5551234", false), ("sms:5551234", false), ("omni://open?u=x", false),
                               ("javascript:alert(1)", false), ("https://evil.example", false)]
        {
            let link = OmniDeepLink.openURL(target: target, appURL: URL(string: raw)!)
            XCTAssertEqual(OmniDeepLink.parse(link)?.appURL != nil, allowed, raw)
            XCTAssertEqual(OmniDeepLink.parse(link)?.target, target)
        }
    }

    func testCachedSnapshotsExpire() {
        let now = Date(timeIntervalSince1970: 1_760_000_000)
        let fresh = CachedSnapshot(value: 1, fetchedAt: now.addingTimeInterval(-2 * 3_600))
        let old = CachedSnapshot(value: 1, fetchedAt: now.addingTimeInterval(-4 * 3_600))
        XCTAssertNotNil(SnapshotStore.usable(fresh, maxAge: OnAirPolicy.hardExpiry, now: now))
        XCTAssertNil(SnapshotStore.usable(old, maxAge: OnAirPolicy.hardExpiry, now: now))
        XCTAssertNotNil(SnapshotStore.usable(old, maxAge: 5 * 3_600, now: now))
        XCTAssertNil(SnapshotStore.usable(Optional<CachedSnapshot<Int>>.none, maxAge: 1, now: now))
    }

    func testAPNsEnvironmentMapping() {
        XCTAssertEqual(OmniSettings.apnsEnvironment(entitlement: "production"), "production")
        XCTAssertEqual(OmniSettings.apnsEnvironment(entitlement: "development"), "sandbox")
        XCTAssertEqual(OmniSettings.apnsEnvironment(entitlement: nil), "sandbox")
    }

    func testCappedDownloadThumbnailsAreDerivedFromOneSource() throws {
        let image = try XCTUnwrap(WidgetFixtures.avatars(for: WidgetFixtures.liveOne())["destiny"]).image
        let png = try XCTUnwrap(AvatarStore.pngData(image))
        let source = try XCTUnwrap(AvatarStore.thumbnailPNG(png, maxPixels: AvatarStore.sourcePixels))
        XCTAssertEqual(AvatarStore.decode(source, maxPixels: 120)?.image.width, 120)
        XCTAssertLessThanOrEqual(AvatarStore.decode(source, maxPixels: 288)?.image.width ?? 0, 288)
    }
}
