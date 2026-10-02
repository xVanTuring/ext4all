import Foundation

/// Localized string lookup; keys are the English text.
func L(_ key: String) -> String {
    NSLocalizedString(key, comment: "")
}
