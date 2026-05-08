import Foundation
import TOMLKit

/// Mirrors `nestty-core::plugin::PluginManifest` for macOS — only the
/// pieces PR 3 needs. PR 4+ will grow this as panels/commands/modules
/// come online.
///
/// Discovery walks two roots and unions the result. macOS-native first
/// (`~/Library/Application Support/nestty/plugins/`), then the XDG path
/// (`~/.config/nestty/plugins/`) for users who share dotfiles across
/// Linux/macOS. Last-write-wins on duplicate plugin names; the macOS
/// path takes precedence intentionally so a per-OS override works.
enum PluginManifestStore {
    /// Top-level macOS plugin directory. Created lazily by the installer.
    static var macOSRoot: URL {
        FileManager.default
            .homeDirectoryForCurrentUser
            .appendingPathComponent("Library")
            .appendingPathComponent("Application Support")
            .appendingPathComponent("nestty")
            .appendingPathComponent("plugins")
    }

    /// XDG-style fallback (matches `dirs::config_dir()` on Linux). Lets
    /// users with a Linux/macOS shared dotfile setup install once.
    static var xdgRoot: URL {
        FileManager.default
            .homeDirectoryForCurrentUser
            .appendingPathComponent(".config")
            .appendingPathComponent("nestty")
            .appendingPathComponent("plugins")
    }

    /// Walk both plugin roots, parse every `plugin.toml`, dedupe by
    /// plugin name (macOS root wins). Parse errors are logged to stderr
    /// and skipped — one bad manifest does not break discovery.
    static func discover() -> [LoadedPluginManifest] {
        var byName: [String: LoadedPluginManifest] = [:]
        // XDG first so macOS root entries can overwrite duplicates.
        for root in [xdgRoot, macOSRoot] {
            for entry in directories(in: root) {
                guard let loaded = parse(at: entry) else { continue }
                byName[loaded.manifest.plugin.name] = loaded
            }
        }
        return Array(byName.values)
    }

    private static func directories(in root: URL) -> [URL] {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles],
        ) else { return [] }
        return entries.filter { entry in
            (try? entry.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true
        }
    }

    private static func parse(at dir: URL) -> LoadedPluginManifest? {
        let manifestURL = dir.appendingPathComponent("plugin.toml")
        guard let contents = try? String(contentsOf: manifestURL, encoding: .utf8) else {
            return nil
        }
        do {
            let manifest = try TOMLDecoder().decode(PluginManifest.self, from: contents)
            return LoadedPluginManifest(manifest: manifest, dir: dir)
        } catch {
            let msg = "[nestty] failed to parse \(manifestURL.path): \(error)\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return nil
        }
    }
}

/// Discovered manifest + the directory it lives in. `dir` is needed to
/// resolve relative `services.exec` paths against the plugin folder
/// (the install layout symlinks the binary into `<dir>/<exec>`).
struct LoadedPluginManifest {
    let manifest: PluginManifest
    let dir: URL
}

// MARK: - TOML decode types

// IMPORTANT: TOMLKit's Decoder (like Swift's JSONDecoder) does NOT honor
// `var foo: T = default` syntax — that's a Swift-init feature, not a
// Decodable feature. A missing key throws keyNotFound regardless of the
// default. We mirror serde's `#[serde(default)]` behavior with explicit
// `decodeIfPresent ?? <default>` in the inits below.

struct PluginManifest: Decodable {
    let plugin: PluginMeta
    let services: [PluginServiceDef]
    /// PR Tier 4.1 — `[[panels]]` declarations. Each panel maps a `name`
    /// (used by `plugin.open`) to a relative HTML `file` plus a display
    /// `title`. Empty when the plugin doesn't ship any panels (echo, git).
    let panels: [PluginPanelDef]
    /// PR Tier 4.2 — `[[modules]]` declarations. Each module is a status-bar
    /// widget that runs a shell command on a timer and renders the stdout
    /// (plain text or JSON `{text, tooltip}`). Empty when the plugin
    /// doesn't ship a status bar widget.
    let modules: [PluginModuleDef]
    // commands deferred (no current macOS user).

    enum CodingKeys: String, CodingKey {
        case plugin, services, panels, modules
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        plugin = try c.decode(PluginMeta.self, forKey: .plugin)
        services = try c.decodeIfPresent([PluginServiceDef].self, forKey: .services) ?? []
        panels = try c.decodeIfPresent([PluginPanelDef].self, forKey: .panels) ?? []
        modules = try c.decodeIfPresent([PluginModuleDef].self, forKey: .modules) ?? []
    }
}

struct PluginModuleDef: Decodable {
    let name: String
    let exec: String
    let interval: Int
    let position: String
    let order: Int
    let cssClass: String?

    enum CodingKeys: String, CodingKey {
        case name, exec, interval, position, order
        case cssClass = "class"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        exec = try c.decode(String.self, forKey: .exec)
        interval = try c.decodeIfPresent(Int.self, forKey: .interval) ?? 10
        position = try c.decodeIfPresent(String.self, forKey: .position) ?? "right"
        order = try c.decodeIfPresent(Int.self, forKey: .order) ?? 50
        cssClass = try c.decodeIfPresent(String.self, forKey: .cssClass)
    }
}

struct PluginPanelDef: Decodable {
    let name: String
    let title: String
    /// Relative path under the plugin directory, e.g. `panel.html`. Resolved
    /// to an absolute file URL by `PluginPanelController` at load time.
    let file: String
    let icon: String?

    enum CodingKeys: String, CodingKey {
        case name, title, file, icon
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        title = try c.decode(String.self, forKey: .title)
        file = try c.decode(String.self, forKey: .file)
        icon = try c.decodeIfPresent(String.self, forKey: .icon)
    }
}

struct PluginMeta: Decodable {
    let name: String
    let title: String
    let version: String
    let description: String?
}

struct PluginServiceDef: Decodable {
    let name: String
    let exec: String
    let args: [String]
    /// Raw activation string from the manifest. Parsed lazily because
    /// PR 3 only handles `onStartup` — the `onAction:<glob>` and
    /// `onEvent:<glob>` variants land in PR 5 with the trigger engine.
    let activation: String
    let restart: String
    let provides: [String]
    let subscribes: [String]

    enum CodingKeys: String, CodingKey {
        case name, exec, args, activation, restart, provides, subscribes
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        exec = try c.decode(String.self, forKey: .exec)
        args = try c.decodeIfPresent([String].self, forKey: .args) ?? []
        activation = try c.decodeIfPresent(String.self, forKey: .activation) ?? "onStartup"
        restart = try c.decodeIfPresent(String.self, forKey: .restart) ?? "on-crash"
        provides = try c.decodeIfPresent([String].self, forKey: .provides) ?? []
        subscribes = try c.decodeIfPresent([String].self, forKey: .subscribes) ?? []
    }
}
