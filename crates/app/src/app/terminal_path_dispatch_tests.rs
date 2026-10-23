//! Terminal file activation through the actual App dispatch boundary.
//! Host actions are inspected, never executed by an OS opener or real SSH worker.
use super::*;

fn open_path_action(workspace: &str, runtime_instance: u64, path: &Path) -> AppHostIoAction {
    AppHostIoAction::Workspace {
        workspace_id: workspace.to_owned(),
        runtime_instance,
        intent: ui::workspace::WorkspaceIoIntent::OpenPath(
            ui::workspace::WorkspacePathPayload::try_new(path.to_owned()).unwrap(),
        ),
    }
}

fn switch_to_local_fixture(app: &mut App, root: &Path, name: &str) -> String {
    let workspace = app.db.create_workspace(name).unwrap();
    app.db
        .set_workspace_path(&workspace, root.to_str().unwrap())
        .unwrap();
    app.refresh_workspaces();
    app.switch_workspace(&workspace);
    assert_eq!(app.active.id, workspace);
    assert!(!app.active.runtime.is_remote());
    workspace
}

fn install_focused_lazy_pane(app: &mut App) {
    let pane = runtime::MuxPaneId::new();
    let tab = runtime::MuxTabId::new();
    let snapshot = Arc::new(runtime::MuxSnapshot {
        tabs: vec![runtime::TabSnapshot {
            id: tab.clone(),
            title: "Terminal path fixture".into(),
            layout: runtime::LayoutNode::Pane(pane.clone()),
            panes: vec![runtime::PaneSnapshot {
                id: pane.clone(),
                session_id: None,
                title: "Unspawned fixture pane".into(),
                persistent_session_id: None,
            }],
        }],
        active_tab: Some(tab),
        focused_pane: Some(pane),
    });
    app.active
        .workspace_ui
        .apply_warm_events(&[runtime::RuntimeEvent::MuxUpdated { snapshot }], &app.i18n);
}

#[test]
fn terminal_path_dispatch_local_documents_open_in_app_instead_of_os() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    switch_to_local_fixture(&mut app, &root, "Local terminal paths");
    install_focused_lazy_pane(&mut app);
    let workspace = app.active.id.clone();
    let runtime_instance = app.active.runtime_instance;

    for name in [
        "design.md",
        "notes.txt",
        "README",
        "guide.markdown",
        "run.log",
    ] {
        let path = root.join(name);
        std::fs::write(&path, "Owned terminal document fixture").unwrap();
        let action = open_path_action(&workspace, runtime_instance, &path);

        let external = app.route_ssh_host_action(action);

        assert!(
            external.is_none(),
            "supported terminal document {name} must not reach the OS opener"
        );
        let document = app
            .documents
            .iter()
            .find(|document| document.path == path)
            .expect("terminal path must create an in-app document");
        assert_eq!(document.transport_scope, DocumentTransportScope::Local);
        assert_eq!(app.session_aux.current.active_document, Some(document.id));
        assert!(app.pending_app_host_action.is_none());
    }
}

#[test]
fn terminal_path_dispatch_extensionless_document_never_broadens_os_open_allowlist() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    switch_to_local_fixture(&mut app, &root, "Extensionless document owner");
    install_focused_lazy_pane(&mut app);
    let path = root.join("README");
    std::fs::write(&path, "Owned extensionless document fixture").unwrap();
    let resolved = resolve_workspace_host_path(path.to_str().unwrap(), None)
        .expect("an extensionless document remains a terminal file link");
    assert_eq!(
        resolved.kind,
        ui::workspace::WorkspacePathKind::OpenableFile
    );
    assert_eq!(resolved.path.as_path(), path);
    assert!(
        !app_host_openable_file(&path),
        "the existing external opener must keep its original extension allowlist"
    );

    let action = open_path_action(&app.active.id, app.active.runtime_instance, &path);
    assert!(app.route_ssh_host_action(action).is_none());
    assert!(app.documents.iter().any(|document| document.path == path));
    assert!(app.pending_app_host_action.is_none());
}

