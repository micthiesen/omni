import CryptoKit
import Foundation

enum OmniAPIError: LocalizedError, Equatable {
    case notConfigured
    case invalidResponse
    case http(Int, String)

    var errorDescription: String? {
        switch self {
        case .notConfigured: "Enter the Omni server URL and control token first."
        case .invalidResponse: "Omni returned an invalid response."
        case let .http(status, message): "Omni returned HTTP \(status): \(message)"
        }
    }

    /// Whether repeating the same request could succeed. A 4xx other than 408/429 means
    /// the request itself is wrong (auth, validation, limits) and only a change fixes it.
    static func isRetryable(_ error: Error) -> Bool {
        switch error {
        case OmniAPIError.notConfigured: false
        case let OmniAPIError.http(status, _): !(400 ..< 500 ~= status) || status == 408 || status == 429
        default: true
        }
    }
}

/// The Omni-HMAC request signature shared by every `/api/ios-controls` route.
///
/// Canonical string: `ts \n nonce \n METHOD \n path \n sha256hex(body)`, HMAC-SHA256
/// keyed with the control token, sent as `Authorization: Omni-HMAC <hex>` with
/// `X-Omni-Timestamp` and `X-Omni-Nonce`. `scripts/ios-live/doctor.sh` signs the same
/// way in bash; the static tests pin both to one vector.
enum OmniSignature {
    static func sign(
        token: String,
        timestamp: String,
        nonce: String,
        method: String,
        path: String,
        body: Data
    ) -> String {
        let bodyHash = SHA256.hash(data: body).map { String(format: "%02x", $0) }.joined()
        let canonical = "\(timestamp)\n\(nonce)\n\(method)\n\(path)\n\(bodyHash)"
        let key = SymmetricKey(data: Data(token.utf8))
        return HMAC<SHA256>.authenticationCode(for: Data(canonical.utf8), using: key)
            .map { String(format: "%02x", $0) }
            .joined()
    }

    static func authenticate(
        _ request: inout URLRequest,
        body: Data,
        token: String,
        now: Date = Date(),
        nonce: String = UUID().uuidString.lowercased()
    ) {
        let timestamp = String(Int(now.timeIntervalSince1970))
        let path = request.url?.path(percentEncoded: true) ?? "/"
        let signature = sign(
            token: token,
            timestamp: timestamp,
            nonce: nonce,
            method: request.httpMethod ?? "GET",
            path: path,
            body: body
        )
        request.setValue("Omni-HMAC \(signature)", forHTTPHeaderField: "Authorization")
        request.setValue(timestamp, forHTTPHeaderField: "X-Omni-Timestamp")
        request.setValue(nonce, forHTTPHeaderField: "X-Omni-Nonce")
    }
}

struct OmniAPIClient: Sendable {
    private let session: URLSession

    init(session: URLSession = .shared) {
        self.session = session
    }

    func liveSlot(_ slot: Int) async throws -> LiveSlotState {
        let state: LiveSlotState = try await get("api/ios-controls/slots/\(slot)")
        OmniSettings.shared.cache(state)
        return state
    }

    func register(controls: [ControlRegistration]) async throws {
        let settings = OmniSettings.shared
        let body = try JSONEncoder().encode(
            ControlRegistrationRequest(deviceId: settings.deviceId, controls: controls)
        )
        // Server registration may make two bounded APNs attempts (5s each)
        // before recording a later-tick retry.
        _ = try await send("PUT", "api/ios-controls/registrations", body: body, timeout: 15)
        settings.lastRegistrationAt = Date()
        settings.lastRegistrationCount = controls.count
    }

    func diagnostics() async throws -> ControlServerDiagnostics {
        try await get("api/ios-controls/diagnostics")
    }

    func registerDevice(_ registration: DeviceRegistration) async throws -> DeviceRegistrationResponse {
        let data = try await send(
            "PUT",
            "api/ios-controls/device",
            body: JSONEncoder().encode(registration),
            timeout: 10
        )
        return try JSONDecoder().decode(DeviceRegistrationResponse.self, from: data)
    }

    func registerWidgetPush(_ registration: WidgetPushRegistration) async throws {
        _ = try await send(
            "PUT",
            "api/ios-controls/widget-push",
            body: JSONEncoder().encode(registration),
            timeout: 10
        )
    }

    func onAir() async throws -> OnAirSnapshot {
        try await get("api/ios-controls/widgets/on-air")
    }

    private func get<T: Decodable>(_ path: String) async throws -> T {
        let data = try await send("GET", path, body: nil, timeout: 8)
        return try JSONDecoder().decode(T.self, from: data)
    }

    private func send(
        _ method: String,
        _ path: String,
        body: Data?,
        timeout: TimeInterval
    ) async throws -> Data {
        let settings = OmniSettings.shared
        guard let baseURL = settings.serverURL, !settings.authToken.isEmpty else {
            throw OmniAPIError.notConfigured
        }
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = method
        request.timeoutInterval = timeout
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = body
        }
        OmniSignature.authenticate(&request, body: body ?? Data(), token: settings.authToken)
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw OmniAPIError.invalidResponse
        }
        guard 200 ..< 300 ~= http.statusCode else {
            let message = String(data: data.prefix(512), encoding: .utf8) ?? "Unknown error"
            throw OmniAPIError.http(http.statusCode, message)
        }
        return data
    }
}
