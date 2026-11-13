import SwiftUI
import WidgetKit

@MainActor
final class SettingsModel: ObservableObject {
    @Published var serverURL = OmniSettings.shared.serverURLString
    @Published var authToken = OmniSettings.shared.authToken
    @Published var status = "Ready"
    @Published var slots: [LiveSlotState] = []
    @Published var diagnostics: ControlServerDiagnostics?
    @Published var isWorking = false

    var isConfigured: Bool {
        URL(string: serverURL)?.host != nil && authToken.count >= 24
    }

    func refreshIfConfigured() async {
        guard isConfigured else { return }
        await saveAndTest()
    }

    /// Returns whether the server URL changed.
    @discardableResult
    func saveAndTest() async -> Bool {
        let changed = OmniSettings.shared.serverURLString != serverURL.trimmingCharacters(in: .whitespacesAndNewlines)
            || OmniSettings.shared.authToken != authToken.trimmingCharacters(in: .whitespacesAndNewlines)
        OmniSettings.shared.serverURLString = serverURL
        OmniSettings.shared.authToken = authToken
        isWorking = true
        defer { isWorking = false }
        // Device and widget-push registration do not depend on the control checks, so
        // a failed slot fetch or diagnostics call never blocks them.
        async let device: Void = PushRegistrar.shared.sync(force: changed)
        async let widgetPush: Void = PushRegistrar.shared.syncWidgetPush()
        var problems: [String] = []
        do {
            slots = try await withThrowingTaskGroup(of: LiveSlotState.self) { group in
                for slot in 1 ... 4 {
                    group.addTask { try await OmniAPIClient().liveSlot(slot) }
                }
                var result: [LiveSlotState] = []
                for try await value in group { result.append(value) }
                return result.sorted { $0.slot < $1.slot }
            }
        } catch {
            problems.append("Slots: \(error.localizedDescription)")
        }
        var controlCount = 0
        do {
            controlCount = try await ControlRegistrationSync.syncCurrentControls()
            ControlCenter.shared.reloadAllControls()
        } catch {
            problems.append("Controls: \(error.localizedDescription)")
        }
        do {
            diagnostics = try await OmniAPIClient().diagnostics()
        } catch {
            problems.append("Diagnostics: \(error.localizedDescription)")
        }
        _ = await (device, widgetPush)
        WidgetCenter.shared.reloadAllTimelines()
        status = problems.isEmpty
            ? "Connected. Synced \(controlCount) configured control\(controlCount == 1 ? "" : "s")."
            : problems.joined(separator: "\n")
        return changed
    }
}

struct SettingsView: View {
    @ObservedObject var push: PushRegistrar
    let done: () -> Void
    let serverChanged: () -> Void

    @StateObject private var model = SettingsModel()