#[test]
fn terminal_path_dispatch_ssh_documents_keep_remote_scope_and_never_read_mac_twin() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    install_focused_lazy_pane(&mut app);
    let workspace = app.active.id.clone();
    let runtime_instance = app.active.runtime_instance;
    let scope = app.document_transport_scope(&workspace);
    assert!(matches!(scope, DocumentTransportScope::Ssh(_)));

    for name in ["remote.md", "remote.txt", "LICENSE"] {
        let path = root.join(name);
        std::fs::write(&path, "MAC SENTINEL: never load into an SSH document").unwrap();
        let action = open_path_action(&workspace, runtime_instance, &path);

        assert!(app.route_ssh_host_action(action).is_none());

        let document = app
            .documents
            .iter()
            .find(|document| document.path == path)
            .expect("SSH terminal path must create an in-app remote document");
        assert_eq!(document.transport_scope, scope);
        assert_eq!(app.session_aux.current.active_document, Some(document.id));
    }
    // The fixture is deliberately offline: loading must fail through the SSH lane,
    // even though each identical absolute path exists on this Mac.
    app.poll_document_io();
    assert!(
        app.documents
            .iter()
            .all(|document| document.source.is_empty())
    );
    assert!(app.document_load_inflight.is_none());
    assert!(app.pending_app_host_action.is_none());
}

#[test]
fn terminal_path_dispatch_stale_runtime_and_wrong_workspace_never_open_current_documents() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    switch_to_local_fixture(&mut app, &root, "Current terminal owner");
    install_focused_lazy_pane(&mut app);
    let path = root.join("stale.md");
    std::fs::write(&path, "Owned stale path fixture").unwrap();
    let workspace = app.active.id.clone();
    let runtime_instance = app.active.runtime_instance;

    for (owner_workspace, owner_runtime) in [
        (workspace, runtime_instance.checked_add(1).unwrap()),
        ("retired-workspace".into(), runtime_instance),
    ] {
        let action = open_path_action(&owner_workspace, owner_runtime, &path);

        assert!(
            app.route_ssh_host_action(action).is_none(),
            "a stale terminal owner must not fall through to the OS opener"
        );
        assert!(app.documents.is_empty());
        assert!(app.pending_document_open.is_none());
        assert!(app.pending_app_host_action.is_none());
    }
}

#[test]
fn terminal_path_dispatch_warm_local_and_ssh_owners_do_not_open_active_documents_or_os() {
    for remote_owner in [false, true] {
        let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
        if !remote_owner {
            switch_to_local_fixture(&mut app, &root, "Warm local terminal owner");
        }
        let owner_workspace = app.active.id.clone();
        let owner_runtime = app.active.runtime_instance;
        switch_to_local_fixture(&mut app, &root, "Different active terminal owner");
        install_focused_lazy_pane(&mut app);
        assert!(
            app.warm.contains_key(&owner_workspace),
            "the queued action must retain a resident warm owner"
        );
        let path = root.join("warm-owner.md");
        std::fs::write(&path, "Owned warm path fixture").unwrap();
        let action = open_path_action(&owner_workspace, owner_runtime, &path);

        assert!(
            app.route_ssh_host_action(action).is_none(),
            "a warm terminal document must not be opened by the OS"
        );
        assert!(app.documents.is_empty());
        assert!(app.pending_document_open.is_none());
        assert!(app.pending_app_host_action.is_none());
    }
}

#[test]
fn terminal_path_dispatch_local_png_preserves_external_open_action() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    switch_to_local_fixture(&mut app, &root, "Local image terminal owner");
    install_focused_lazy_pane(&mut app);
    let path = root.join("diagram.png");
    std::fs::write(&path, b"owned image fixture").unwrap();
    let action = open_path_action(&app.active.id, app.active.runtime_instance, &path);

    let external = app.route_ssh_host_action(action);

    assert!(matches!(
        external,
        Some(AppHostIoAction::Workspace {
            intent: ui::workspace::WorkspaceIoIntent::OpenPath(ref target),
            ..
        }) if target.as_path() == path
    ));
    assert!(app.documents.is_empty());
    assert!(app.pending_document_open.is_none());
}

