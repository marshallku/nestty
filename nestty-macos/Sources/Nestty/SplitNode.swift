import AppKit

enum SplitOrientation {
    /// Vertical divider — panes side by side (Cmd+D)
    case horizontal
    /// Horizontal divider — panes stacked (Cmd+Shift+D)
    case vertical
}

/// N-ary recursive split tree for a single tab.
/// Leaves hold any NesttyPanel (terminal or webview).
/// Does NOT store NSSplitView references — the view hierarchy is rebuilt from
/// scratch on every split/close operation.
indirect enum SplitNode {
    case leaf(any NesttyPanel)
    case branch(SplitOrientation, [SplitNode])

    // MARK: - Leaf enumeration

    func allLeaves() -> [any NesttyPanel] {
        switch self {
        case let .leaf(p): [p]
        case let .branch(_, children): children.flatMap { $0.allLeaves() }
        }
    }

    // MARK: - Tree mutations

    /// Replaces `panel`'s leaf with a new two-child branch containing the
    /// original leaf and `newNode`. Only the focused pane's space is halved;
    /// all other panes are completely unchanged.
    func splitting(
        _ panel: any NesttyPanel,
        with newNode: SplitNode,
        orientation: SplitOrientation,
    ) -> SplitNode {
        switch self {
        case let .leaf(p):
            guard ObjectIdentifier(p) == ObjectIdentifier(panel) else { return self }
            return .branch(orientation, [.leaf(p), newNode])

        case let .branch(o, children):
            return .branch(o, children.map { $0.splitting(panel, with: newNode, orientation: orientation) })
        }
    }

    /// Returns a new tree with `panel` removed, or nil if this was the only leaf.
    func removing(_ panel: any NesttyPanel) -> SplitNode? {
        switch self {
        case let .leaf(p):
            return ObjectIdentifier(p) == ObjectIdentifier(panel) ? nil : self

        case let .branch(o, children):
            let remaining = children.compactMap { $0.removing(panel) }
            if remaining.isEmpty { return nil }
            if remaining.count == 1 { return remaining[0] } // collapse single-child branch
            return .branch(o, remaining)
        }
    }
}