    var body: some View {
        NavigationStack {
            Form {
                deviceSection
                connectionSection
                widgetsSection
                liveSlotsSection
                diagnosticsSection
            }
            .scrollContentBackground(.hidden)
            .background(Omni.canvas)
            .tint(Omni.signal)
            .navigationTitle("Omni")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", action: done)
                }
            }
            .task {
                await push.refreshAuthorization()
                await model.refreshIfConfigured()
            }
        }
    }

    // MARK: Sections

    private var deviceSection: some View {
        Section {
            Toggle("Alerts on this device", isOn: Binding(
                get: { push.alertsEnabled },
                set: { push.setAlertsEnabled($0) }
            ))
            LabeledContent("Notifications") {
                switch push.authorization {
                case .notDetermined:
                    Button("Allow") { Task { await push.requestAuthorization() } }
                case .denied:
                    Button("Open Settings") {
                        if let url = URL(string: UIApplication.openSettingsURLString) {
                            UIApplication.shared.open(url)
                        }
                    }
                default:
                    Text(push.authorizationLabel).foregroundStyle(Omni.text2)
                }
            }
            LabeledContent("Registration", value: registrationLabel)
            if let error = OmniSettings.shared.deviceRegistrationError {
                Text(error)
                    .font(.footnote)
                    .foregroundStyle(Omni.warn)
            }
            Button {
                Task { await push.sync(force: true) }
            } label: {
                if push.isSyncing { ProgressView() } else { Text("Register again") }
            }
            .disabled(push.isSyncing || !model.isConfigured)
        } header: {
            Text("This device")
        } footer: {
            Text(push.kind == .ipad
                ? "iPad shows widgets only unless alerts are on. Omni falls back to Pushover when no device takes alerts."
                : "Omni sends alerts here. It falls back to Pushover only when no device can take them.")
        }
    }

    private var connectionSection: some View {
        Section("Connection") {
            TextField("Server URL", text: $model.serverURL)
                .textInputAutocapitalization(.never)
                .keyboardType(.URL)
                .autocorrectionDisabled()
            SecureField("Control Token", text: $model.authToken)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
            Button {
                Task {
                    if await model.saveAndTest() { serverChanged() }
                }
            } label: {
                if model.isWorking {
                    ProgressView()
                } else {
                    Label("Save, Test, and Sync", systemImage: "arrow.triangle.2.circlepath")
                }
            }
            .disabled(!model.isConfigured || model.isWorking)
            Text(model.status)
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }

    private var widgetsSection: some View {
        Section("Widgets and controls") {
            Text("Add On Air from the Home Screen or Lock Screen widget gallery. Tapping a stream opens it in its app.")
            Text("For Control Center, add Omni Live Stream up to four times and set each copy to Live Slot 1, 2, 3, or 4. Open this app after changing controls so their push tokens sync.")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }

    private var liveSlotsSection: some View {
        Section("Live Slots") {
            if model.slots.isEmpty {
                Text("Run the connection test to preview all four Control Center slots.")
                    .foregroundStyle(.secondary)
            } else {
                ForEach(model.slots, id: \.slot) { slot in
                    HStack(spacing: 12) {
                        Image(systemName: slot.isLive ? "play.circle.fill" : "moon.zzz")
                            .foregroundStyle(slot.isLive ? Omni.live : .secondary)
                        VStack(alignment: .leading) {
                            Text("Slot \(slot.slot): \(slot.displayName)")
                            if let title = slot.title {
                                Text(title).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
        }
    }

    private var diagnosticsSection: some View {
        let settings = OmniSettings.shared
        return Section("Diagnostics") {
            LabeledContent("Device ID", value: settings.deviceId)
            LabeledContent("Push Environment", value: settings.apnsEnvironment)
            LabeledContent("APNs Token", value: settings.apnsToken == nil ? "None yet" : "Received")
            LabeledContent(
                "Widget Push",
                value: settings.pendingWidgetPush != nil ? "Pending Retry"
                    : settings.lastSentWidgetPush != nil ? "Registered" : "Not yet"
            )
            if let error = settings.widgetPushError {
                Text("Last widget push error: \(error)")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            LabeledContent("Registered Controls", value: String(settings.lastRegistrationCount))
            if let date = settings.lastRegistrationAt {
                LabeledContent("Last Registration", value: date.formatted())
            }
            if settings.pendingRegistrations != nil {
                LabeledContent("Registration Sync", value: "Pending Retry")
            }
            if let error = settings.lastRegistrationError {
                Text("Last registration error: \(error)")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            if let diagnostics = model.diagnostics {
                LabeledContent("Server APNs", value: diagnostics.apnsEnabled ? "Enabled" : "Disabled")
                LabeledContent("Server Registrations", value: String(diagnostics.registrationCount))
                LabeledContent("Undelivered Controls", value: String(diagnostics.undeliveredCount))
                if let timestamp = diagnostics.lastReconciledAt {
                    LabeledContent(
                        "Last Live Reconcile",
                        value: Date(timeIntervalSince1970: timestamp / 1_000).formatted()
                    )
                }
            }
            LabeledContent("App Version", value: DeviceRegistration.appVersion(info: Bundle.main.infoDictionary))
        }
    }

    private var registrationLabel: String {
        let settings = OmniSettings.shared
        if settings.pendingDevice != nil { return "Pending retry" }
        guard let sent = settings.lastSentDevice, let at = settings.lastDeviceRegistrationAt else {
            return "Not registered"
        }
        let alerts = sent.alertsEnabled ? "alerts on" : "widgets only"
        let apns = settings.serverApnsEnabled == false ? ", server APNs off" : ""
        return "\(alerts)\(apns) · \(at.formatted(date: .omitted, time: .shortened))"
    }
}
