import SwiftUI
import MatrixMediaSDK
#if canImport(UIKit)
import UIKit
#endif

/// Creator self-service screen for the LUD-16 Lightning Address + an inline
/// "send a Lightning tip" form. Mirrors the My Profile page in mm-dashboard.
struct LightningView: View {
    let client: MMClient

    @State private var address: String = ""
    @State private var loadedAddress: String? = nil
    @State private var savedMessage: String? = nil
    @State private var errorMessage: String? = nil
    @State private var isLoading: Bool = true
    @State private var isSaving: Bool = false

    @State private var tipStreamID: String = ""
    @State private var tipSats: String = "1000"
    @State private var tipBolt11: String? = nil
    @State private var isTipping: Bool = false

    var body: some View {
        Form {
            Section {
                if isLoading {
                    HStack { Spacer(); ProgressView(); Spacer() }
                } else {
                    TextField("name@domain.tld", text: $address)
                        .autocorrectionDisabled()
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.emailAddress)
                        #endif

                    HStack {
                        Button {
                            Task { await save() }
                        } label: {
                            if isSaving {
                                ProgressView().controlSize(.small)
                            } else {
                                Label("Save", systemImage: "square.and.arrow.down")
                            }
                        }
                        .disabled(isSaving)
                        .buttonStyle(.borderedProminent)

                        if loadedAddress?.isEmpty == false {
                            Button(role: .destructive) {
                                Task { await clear() }
                            } label: {
                                Label("Clear", systemImage: "xmark.circle")
                            }
                            .disabled(isSaving)
                        }
                    }
                }
            } header: {
                Label("Lightning Address (LUD-16)", systemImage: "bolt.fill")
            } footer: {
                Text("Donations route wallet-to-wallet via LNURL-pay. The operator never holds funds. Don't have one? Install Phoenix or Wallet of Satoshi for a free address.")
            }

            if let savedMessage = savedMessage {
                Section { Label(savedMessage, systemImage: "checkmark.circle.fill").foregroundStyle(.green) }
            }
            if let errorMessage = errorMessage {
                Section { Label(errorMessage, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red) }
            }

            Section {
                TextField("Stream ID", text: $tipStreamID)
                    .autocorrectionDisabled()
                TextField("Amount (sats)", text: $tipSats)
                    #if os(iOS)
                    .keyboardType(.numberPad)
                    #endif
                Button {
                    Task { await sendTip() }
                } label: {
                    if isTipping {
                        ProgressView().controlSize(.small)
                    } else {
                        Label("Send Lightning tip", systemImage: "bolt.heart")
                    }
                }
                .disabled(isTipping || tipStreamID.isEmpty)

                if let bolt11 = tipBolt11 {
                    bolt11Card(bolt11)
                }
            } header: {
                Label("Send a tip", systemImage: "paperplane.fill")
            }
        }
        .navigationTitle("Lightning")
        .task { await load() }
    }

    @ViewBuilder
    private func bolt11Card(_ bolt11: String) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("BOLT11 invoice").font(.caption).foregroundStyle(.secondary)
            Text(bolt11)
                .font(.system(.caption2, design: .monospaced))
                .lineLimit(4)
                .truncationMode(.middle)
            HStack {
                Button {
                    #if canImport(UIKit)
                    UIPasteboard.general.string = bolt11
                    #endif
                } label: {
                    Label("Copy", systemImage: "doc.on.doc")
                }
                .buttonStyle(.bordered)

                Button {
                    #if os(iOS)
                    if let url = URL(string: "lightning:\(bolt11)") {
                        UIApplication.shared.open(url)
                    }
                    #endif
                } label: {
                    Label("Open in wallet", systemImage: "arrow.up.forward.app")
                }
                .buttonStyle(.borderedProminent)
            }
        }
    }

    // MARK: - Actions

    private func load() async {
        isLoading = true
        errorMessage = nil
        do {
            let p = try await client.getCreatorProfile()
            let addr = p?["lightning_address"] as? String
            loadedAddress = addr
            address = addr ?? ""
        } catch {
            // Profile may not exist yet — silent
            loadedAddress = nil
        }
        isLoading = false
    }

    private func save() async {
        isSaving = true
        errorMessage = nil
        savedMessage = nil
        let trimmed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let updated = try await client.updateLightningAddress(trimmed.isEmpty ? nil : trimmed)
            let addr = updated["lightning_address"] as? String
            loadedAddress = addr
            address = addr ?? ""
            savedMessage = trimmed.isEmpty
                ? "Lightning Address cleared."
                : "Saved — donations route directly to your wallet."
        } catch {
            errorMessage = "\(error)"
        }
        isSaving = false
    }

    private func clear() async {
        address = ""
        await save()
    }

    private func sendTip() async {
        isTipping = true
        errorMessage = nil
        tipBolt11 = nil
        let sats = Int(tipSats) ?? 0
        guard sats > 0 else {
            errorMessage = "Amount must be > 0"
            isTipping = false
            return
        }
        do {
            let res = try await client.sendLightningTip(streamID: tipStreamID, amountSats: sats)
            // M1 LNURL-pay path: invoice.bolt11; legacy path: checkout_url
            if let invoice = res["invoice"] as? [String: Any],
               let bolt11 = invoice["bolt11"] as? String {
                tipBolt11 = bolt11
            } else if let url = res["checkout_url"] as? String, url.hasPrefix("ln") {
                tipBolt11 = url
            } else {
                errorMessage = "Unexpected response shape: \(res)"
            }
        } catch {
            errorMessage = "\(error)"
        }
        isTipping = false
    }
}