#[test]
fn terminal_path_dispatch_ssh_png_never_falls_back_to_local_os_opener() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    install_focused_lazy_pane(&mut app);
    let path = root.join("remote-diagram.png");
    std::fs::write(&path, b"MAC SENTINEL: remote image is a different file").unwrap();
    let action = open_path_action(&app.active.id, app.active.runtime_instance, &path);

    assert!(app.route_ssh_host_action(action).is_none());

    assert!(app.documents.is_empty());
    assert!(app.pending_document_open.is_none());
    assert!(app.pending_app_host_action.is_none());
}

#[test]
fn terminal_path_dispatch_home_and_fleet_discard_queued_document_open() {
    for view in [
        ui::agent_terminal::AgentTerminalView::Home,
        ui::agent_terminal::AgentTerminalView::Fleet,
    ] {
        for remote in [false, true] {
            let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
            if !remote {
                switch_to_local_fixture(&mut app, &root, "Hidden local terminal owner");
            }
            install_focused_lazy_pane(&mut app);
            let path = root.join("queued-before-navigation.md");
            std::fs::write(&path, "Owned queued document fixture").unwrap();
            let action = open_path_action(&app.active.id, app.active.runtime_instance, &path);
            app.agent_terminal_ui.set_view(view);

            assert!(
                app.route_ssh_host_action(action).is_none(),
                "a hidden terminal's queued file must not reach an OS opener"
            );
            assert!(app.documents.is_empty());
            assert!(app.pending_document_open.is_none());
            assert!(app.pending_app_host_action.is_none());
        }
    }
}

