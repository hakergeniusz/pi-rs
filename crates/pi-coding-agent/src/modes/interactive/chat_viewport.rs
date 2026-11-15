//! The shared fullscreen transcript and input-dock layout.
//!
//! Ground truth: `modes/interactive/chat-viewport.js` (`createChatViewport`).
//! The fullscreen renderer's root is a two-entry vertical stack: a primary
//! [`ScrollView`](pi_tui::components::scroll_view::ScrollView) over the
//! document (header + chat) that grows to fill the screen, and a dock that
//! pins the pending-messages, status, editor, and footer hosts to the bottom.
//!
//! Ownership is the one structural deviation from upstream: `chat-viewport.js`
//! receives the same component objects the regular renderer mounts flat and
//! keeps both arrangements valid at once. Rust needs a single owner, so
//! [`mount`] *moves* the flat root children into the viewport and
//! [`disassemble`] hands them back in flat order when the mode switches. The
//! recorded component ids are how the orchestrator's tree helpers find the
//! hosts in either arrangement, and
//! [`FocusTarget::Id`](pi_tui::tui::FocusTarget::Id) is how keyboard focus
//! follows them.

use pi_tui::component::{Component, Container};
use pi_tui::components::scroll_view::{Overscroll, ScrollView, Scrollbar, ScrollbarStyles};
use pi_tui::components::stack::{StackEntryOptions, VStack};
use pi_tui::mouse::{ComponentId, component_id};
use pi_tui::tui::Tui;

use super::theme;

/// The flat slot order the six hosts mount in — both in the regular
/// renderer's flat child list and inside the viewport's document and dock.
/// Upstream mounts `documentContainer` (the port keeps the header as its own
/// slot, folded into the scrolled document in fullscreen),
/// `pendingMessagesContainer`, `statusContainer` (no widget containers here),
/// `editorContainer`, and `footerContainer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSlot {
    Header = 0,
    Chat = 1,
    Pending = 2,
    Status = 3,
    Editor = 4,
    Footer = 5,
}

impl HostSlot {
    /// The slot's index in the flat order.
    pub fn index(self) -> usize {
        self as usize
    }
}

/// A mounted fullscreen chat viewport, with the component identities the
/// host needs to reach the moved hosts. Built by [`mount`], consumed by
/// [`disassemble`].
pub struct ChatViewport {
    root_id: ComponentId,
    /// The transcript `ScrollView` — the layout's primary scroll view.
    transcript_id: ComponentId,
    /// The dock stack holding the pending, status, editor, and footer hosts.
    dock_id: ComponentId,
    slot_ids: [ComponentId; 6],
}

impl ChatViewport {
    /// The identity of the slot's host, wherever it is mounted.
    pub fn slot_id(&self, slot: HostSlot) -> ComponentId {
        self.slot_ids[slot.index()]
    }

    /// The identity of the transcript scroll view, for scrollbar updates
    /// (`applyFullscreenScrollbarSetting`, `interactive-mode.ts:1659-1661`).
    pub fn transcript_id(&self) -> ComponentId {
        self.transcript_id
    }

    /// The identity of the dock stack, for the editor-slot swap.
    pub fn dock_id(&self) -> ComponentId {
        self.dock_id
    }

    /// The identity of the viewport root, for structural checks.
    pub fn root_id(&self) -> ComponentId {
        self.root_id
    }
}

