import Foundation

/// Type-erased `Sendable` bridge for non-`Sendable` values that we need to
/// hop across an actor boundary (typically `DispatchQueue.main.async` or a
/// `@Sendable` callback) without redesigning every caller's closure type.
///
/// The unsafety is contained: we use this when a value (usually a closure)
/// is invoked on a single, known thread immediately after the hop, so the
/// race the type system warns about can't actually occur. Common pattern:
///
/// ```swift
/// let box = SendableBox(completion)            // completion is @escaping (Any?) -> Void
/// someBackgroundThread.run {
///     // ... compute result ...
///     DispatchQueue.main.async { box.value(result) }
/// }
/// ```
///
/// First introduced in `WebViewController.executeJS` (PR 1) for the WKWebView
/// `@Sendable` completion handler. Lifted here when `PluginSupervisor` (PR 3)
/// hit the same pattern in two more places — Rule of Three.
final class SendableBox<T>: @unchecked Sendable {
    let value: T
    init(_ value: T) {
        self.value = value
    }
}