#[test]
fn terminal_path_dispatch_attached_focus_discards_primary_open() {
    for remote in [false, true] {
        let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
        if !remote {
            switch_to_local_fixture(&mut app, &root, "Primary terminal owner");
        }
        install_focused_lazy_pane(&mut app);
        let primary = app.active.id.clone();
        let attachment = app
            .cross_workspace_pane
            .attach_right(
                primary.clone(),
                ui::cross_workspace::WorkspacePaneTarget::new(
                    "Attached terminal fixture",
                    app.active.runtime_instance.checked_add(1).unwrap(),
                    runtime::MuxTabId::new(),
                    runtime::MuxPaneId::new(),
                    runtime::SessionId(7),
                ),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        assert_eq!(
            app.cross_workspace_pane.focused(),
            ui::cross_workspace::FocusedSurface::Attached(attachment)
        );
        let path = root.join("queued-primary.md");
        std::fs::write(&path, "Owned primary terminal document fixture").unwrap();
        for visible in [false, true] {
            let visible_attachments = if visible { vec![attachment] } else { vec![] };
            app.frame_terminal_owner = frame_terminal_owner(
                &app.cross_workspace_pane,
                true,
                &visible_attachments,
                |_| true,
            );
            assert_ne!(app.frame_terminal_owner, FrameTerminalOwner::Primary);
            let action = open_path_action(&primary, app.active.runtime_instance, &path);

            assert!(app.route_ssh_host_action(action).is_none());

            assert!(
                app.documents.is_empty(),
                "an attached terminal's focus must retire a primary terminal file open"
            );
            assert!(app.pending_document_open.is_none());
            assert!(app.pending_app_host_action.is_none());
        }
    }
}

#[test]
fn terminal_path_dispatch_ssh_rejects_outside_root_and_lexical_escape() {
    let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
    install_focused_lazy_pane(&mut app);
    let outside = ResolverScratch::new();
    let outside_file = outside.0.join("outside.md");
    std::fs::write(&outside_file, "Mac file outside the authenticated SSH root").unwrap();
    let sibling_file = root.parent().unwrap().join("outside-root.md");
    std::fs::write(&sibling_file, "Mac sibling of the authenticated SSH root").unwrap();
    for path in [outside_file, root.join("../outside-root.md")] {
        let action = open_path_action(&app.active.id, app.active.runtime_instance, &path);

        assert!(app.route_ssh_host_action(action).is_none());

        assert!(app.documents.is_empty());
        assert!(app.pending_document_open.is_none());
        assert!(app.pending_app_host_action.is_none());
        assert!(app.document_load_inflight.is_none());
        assert!(app.ssh_pending_rpc.is_empty());
    }
}

fn cold_terminal_path_click(
    path: &Path,
) -> (
    ui::workspace::WorkspaceUi,
    ui::workspace::WorkspaceIoOperation,
    u64,
) {
    let session = runtime::SessionId(7);
    let pane = runtime::MuxPaneId::new();
    let tab = runtime::MuxTabId::new();
    let cols = 200;
    let rows = 24;
    let text = path.to_str().unwrap();
    assert!(text.chars().count() < cols);
    let mut cells = text
        .chars()
        .map(|c| {
            terminal::TerminalCell::new(
                c,
                [255, 255, 255],
                [0, 0, 0],
                false,
                false,
                Default::default(),
            )
        })
        .collect::<Vec<_>>();
    cells.resize_with(cols * rows, || {
        terminal::TerminalCell::new(
            ' ',
            [255, 255, 255],
            [0, 0, 0],
            false,
            false,
            Default::default(),
        )
    });
    let mut events = vec![
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: tab.clone(),
                    title: "Cold terminal path fixture".into(),
                    layout: runtime::LayoutNode::Pane(pane.clone()),
                    panes: vec![runtime::PaneSnapshot {
                        id: pane.clone(),
                        session_id: Some(session),
                        title: "Fixture session".into(),
                        persistent_session_id: None,
                    }],
                }],
                active_tab: Some(tab),
                focused_pane: Some(pane),
            }),
        },
        runtime::RuntimeEvent::Viewport {
            session,
            snapshot: Arc::new(terminal::TerminalViewportSnapshot {
                cols: cols as u16,
                rows: rows as u16,
                cursor: terminal::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: terminal::CursorShape::Block,
                    visible: false,
                },
                visible_cells: cells.into(),
                graphemes: Default::default(),
                dirty_ranges: Vec::new(),
                title: None,
                scroll_offset: 0,
                is_alt_screen: false,
                history: None,
            }),
            bracketed_paste: false,
        },
    ];
    let config = crate::config::TerminalConfig::default();
    let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
    let mut harness = egui_kittest::Harness::new_ui_state(
        move |ui, workspace: &mut ui::workspace::WorkspaceUi| {
            workspace.show_with_input(ui, &config, &events, &catalog, true);
            events.clear();
        },
        ui::workspace::WorkspaceUi::new(),
    );
    harness.set_size(egui::vec2(2000.0, 700.0));
    // Focus animations deliberately keep requesting repaint. Render the initial
    // viewport and settle its geometry without waiting for animation quiescence.
    harness.run_steps(2);
    let point = harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(shape) if shape.galley.text().contains(text) => Some(
                shape.pos
                    + shape.galley.rect.min.to_vec2()
                    + egui::vec2(
                        shape.galley.size().x.min(30.0) * 0.5,
                        shape.galley.size().y * 0.5,
                    ),
            ),
            _ => None,
        })
        .expect("fixture path must be visibly painted before it is clicked");
    for pressed in [true, false] {
        harness.input_mut().events.extend([
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        harness.run_steps(1);
    }
    let (operation, generation) = match harness.state_mut().take_io_intent() {
        Some(ui::workspace::WorkspaceIoIntent::ResolvePath {
            operation,
            generation,
            session: owner,
            word,
            ..
        }) => {
            assert_eq!(owner, session);
            assert_eq!(word, text);
            (operation, generation)
        }
        other => panic!("a cold terminal click must request its path: {other:?}"),
    };
    assert!(harness.state_mut().take_io_intent().is_none());
    (
        std::mem::replace(harness.state_mut(), ui::workspace::WorkspaceUi::new()),
        operation,
        generation,
    )
}

fn remote_document_path_reply(name: &str) -> deppy_remote_host::services::Reply {
    Ok(deppy_remote_host::services::Response::Directory(
        deppy_remote_host::files::DirectoryPage {
            entries: vec![deppy_remote_host::files::DirectoryEntry {
                name: name.as_bytes().to_vec(),
                is_directory: false,
                is_symlink: false,
            }],
            next: None,
        },
    ))
}

#[test]
fn terminal_path_dispatch_retired_rpc_does_not_complete_new_runtime_colliding_operation() {
    for failed_old_rpc in [false, true] {
        let (mut app, root) = super::tests::ssh_workspace_isolated_app_fixture();
        let workspace = app.active.id.clone();
        let retired_runtime = app.active.runtime_instance;
        let current_runtime = retired_runtime.checked_add(1).unwrap();
        let current_path = root.join("current.md");
        let old_path = root.join("retired.md");
        let (ui, operation, generation) = cold_terminal_path_click(&current_path);
        app.active.workspace_ui = ui;
        app.active.runtime_instance = current_runtime;
        let provider = app.remote_provider(&workspace).unwrap();
        let old_lookup = crate::ssh_terminal_paths::RemotePathLookup::start(
            &provider,
            old_path.to_str().unwrap(),
            None,
        )
        .unwrap()
        .unwrap();
        let old_reply =
            (!failed_old_rpc).then(|| (provider.clone(), remote_document_path_reply("retired.md")));

        app.finish_ssh_provider_action(
            workspace.clone(),
            SshProviderAction::PathResolution {
                runtime_instance: retired_runtime,
                operation,
                generation,
                lookup: old_lookup,
            },
            old_reply,
        );

        assert!(
            app.active.workspace_ui.take_io_intent().is_none(),
            "a retired reply must never open its file in the replacement runtime"
        );
        let current_lookup = crate::ssh_terminal_paths::RemotePathLookup::start(
            &provider,
            current_path.to_str().unwrap(),
            None,
        )
        .unwrap()
        .unwrap();
        app.finish_ssh_provider_action(
            workspace,
            SshProviderAction::PathResolution {
                runtime_instance: current_runtime,
                operation,
                generation,
                lookup: current_lookup,
            },
            Some((provider, remote_document_path_reply("current.md"))),
        );
        assert!(
            matches!(
                app.active.workspace_ui.take_io_intent(),
                Some(ui::workspace::WorkspaceIoIntent::OpenPath(ref path))
                    if path.as_path() == current_path
            ),
            "a retired failure must not consume the replacement runtime's colliding operation"
        );
        assert!(app.documents.is_empty());
        assert!(app.pending_app_host_action.is_none());
    }
}

struct ResolverScratch(PathBuf);
impl ResolverScratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "deppy-terminal-path-dispatch-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for ResolverScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn terminal_path_dispatch_resolver_preserves_balanced_parentheses_in_raw_filename() {
    let scratch = ResolverScratch::new();
    let path = scratch.0.join("notes(final)");
    std::fs::write(&path, "Owned filename fixture").unwrap();

    for token in [path.display().to_string(), format!("`{}`.", path.display())] {
        let resolved = resolve_workspace_host_path(&token, None)
            .expect("balanced filename parentheses must survive punctuation trimming");
        assert_eq!(
            resolved.kind,
            ui::workspace::WorkspacePathKind::OpenableFile
        );
        assert_eq!(resolved.path.as_path(), path);
    }
}