/// Take the flat root children and mount them as the fullscreen viewport: a
/// primary transcript scroll view over the document, and a dock pinning the
/// input hosts to the bottom. The `Tui` must have the alt screen entered —
/// the viewport becomes the explicit layout root (`setLayoutRoot`,
/// `interactive-mode.ts:721`).
///
/// `scrollbar` is the fullscreen-scrollbar setting; the track and thumb
/// painters come from the active theme (`chat-viewport.js:14-17`).
pub fn mount(tui: &mut Tui, hosts: Vec<Box<dyn Component>>, scrollbar: Scrollbar) -> ChatViewport {
    assert_eq!(
        hosts.len(),
        6,
        "the chat viewport mounts exactly the six flat hosts"
    );
    let slot_ids: [ComponentId; 6] =
        std::array::from_fn(|index| component_id(hosts[index].as_ref()));

    // Take the hosts out from the far end so the flat indices stay stable.
    let mut hosts = hosts;
    let footer = hosts.pop().expect("footer host");
    let editor = hosts.pop().expect("editor host");
    let status = hosts.pop().expect("status host");
    let pending = hosts.pop().expect("pending host");
    let chat = hosts.pop().expect("chat host");
    let header = hosts.pop().expect("header host");

    // The document: header + chat (`documentContainer`), scrolled by the
    // transcript view. Upstream builds the view with `follow: "end"`,
    // `primary: true`, `overscroll: "chain"`, and the configured scrollbar
    // (`chat-viewport.js:4-12`).
    let mut document = Container::new();
    document.add_child(header);
    document.add_child(chat);
    let mut transcript = ScrollView::new(
        Box::new(document),
        pi_tui::components::scroll_view::ScrollViewOptions {
            follow_end: true,
            primary: true,
            overscroll: Overscroll::Chain,
            scrollbar,
        },
    );
    let theme = theme::theme();
    transcript.set_styles(ScrollbarStyles {
        track: std::sync::Arc::new({
            let theme = std::sync::Arc::clone(&theme);
            move |text: &str| theme.fg(pi_tui::theme::ThemeColor::ScrollbarTrack, text)
        }),
        thumb: std::sync::Arc::new(move |text: &str| {
            theme.fg(pi_tui::theme::ThemeColor::ScrollbarThumb, text)
        }),
    });
    // Ids are heap addresses, so each one is taken once its `Box` exists —
    // moving the box afterwards keeps the pointee (and the id) stable.
    let transcript_box = Box::new(transcript);
    let transcript_id = component_id(transcript_box.as_ref());

    // The dock: pending messages, status, editor, footer
    // (`chat-viewport.js:13-22`). Every entry shrinks; the editor keeps at
    // least three rows so its border stays visible while the transcript
    // takes the rest.
    let mut dock = VStack::new(Vec::new());
    for host in [pending, status] {
        dock.add_child(
            host,
            StackEntryOptions {
                shrink: Some(1),
                min_size: Some(0),
                ..StackEntryOptions::default()
            },
        );
    }
    dock.add_child(
        editor,
        StackEntryOptions {
            shrink: Some(1),
            min_size: Some(3),
            ..StackEntryOptions::default()
        },
    );
    dock.add_child(
        footer,
        StackEntryOptions {
            shrink: Some(1),
            min_size: Some(0),
            ..StackEntryOptions::default()
        },
    );
    let dock_box = Box::new(dock);
    let dock_id = component_id(dock_box.as_ref());

    // The root: the transcript grows from zero basis, the dock takes what it
    // needs (`chat-viewport.js:23-27`).
    let mut root = VStack::new(Vec::new());
    root.add_child(
        transcript_box,
        StackEntryOptions {
            basis: Some(0),
            grow: Some(1),
            shrink: Some(1),
            min_size: Some(1),
            max_size: None,
            visible: None,
        },
    );
    root.add_child(
        dock_box,
        StackEntryOptions {
            basis: None,
            grow: Some(0),
            shrink: Some(1),
            min_size: Some(1),
            max_size: None,
            visible: None,
        },
    );
    let root_box = Box::new(root);
    let root_id = component_id(root_box.as_ref());
    tui.set_alt_screen_root(root_box);

    ChatViewport {
        root_id,
        transcript_id,
        dock_id,
        slot_ids,
    }
}