#[test]
fn terminal_path_dispatch_resolver_accepts_document_extensions_and_line_column_suffix() {
    let scratch = ResolverScratch::new();
    for name in [
        "design.markdown",
        "session.log",
        "LICENSE",
        "notes(final).md",
        "plain.txt",
    ] {
        let path = scratch.0.join(name);
        std::fs::write(&path, "Owned resolver fixture").unwrap();
        for token in [
            path.display().to_string(),
            format!("{}:17:4", path.display()),
        ] {
            let resolved = resolve_workspace_host_path(&token, None)
                .expect("document targets and compiler-style location suffixes must resolve");
            assert_eq!(
                resolved.kind,
                ui::workspace::WorkspacePathKind::OpenableFile
            );
            assert_eq!(resolved.path.as_path(), path);
        }
    }
}

#[test]
fn terminal_path_dispatch_resolver_keeps_directory_and_unsupported_file_guards() {
    let scratch = ResolverScratch::new();
    let directory = scratch.0.join("folder.md");
    std::fs::create_dir(&directory).unwrap();
    let resolved = resolve_workspace_host_path(directory.to_str().unwrap(), None).unwrap();
    assert_eq!(resolved.kind, ui::workspace::WorkspacePathKind::Directory);
    assert_eq!(resolved.path.as_path(), directory);

    let unsupported = scratch.0.join("executable.sh");
    std::fs::write(&unsupported, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(resolve_workspace_host_path(unsupported.to_str().unwrap(), None).is_none());
    assert!(
        resolve_workspace_host_path(scratch.0.join("missing.md").to_str().unwrap(), None).is_none()
    );
}