/// Take the viewport apart and return the six hosts in flat order (header,
/// chat, pending, status, editor, footer), ready to be re-added as the
/// regular renderer's root children. The counterpart of upstream rebuilding
/// the renderer and remounting the same tree (`switchTuiMode`,
/// `interactive-mode.ts:660-680`).
pub fn disassemble(viewport: &ChatViewport, tui: &mut Tui) -> Vec<Box<dyn Component>> {
    let _ = viewport;
    tui.clear_alt_screen_root();
    // The viewport root is the only root child while fullscreen is mounted.
    let mut taken = tui.take_root_children();
    assert_eq!(
        taken.len(),
        1,
        "the fullscreen viewport is the only root child"
    );
    let mut root_box = taken.pop().expect("the viewport root");
    let root = root_box
        .as_any_mut()
        .and_then(|any| any.downcast_mut::<VStack>())
        .expect("the viewport root is a VStack");

    // Dismantle the dock from the far end so indices stay valid.
    let mut dock_box = root.stack().remove_child(1);
    let mut transcript_box = root.stack().remove_child(0);
    let dock = dock_box
        .as_any_mut()
        .and_then(|any| any.downcast_mut::<VStack>())
        .expect("the dock is a VStack");
    let footer = dock.stack().remove_child(3);
    let editor = dock.stack().remove_child(2);
    let status = dock.stack().remove_child(1);
    let pending = dock.stack().remove_child(0);

    let document = transcript_box
        .as_any_mut()
        .and_then(|any| any.downcast_mut::<ScrollView>())
        .expect("the transcript is a ScrollView")
        .take_child();
    let mut document = document;
    let mut children = document
        .as_any_mut()
        .and_then(|any| any.downcast_mut::<Container>())
        .expect("the document is a Container")
        .take_children();
    assert_eq!(children.len(), 2, "the document holds the header and chat");
    let chat = children.pop().expect("chat host");
    let header = children.pop().expect("header host");

    vec![header, chat, pending, status, editor, footer]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::terminal::{CaptureTerminal, TerminalSize};

    struct Tag(&'static str);

    impl Component for Tag {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![self.0.to_string()]
        }

        fn as_any(&self) -> Option<&dyn std::any::Any> {
            Some(self)
        }
    }

    /// Six tagged hosts, a fresh capture `Tui`, and the alt screen entered —
    /// the state [`mount`] expects.
    fn mounted_tui() -> (Tui, Vec<ComponentId>) {
        let names = ["header", "chat", "pending", "status", "editor", "footer"];
        let hosts: Vec<Box<dyn Component>> =
            names.iter().map(|name| Box::new(Tag(name)) as _).collect();
        let ids: Vec<ComponentId> = hosts
            .iter()
            .map(|host| component_id(host.as_ref()))
            .collect();
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 40,
            rows: 10,
        });
        let mut tui = Tui::new(Box::new(terminal));
        for host in hosts {
            tui.add_child(host);
        }
        tui.enter_alt_screen(pi_tui::tui_alt_screen::AltScreenOptions::default())
            .expect("enter alt screen");
        (tui, ids)
    }

    /// Mount and disassemble round-trip the hosts: identities survive, the
    /// flat order comes back, and the viewport is the explicit layout root
    /// in between.
    #[test]
    fn mount_and_disassemble_round_trip_the_hosts() {
        let (mut tui, expected_ids) = mounted_tui();
        let hosts = tui.take_root_children();
        let viewport = mount(&mut tui, hosts, Scrollbar::Auto);
        for (index, expected) in expected_ids.iter().enumerate() {
            assert_eq!(
                viewport.slot_ids[index], *expected,
                "slot {index} keeps the host's identity"
            );
        }
        assert_eq!(
            tui.alt_screen_root(),
            Some(viewport.root_id()),
            "the viewport is the explicit layout root"
        );
        assert!(
            tui.component_by_id_deep_mut(viewport.transcript_id())
                .is_some(),
            "the transcript is reachable through the viewport"
        );

        let hosts = disassemble(&viewport, &mut tui);
        let names: Vec<String> = hosts
            .iter()
            .map(|host| {
                host.as_any()
                    .and_then(|any| any.downcast_ref::<Tag>())
                    .expect("the host comes back")
                    .0
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            ["header", "chat", "pending", "status", "editor", "footer"],
            "the flat order survives the round trip"
        );
        for (host, expected) in hosts.into_iter().zip(expected_ids) {
            assert_eq!(component_id(host.as_ref()), expected);
        }
    }

    /// The transcript is the primary scroll view following the end, the dock
    /// entries carry the mount options, and every slot is reachable by
    /// identity through the deep walk.
    #[test]
    fn the_viewport_matches_the_upstream_layout_options() {
        let (mut tui, _) = mounted_tui();
        let hosts = tui.take_root_children();
        let viewport = mount(&mut tui, hosts, Scrollbar::Always);

        {
            let transcript = tui
                .component_by_id_deep_mut(viewport.transcript_id())
                .and_then(|component| {
                    component
                        .as_any_mut()
                        .and_then(|any| any.downcast_mut::<ScrollView>())
                })
                .expect("the transcript is reachable by identity");
            assert!(
                transcript.state.primary(),
                "the transcript is the primary view"
            );
            assert!(
                transcript.state.is_following_end(),
                "the transcript follows the end"
            );
            assert_eq!(transcript.state.scrollbar(), Scrollbar::Always);
        }

        {
            let dock = tui
                .component_by_id_deep_mut(viewport.dock_id())
                .and_then(|component| {
                    component
                        .as_any_mut()
                        .and_then(|any| any.downcast_mut::<VStack>())
                })
                .expect("the dock is reachable by identity");
            assert_eq!(dock.stack().len(), 4, "pending, status, editor, footer");
            assert_eq!(
                dock.entry_options(2).min_size,
                Some(3),
                "the editor keeps at least three rows"
            );
        }

        // Every host slot resolves through the deep walk.
        for slot in [
            HostSlot::Header,
            HostSlot::Chat,
            HostSlot::Pending,
            HostSlot::Status,
            HostSlot::Editor,
            HostSlot::Footer,
        ] {
            assert!(
                tui.component_by_id_deep_mut(viewport.slot_id(slot))
                    .is_some(),
                "slot {slot:?} is mounted"
            );
        }
    }
}
