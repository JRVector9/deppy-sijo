//! Bounded, lazy SSH folder presentation. All paths remain server paths; no local I/O.
use super::*;
use crate::ui::popup;

const CACHE_DIRECTORIES: usize = 32;
const CACHE_ENTRIES: usize = 2048;
const CACHE_BYTES: usize = 512 * 1024;
const EXPANDED_MAX: usize = 32;
const DEPTH_MAX: usize = 32;
const ROW_HEIGHT: f32 = 34.0;

#[derive(Default)]
pub(super) struct FolderTree {
    pages: VecDeque<(RemotePath, DirectoryPage)>,
    expanded: VecDeque<RemotePath>,
    focused: Option<RemotePath>,
    editing_path: bool,
    focus_path_editor: bool,
    scroll_to_focus: bool,
    connection_details: bool,
}

#[derive(Clone)]
enum TreeRow {
    Folder {
        path: RemotePath,
        depth: usize,
    },
    Empty {
        depth: usize,
    },
    Loading {
        depth: usize,
    },
    Unsupported {
        depth: usize,
    },
    More {
        path: RemotePath,
        cursor: DirectoryCursor,
        depth: usize,
    },
}

fn parent(path: &RemotePath) -> Option<RemotePath> {
    (path.as_str() != "/")
        .then(|| {
            path.as_str()
                .rsplit_once('/')
                .map(|(p, _)| if p.is_empty() { "/" } else { p })
        })
        .flatten()
        .and_then(|value| RemotePath::new(value).ok())
}

fn basename(path: &RemotePath) -> &str {
    path.as_str()
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("/")
}

fn page_weight(path: &RemotePath, page: &DirectoryPage) -> usize {
    path.as_str().len()
        + page.next.as_ref().map_or(0, |cursor| cursor.revision.len())
        + page
            .entries
            .iter()
            .map(|entry| entry.name.len() + std::mem::size_of_val(entry))
            .sum::<usize>()
}

impl FolderTree {
    pub(super) fn page(&self, path: &RemotePath) -> Option<&DirectoryPage> {
        self.pages
            .iter()
            .find(|(cached, _)| cached == path)
            .map(|(_, page)| page)
    }
    fn is_expanded(&self, path: &RemotePath) -> bool {
        self.expanded.contains(path)
    }
    pub(super) fn collapse(&mut self, path: &RemotePath) {
        self.expanded.retain(|expanded| expanded != path);
    }
    fn expand(&mut self, path: &RemotePath, root: Option<&RemotePath>) {
        if !self.is_expanded(path) {
            if self.expanded.len() >= EXPANDED_MAX
                && let Some(index) = self.expanded.iter().position(|path| Some(path) != root)
            {
                self.expanded.remove(index);
            }
            self.expanded.push_back(path.clone());
        }
    }
    pub(super) fn navigate(&mut self, path: &RemotePath) {
        self.expand(path, Some(path));
        self.focused = Some(path.clone());
        self.editing_path = false;
    }
    pub(super) fn cache(
        &mut self,
        path: RemotePath,
        page: DirectoryPage,
        root: Option<&RemotePath>,
    ) {
        self.pages.retain(|(cached, _)| cached != &path);
        while self.pages.len() >= CACHE_DIRECTORIES
            || self
                .pages
                .iter()
                .map(|(_, page)| page.entries.len())
                .sum::<usize>()
                + page.entries.len()
                > CACHE_ENTRIES
            || self
                .pages
                .iter()
                .map(|(path, page)| page_weight(path, page))
                .sum::<usize>()
                + page_weight(&path, &page)
                > CACHE_BYTES
        {
            let Some(index) = self
                .pages
                .iter()
                .position(|(cached, _)| Some(cached) != root)
            else {
                break;
            };
            if let Some((evicted, _)) = self.pages.remove(index) {
                self.collapse(&evicted);
            }
        }
        self.pages.push_back((path, page));
    }
    fn rows(&self, root: &RemotePath) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.append_rows(root, 0, &mut rows);
        rows
    }
    fn append_rows(&self, path: &RemotePath, depth: usize, rows: &mut Vec<TreeRow>) {
        if rows.len() >= CACHE_ENTRIES || depth > DEPTH_MAX {
            return;
        }
        rows.push(TreeRow::Folder {
            path: path.clone(),
            depth,
        });
        if rows.len() >= CACHE_ENTRIES || !self.is_expanded(path) || depth == DEPTH_MAX {
            return;
        }
        let Some(page) = self.page(path) else {
            rows.push(TreeRow::Loading { depth: depth + 1 });
            return;
        };
        let mut count = 0;
        for entry in page.entries.iter().filter(|entry| entry.is_directory) {
            if rows.len() >= CACHE_ENTRIES {
                return;
            }
            count += 1;
            if let Some(child) = SshWorkspaceAddUi::folder_path(path, &entry.name) {
                self.append_rows(&child, depth + 1, rows);
            } else {
                rows.push(TreeRow::Unsupported { depth: depth + 1 });
            }
        }
        if rows.len() < CACHE_ENTRIES {
            if let Some(cursor) = &page.next {
                rows.push(TreeRow::More {
                    path: path.clone(),
                    cursor: cursor.clone(),
                    depth: depth + 1,
                });
            } else if count == 0 {
                rows.push(TreeRow::Empty { depth: depth + 1 });
            }
        }
    }
}

impl SshWorkspaceAddUi {
    pub(super) fn show_connection_summary(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> bool {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if popup::action_button(
                    ui,
                    &catalog.t("workspace.ssh.options", &[]),
                    popup::ActionTone::Ghost,
                    true,
                )
                .clicked()
                {
                    self.tree.connection_details = !self.tree.connection_details;
                }
                if let Some(server) = &self.server {
                    let label = catalog.t(
                        "workspace.ssh.connected",
                        &[("server", &format!("{}@{}", server.account, server.host))],
                    );
                    ui.add_sized(
                        [ui.available_width(), ROW_HEIGHT],
                        egui::Label::new(egui::RichText::new(label).size(12.0)).truncate(),
                    );
                }
            },
        );
        self.tree.connection_details
    }
    #[cfg(test)]
    pub(super) fn approved_tree_render_fixture(&mut self) {
        fn supply(state: &mut SshWorkspaceAddUi, intent: SshWorkspaceIntent, names: &[&str]) {
            let SshWorkspaceIntent::Browse {
                request_id, path, ..
            } = intent
            else {
                panic!("render browse")
            };
            assert!(
                state.apply_snapshot(SshWorkspaceSnapshot {
                    generation: state.generation(),
                    update: SshWorkspaceUpdate::Directory {
                        request_id,
                        path,
                        page: DirectoryPage {
                            entries: names
                                .iter()
                                .map(|name| deppy_remote_host::files::DirectoryEntry {
                                    name: name.as_bytes().to_vec(),
                                    is_directory: true,
                                    is_symlink: false
                                })
                                .collect(),
                            next: None,
                        },
                    },
                })
            );
        }
        let root = RemotePath::new("/home/ubuntu/projects").unwrap();
        let intent = self.browse(root.clone(), None).unwrap();
        supply(
            self,
            intent,
            &[
                "nomorevibe",
                "xAegis-fast",
                "nmv-rsv",
                "nomorevibe-deepseek-admin",
                "xAegis-devfix",
                "nmv-qc",
                "nmv-second",
                "xAegis-update-approvals",
                "nmv-rem",
                "nmv-build",
                "nmv-follow",
                "xAegis-nmv",
                "xAegis-local-id-fix",
                "nmv-garb",
                "nmv-ci2",
                "xAegis-update-integration",
                ".agents",
                "nmv-hold",
            ],
        );
        for (suffix, names) in [
            ("nomorevibe", &["apps", "packages", "docs", "scripts"][..]),
            ("nomorevibe/apps", &["admin", "web"][..]),
            ("nomorevibe/apps/web", &["public", "src"][..]),
            ("xAegis-fast", &["src", "tests"][..]),
        ] {
            let path = RemotePath::new(format!("{}/{suffix}", root.as_str())).unwrap();
            let intent = self.toggle_folder(path).unwrap();
            supply(self, intent, names);
        }
        let selected = RemotePath::new("/home/ubuntu/projects/nomorevibe/apps/web").unwrap();
        let SshWorkspaceIntent::VerifyRoot { request_id, .. } =
            self.verify_root(selected.clone()).unwrap()
        else {
            panic!("render verify")
        };
        let mut target = self.server.clone().unwrap();
        target.canonical_root = selected;
        target.remote_workspace_id = "render-project".into();
        assert!(self.apply_snapshot(SshWorkspaceSnapshot {
            generation: self.generation(),
            update: SshWorkspaceUpdate::RootVerified { request_id, target }
        }));
    }
    pub(super) fn show_folder_heading(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(catalog.t("workspace.ssh.folders", &[]))
                    .size(12.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.phase == Phase::Connected {
                    let collapse =
                        ui.add_sized([ROW_HEIGHT, ROW_HEIGHT], egui::Button::new("").frame(false));
                    let c = collapse.rect.center();
                    for offset in [-3.0, 3.0] {
                        ui.painter().add(egui::Shape::line(
                            vec![
                                c + egui::vec2(-4.0, offset + 2.0),
                                c + egui::vec2(0.0, offset - 1.0),
                                c + egui::vec2(4.0, offset + 2.0),
                            ],
                            egui::Stroke::new(1.0, connector_ui::popup::palette(ui).muted),
                        ));
                    }
                    collapse.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            catalog.t("workspace.ssh.collapse_subfolders", &[]),
                        )
                    });
                    if collapse
                        .on_hover_text(catalog.t("workspace.ssh.collapse_subfolders", &[]))
                        .clicked()
                    {
                        self.tree.expanded.clear();
                        if let Some((root, _)) = &self.directory {
                            self.tree.expand(root, Some(root));
                        }
                    }
                }
            });
        });
    }
    fn toggle_folder(&mut self, path: RemotePath) -> Option<SshWorkspaceIntent> {
        self.tree.focused = Some(path.clone());
        if self.tree.is_expanded(&path) {
            self.tree.collapse(&path);
            return None;
        }
        if self.tree.page(&path).is_some() {
            self.tree
                .expand(&path, self.directory.as_ref().map(|(root, _)| root));
            return None;
        }
        // One correlated browse at a time. Existing cached branches remain usable.
        if self.pending_browse.is_some() {
            return None;
        }
        let intent = self.request_directory(path.clone(), None, false)?;
        self.tree
            .expand(&path, self.directory.as_ref().map(|(root, _)| root));
        Some(intent)
    }

    pub(super) fn show_folder_tree(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
    ) -> Option<SshWorkspaceIntent> {
        let mut action = None;
        let root = self.directory.as_ref().map(|(path, _)| path.clone());
        let up_path = root.as_ref().and_then(parent);
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 36.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let up = popup::action_button(ui, "↑", popup::ActionTone::Ghost, up_path.is_some());
                up.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        catalog.t("workspace.ssh.up", &[]),
                    )
                });
                if up
                    .on_hover_text(catalog.t("workspace.ssh.up", &[]))
                    .clicked()
                    && let Some(path) = up_path
                {
                    action = self.browse(path, None);
                }
                if self.tree.editing_path {
                    let row = popup::path_input(
                        ui,
                        &mut self.path_input,
                        &catalog.t("workspace.ssh.open_path", &[]),
                    );
                    if self.tree.focus_path_editor {
                        row.input.request_focus();
                        self.tree.focus_path_editor = false;
                    }
                    if row.input.changed() {
                        if !self.browse_changes_root
                            && let Some((_, path)) = self.pending_browse.as_ref()
                            && self.tree.page(path).is_none()
                        {
                            self.tree.collapse(path);
                        }
                        self.pending_browse = None;
                    }
                    if (row.input.has_focus() || row.input.lost_focus())
                        && ui.input_mut(|input| {
                            input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                        })
                    {
                        self.tree.editing_path = false;
                        if let Some(root) = &root {
                            self.path_input = root.as_str().to_owned();
                        }
                    }
                    if row.browse.clicked()
                        || (row.input.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        match RemotePath::new(self.path_input.clone()) {
                            Ok(path) => action = self.browse(path, None),
                            Err(_) => self.error = Some(SshWorkspaceError::FolderMissing),
                        }
                    }
                } else {
                    let edit_width = 34.0;
                    let crumb_width = (ui.available_width() - edit_width - 7.0).max(1.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(crumb_width, 36.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("ssh_folder_breadcrumbs")
                                .max_height(36.0)
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        if let Some(root) = &root {
                                            let mut current = String::new();
                                            let crumbs = std::iter::once("/").chain(
                                                root.as_str()
                                                    .split('/')
                                                    .filter(|part| !part.is_empty()),
                                            );
                                            for (index, part) in crumbs.enumerate() {
                                                if index > 0 {
                                                    ui.label(egui::RichText::new("›").weak());
                                                    current.push('/');
                                                    current.push_str(part);
                                                }
                                                let path = RemotePath::new(if index == 0 {
                                                    "/"
                                                } else {
                                                    &current
                                                })
                                                .expect("validated remote path component");
                                                let response = ui.add(
                                                    egui::Button::new(
                                                        egui::RichText::new(part).size(12.0),
                                                    )
                                                    .frame(false)
                                                    .min_size(egui::vec2(0.0, ROW_HEIGHT)),
                                                );
                                                if response.on_hover_text(path.as_str()).clicked() {
                                                    action = self.browse(path, None);
                                                }
                                            }
                                        }
                                    });
                                });
                        },
                    );
                    let edit = popup::action_button(ui, " ", popup::ActionTone::Ghost, true);
                    let c = edit.rect.center();
                    let stroke = egui::Stroke::new(1.2, connector_ui::popup::palette(ui).muted);
                    ui.painter().add(egui::Shape::closed_line(
                        vec![
                            c + egui::vec2(-5.0, 5.0),
                            c + egui::vec2(-3.0, 1.0),
                            c + egui::vec2(4.0, -6.0),
                            c + egui::vec2(7.0, -3.0),
                            c + egui::vec2(0.0, 4.0),
                        ],
                        stroke,
                    ));
                    edit.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            catalog.t("workspace.ssh.edit_path", &[]),
                        )
                    });
                    if edit
                        .on_hover_text(catalog.t("workspace.ssh.edit_path", &[]))
                        .clicked()
                    {
                        self.tree.editing_path = true;
                        self.tree.focus_path_editor = true;
                    }
                }
            });
        });
        let rows = root
            .as_ref()
            .map(|root| self.tree.rows(root))
            .unwrap_or_default();
        let selection_height = self.selected_path.as_ref().map_or(0.0, |path| {
            ui.fonts_mut(|fonts| {
                fonts
                    .layout(
                        path.as_str().into(),
                        egui::FontId::monospace(12.0),
                        egui::Color32::WHITE,
                        (ui.available_width() - 24.0).max(1.0),
                    )
                    .size()
                    .y
            }) + 64.0
        });
        // Reserve the visible full-path panel, hint, optional-name header and shared footer.
        let bottom = ui
            .max_rect()
            .bottom()
            .min(ui.ctx().content_rect().bottom() - 95.0);
        let height = (bottom - ui.cursor().top() - selection_height - 96.0).clamp(68.0, 306.0);
        popup::list_row(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("ssh_folder_tree")
                .min_scrolled_height(height)
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if rows.is_empty() {
                        ui.label(catalog.t("workspace.ssh.loading_folders", &[]));
                    }
                    for row in &rows {
                        match row {
                            TreeRow::Folder { path, depth } => {
                                if let Some(intent) =
                                    self.show_folder_row(ui, catalog, path, *depth, &rows)
                                {
                                    action = Some(intent);
                                }
                            }
                            TreeRow::More {
                                path,
                                cursor,
                                depth,
                            } => {
                                ui.horizontal(|ui| {
                                    ui.add_space((*depth as f32 * 18.0).min(100.0));
                                    if popup::action_button(
                                        ui,
                                        &catalog.t("workspace.ssh.next_page", &[]),
                                        popup::ActionTone::Ghost,
                                        self.pending_browse.is_none(),
                                    )
                                    .clicked()
                                    {
                                        action = self.request_directory(
                                            path.clone(),
                                            Some(cursor.clone()),
                                            false,
                                        );
                                    }
                                });
                            }
                            TreeRow::Empty { depth }
                            | TreeRow::Loading { depth }
                            | TreeRow::Unsupported { depth } => {
                                let key = match row {
                                    TreeRow::Empty { .. } => "workspace.ssh.empty",
                                    TreeRow::Loading { .. } => "workspace.ssh.loading_folders",
                                    _ => "workspace.ssh.unsupported_name",
                                };
                                ui.horizontal(|ui| {
                                    ui.add_space((*depth as f32 * 18.0).min(100.0));
                                    ui.add_sized(
                                        [ui.available_width(), ROW_HEIGHT],
                                        egui::Label::new(
                                            egui::RichText::new(catalog.t(key, &[]))
                                                .size(11.0)
                                                .weak(),
                                        )
                                        .truncate(),
                                    );
                                });
                            }
                        }
                    }
                });
        });
        ui.label(
            egui::RichText::new(catalog.t("workspace.ssh.tree_hint", &[]))
                .size(11.0)
                .weak(),
        );
        action
    }

    fn show_folder_row(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        path: &RemotePath,
        depth: usize,
        rows: &[TreeRow],
    ) -> Option<SshWorkspaceIntent> {
        let colors = connector_ui::popup::palette(ui);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::hover(),
        );
        let left = rect.left() + (depth as f32 * 18.0).min(100.0);
        let toggle_rect = egui::Rect::from_min_max(
            egui::pos2(left, rect.top()),
            egui::pos2(left + 22.0, rect.bottom()),
        );
        let name_rect = egui::Rect::from_min_max(egui::pos2(left + 22.0, rect.top()), rect.max);
        let id = ui.id().with(("ssh_folder", path.as_str()));
        let name = basename(path);
        let response = ui.interact(name_rect, id, egui::Sense::click());
        let open = self.tree.is_expanded(path);
        let expandable = depth < DEPTH_MAX
            && self.tree.page(path).is_none_or(|page| {
                page.next.is_some() || page.entries.iter().any(|entry| entry.is_directory)
            });
        let toggle = ui.interact(
            toggle_rect,
            id.with("toggle"),
            if expandable {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            },
        );
        let selected = self.selected_path.as_ref() == Some(path);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                ui.is_enabled(),
                selected,
                path.as_str(),
            )
        });
        if expandable {
            toggle.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    ui.is_enabled(),
                    catalog.t(
                        if open {
                            "workspace.ssh.collapse_folder"
                        } else {
                            "workspace.ssh.expand_folder"
                        },
                        &[("name", name)],
                    ),
                )
            });
        }
        if selected || response.hovered() || toggle.hovered() {
            ui.painter().rect_filled(
                rect,
                2,
                if selected {
                    colors.accent_low
                } else {
                    colors.hover
                },
            );
        }
        if response.has_focus() || toggle.has_focus() {
            ui.painter().rect_stroke(
                rect,
                2,
                egui::Stroke::new(1.0, colors.accent),
                egui::StrokeKind::Inside,
            );
        }
        if expandable {
            let c = toggle_rect.center();
            let points = if open {
                vec![
                    c + egui::vec2(-4.0, -2.0),
                    c + egui::vec2(0.0, 2.0),
                    c + egui::vec2(4.0, -2.0),
                ]
            } else {
                vec![
                    c + egui::vec2(-2.0, -4.0),
                    c + egui::vec2(2.0, 0.0),
                    c + egui::vec2(-2.0, 4.0),
                ]
            };
            ui.painter().add(egui::Shape::line(
                points,
                egui::Stroke::new(1.2, colors.muted),
            ));
        }
        let folder = egui::Rect::from_min_size(
            egui::pos2(name_rect.left() + 3.0, rect.center().y - 6.0),
            egui::vec2(14.0, 11.0),
        );
        ui.painter().rect_stroke(
            folder,
            1,
            egui::Stroke::new(
                1.0,
                if selected {
                    colors.accent
                } else {
                    colors.muted
                },
            ),
            egui::StrokeKind::Inside,
        );
        ui.painter().line_segment(
            [
                folder.left_top() - egui::vec2(0.0, 2.0),
                folder.left_top() + egui::vec2(6.0, -2.0),
            ],
            egui::Stroke::new(
                1.0,
                if selected {
                    colors.accent
                } else {
                    colors.muted
                },
            ),
        );
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(folder.right() + 7.0, rect.top()),
            egui::pos2(
                rect.right() - if selected { 22.0 } else { 5.0 },
                rect.bottom(),
            ),
        );
        ui.painter()
            .with_clip_rect(text_rect.intersect(ui.clip_rect()))
            .text(
                text_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(12.0),
                if selected { colors.accent } else { colors.text },
            );
        if selected {
            let c = egui::pos2(rect.right() - 10.0, rect.center().y);
            ui.painter().add(egui::Shape::line(
                vec![
                    c + egui::vec2(-4.0, 0.0),
                    c + egui::vec2(-1.0, 3.0),
                    c + egui::vec2(5.0, -4.0),
                ],
                egui::Stroke::new(1.2, colors.accent),
            ));
        }
        let row_focused = response.has_focus();
        let focused = row_focused || toggle.has_focus();
        if focused {
            let focus_id = if row_focused { response.id } else { toggle.id };
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    focus_id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
        }
        let key = focused
            .then(|| {
                ui.input_mut(|input| {
                    [
                        egui::Key::ArrowDown,
                        egui::Key::ArrowUp,
                        egui::Key::ArrowLeft,
                        egui::Key::ArrowRight,
                        egui::Key::Home,
                        egui::Key::End,
                    ]
                    .into_iter()
                    .find(|key| input.consume_key(egui::Modifiers::NONE, *key))
                })
            })
            .flatten();
        if let Some(key) = key {
            // egui schedules directional focus before widget code runs. Clear it
            // even on the first focused frame so this tree moves focus only once.
            ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
            if matches!(
                key,
                egui::Key::ArrowUp | egui::Key::ArrowDown | egui::Key::Home | egui::Key::End
            ) {
                let paths: Vec<_> = rows
                    .iter()
                    .filter_map(|row| {
                        if let TreeRow::Folder { path, .. } = row {
                            Some(path)
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(index) = paths.iter().position(|candidate| *candidate == path) {
                    let next = match key {
                        egui::Key::Home => 0,
                        egui::Key::End => paths.len() - 1,
                        egui::Key::ArrowDown => (index + 1).min(paths.len() - 1),
                        _ => index.saturating_sub(1),
                    };
                    self.tree.focused = Some(paths[next].clone());
                    self.tree.scroll_to_focus = true;
                    ui.memory_mut(|memory| {
                        memory.request_focus(ui.id().with(("ssh_folder", paths[next].as_str())))
                    });
                }
            } else if key == egui::Key::ArrowLeft {
                if open {
                    self.tree.collapse(path);
                } else if let Some(parent) = parent(path)
                    && rows
                        .iter()
                        .any(|row| matches!(row, TreeRow::Folder { path, .. } if path == &parent))
                {
                    self.tree.focused = Some(parent.clone());
                    self.tree.scroll_to_focus = true;
                    ui.memory_mut(|memory| {
                        memory.request_focus(ui.id().with(("ssh_folder", parent.as_str())))
                    });
                }
            } else if expandable {
                if !open {
                    return self.toggle_folder(path.clone());
                }
                if let Some(TreeRow::Folder { path: child, .. }) = rows
                    .iter()
                    .skip_while(
                        |row| !matches!(row, TreeRow::Folder { path: value, .. } if value == path),
                    )
                    .nth(1)
                {
                    self.tree.focused = Some(child.clone());
                    self.tree.scroll_to_focus = true;
                    ui.memory_mut(|memory| {
                        memory.request_focus(ui.id().with(("ssh_folder", child.as_str())))
                    });
                }
            }
        }
        if self.tree.scroll_to_focus
            && response.has_focus()
            && self.tree.focused.as_ref() == Some(path)
        {
            response.scroll_to_me(Some(egui::Align::Center));
            self.tree.scroll_to_focus = false;
        }
        if toggle.clicked() || (response.double_clicked() && expandable) {
            if toggle.clicked() {
                toggle.request_focus();
            } else {
                response.request_focus();
            }
            return self.toggle_folder(path.clone());
        }
        if response.clicked() {
            response.request_focus();
            self.tree.focused = Some(path.clone());
            return self.verify_root(path.clone());
        }
        None
    }

    pub(super) fn show_folder_selection(&self, ui: &mut egui::Ui, catalog: &i18n::Catalog) {
        if let Some(path) = &self.selected_path {
            popup::list_row(ui, |ui| {
                ui.label(
                    egui::RichText::new(catalog.t("workspace.ssh.selected_folder", &[]))
                        .size(11.0)
                        .weak(),
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(path.as_str()).monospace().size(12.0))
                        .wrap(),
                );
                if let Some(server) = &self.server {
                    ui.label(
                        egui::RichText::new(format!("{}@{}", server.account, server.host))
                            .size(11.0)
                            .weak(),
                    );
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_remote_host::files::DirectoryEntry;
    const ROOT: &str = "/home/ubuntu/projects";

    fn path(value: &str) -> RemotePath {
        RemotePath::new(value).unwrap()
    }
    fn page(names: &[&str]) -> DirectoryPage {
        DirectoryPage {
            entries: names
                .iter()
                .map(|name| DirectoryEntry {
                    name: name.as_bytes().to_vec(),
                    is_directory: true,
                    is_symlink: false,
                })
                .collect(),
            next: None,
        }
    }
    fn fixture() -> SshWorkspaceAddUi {
        let mut state = SshWorkspaceAddUi::new();
        state.draft.destination = "ubuntu@gpu3090".into();
        let profile = state.draft_profile().unwrap();
        let target = SshWorkspaceTarget::new(
            profile.id.clone(),
            "gpu3090".into(),
            22,
            "ubuntu".into(),
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into(),
            "root-id".into(),
            path("/"),
        )
        .unwrap();
        state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Connected {
                profile,
                target,
                home: path(ROOT),
                agents: vec![],
            },
        });
        state.queued.clear();
        let request = state.pending_browse.as_ref().unwrap().0;
        complete(&mut state, request, path(ROOT), page(&["project", "other"]));
        state
    }
    fn complete(
        state: &mut SshWorkspaceAddUi,
        request_id: u64,
        path: RemotePath,
        page: DirectoryPage,
    ) {
        assert!(state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id,
                path,
                page
            }
        }));
    }
    fn verify(state: &mut SshWorkspaceAddUi, selected: RemotePath) {
        let Some(SshWorkspaceIntent::VerifyRoot { request_id, .. }) =
            state.verify_root(selected.clone())
        else {
            panic!("verify")
        };
        let mut target = state.server.clone().unwrap();
        target.canonical_root = selected;
        assert!(state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::RootVerified { request_id, target }
        }));
    }
    #[test]
    fn expansion_is_lazy_cached_and_independent_from_verified_selection() {
        let mut state = fixture();
        verify(&mut state, path(ROOT));
        let project = path(&format!("{ROOT}/project"));
        let Some(SshWorkspaceIntent::Browse {
            request_id,
            path: requested,
            ..
        }) = state.toggle_folder(project.clone())
        else {
            panic!("lazy browse")
        };
        assert_eq!(requested, project);
        assert!(state.can_add());
        complete(
            &mut state,
            request_id,
            project.clone(),
            page(&["apps", "docs"]),
        );
        assert_eq!(state.directory.as_ref().unwrap().0, path(ROOT));
        assert_eq!(state.path_input, ROOT);
        assert_eq!(state.selected_path.as_ref(), Some(&path(ROOT)));
        assert!(state.toggle_folder(project.clone()).is_none());
        assert!(!state.tree.is_expanded(&project));
        assert!(
            state.toggle_folder(project.clone()).is_none(),
            "re-expanding cached data must not issue I/O"
        );
        assert!(state.tree.is_expanded(&project));
        assert!(state.can_add());
    }
    #[test]
    fn stale_expansion_cannot_move_new_navigation_or_restore_closed_generation() {
        let mut state = fixture();
        let project = path(&format!("{ROOT}/project"));
        let Some(SshWorkspaceIntent::Browse {
            request_id: stale, ..
        }) = state.toggle_folder(project.clone())
        else {
            panic!("expand")
        };
        let Some(SshWorkspaceIntent::Browse {
            request_id: current,
            ..
        }) = state.browse(path("/srv"), None)
        else {
            panic!("navigate")
        };
        assert!(!state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id: stale,
                path: project.clone(),
                page: page(&["late"])
            }
        }));
        complete(&mut state, current, path("/srv"), page(&["current"]));
        assert_eq!(state.directory.as_ref().unwrap().0, path("/srv"));
        let old = state.generation();
        state.cancel();
        assert!(!state.apply_snapshot(SshWorkspaceSnapshot {
            generation: old,
            update: SshWorkspaceUpdate::Directory {
                request_id: current,
                path: path("/srv"),
                page: page(&["late"])
            }
        }));
        assert!(state.tree.pages.is_empty());
    }
    #[test]
    fn tree_cache_and_visible_rows_remain_bounded() {
        let mut tree = FolderTree::default();
        let root = path(ROOT);
        tree.navigate(&root);
        tree.cache(root.clone(), page(&["project"]), Some(&root));
        for index in 0..80 {
            let branch = path(&format!("{ROOT}/branch{index}"));
            let mut entries = page(&[]);
            entries.entries = (0..256)
                .map(|entry| DirectoryEntry {
                    name: format!("child-{entry}").into_bytes(),
                    is_directory: true,
                    is_symlink: false,
                })
                .collect();
            tree.cache(branch.clone(), entries, Some(&root));
            tree.expand(&branch, Some(&root));
        }
        assert!(tree.pages.len() <= CACHE_DIRECTORIES);
        assert!(
            tree.pages
                .iter()
                .map(|(_, page)| page.entries.len())
                .sum::<usize>()
                <= CACHE_ENTRIES
        );
        assert!(
            tree.pages
                .iter()
                .map(|(path, page)| page_weight(path, page))
                .sum::<usize>()
                <= CACHE_BYTES
        );
        assert!(tree.expanded.len() <= EXPANDED_MAX);
        assert!(tree.page(&root).is_some(), "visible root remains cached");
        assert!(tree.rows(&root).len() <= CACHE_ENTRIES);
        assert_eq!(parent(&path("/")), None);
        assert_eq!(parent(&path("/srv")), Some(path("/")));
    }
    #[test]
    fn tree_cache_byte_budget_evicts_large_pages() {
        let mut tree = FolderTree::default();
        for index in 0..20 {
            let mut page = page(&[]);
            page.entries = (0..32)
                .map(|_| DirectoryEntry {
                    name: vec![b'a'; 4000],
                    is_directory: true,
                    is_symlink: false,
                })
                .collect();
            tree.cache(path(&format!("/branch{index}")), page, None);
        }
        assert!(
            tree.pages
                .iter()
                .map(|(path, page)| page_weight(path, page))
                .sum::<usize>()
                <= CACHE_BYTES
        );
        assert!(
            tree.pages.len() < CACHE_DIRECTORIES,
            "byte budget must apply before directory count is reached"
        );
    }
    #[test]
    fn malformed_expansion_does_not_leave_a_permanent_loading_row() {
        let mut state = fixture();
        let project = path(&format!("{ROOT}/project"));
        let Some(SshWorkspaceIntent::Browse { request_id, .. }) =
            state.toggle_folder(project.clone())
        else {
            panic!("expand")
        };
        let mut oversized = page(&[]);
        oversized.entries = vec![DirectoryEntry {
            name: vec![b'a'; DIRECTORY_BYTES_MAX + 1],
            is_directory: true,
            is_symlink: false,
        }];
        assert!(!state.apply_snapshot(SshWorkspaceSnapshot {
            generation: state.generation(),
            update: SshWorkspaceUpdate::Directory {
                request_id,
                path: project.clone(),
                page: oversized
            }
        }));
        assert!(!state.tree.is_expanded(&project));
        assert!(state.pending_browse.is_none());
        assert!(
            state.toggle_folder(project).is_some(),
            "failed expansion remains retryable"
        );
    }
    #[test]
    fn tree_row_chevron_and_selection_have_independent_34pt_targets() {
        use egui_kittest::kittest::Queryable as _;
        let mut state = fixture();
        verify(&mut state, path(ROOT));
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut SshWorkspaceAddUi| {
                    state.show_folder_tree(ui, &catalog);
                },
                state,
            );
        harness.run();
        let name = harness.get_by_label(&format!("{ROOT}/project")).rect();
        let toggle = harness.get_by_label("Expand project").rect();
        assert_eq!(name.height(), ROW_HEIGHT);
        assert_eq!(toggle.height(), ROW_HEIGHT);
        assert!(toggle.right() <= name.left());
        harness.get_by_label("Expand project").click();
        harness.run();
        assert_eq!(harness.state().selected_path.as_ref(), Some(&path(ROOT)));
        assert!(harness.state().can_add());
        let project = path(&format!("{ROOT}/project"));
        let request = harness.state().pending_browse.as_ref().unwrap().0;
        complete(
            harness.state_mut(),
            request,
            project.clone(),
            page(&["apps"]),
        );
        harness.run();
        harness.get_by_label(&format!("{ROOT}/project")).click();
        harness.run();
        assert_eq!(harness.state().selected_path.as_ref(), Some(&project));
        assert!(harness.state().tree.is_expanded(&project));
        assert!(
            !harness.state().can_add(),
            "new selection still requires its own root verification"
        );
    }
    #[test]
    fn keyboard_left_stays_on_the_visible_tree_root() {
        use egui_kittest::kittest::Queryable as _;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut SshWorkspaceAddUi| {
                    state.show_folder_tree(ui, &catalog);
                },
                fixture(),
            );
        harness.run();
        harness.get_by_label(ROOT).click();
        harness.run();
        harness.key_press(egui::Key::ArrowLeft);
        harness.run();
        assert!(!harness.state().tree.is_expanded(&path(ROOT)));
        harness.key_press(egui::Key::ArrowLeft);
        harness.run();
        assert_eq!(harness.state().tree.focused.as_ref(), Some(&path(ROOT)));
        assert!(!harness.state().tree.scroll_to_focus);
    }
    #[test]
    fn keyboard_arrows_move_actual_focus_once_without_changing_selection() {
        use egui_kittest::kittest::Queryable as _;
        let mut state = fixture();
        verify(&mut state, path(ROOT));
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut SshWorkspaceAddUi| {
                    state.show_folder_tree(ui, &catalog);
                },
                state,
            );
        harness.run();
        harness.get_by_label(ROOT).focus();
        harness.run();
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert!(
            harness
                .get_by_label(&format!("{ROOT}/project"))
                .is_focused(),
            "one ArrowDown must focus the next folder exactly once"
        );
        assert_eq!(harness.state().selected_path.as_ref(), Some(&path(ROOT)));
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert!(harness.get_by_label(ROOT).is_focused());
        assert!(harness.state().can_add());
    }
    #[test]
    fn keyboard_home_end_enter_and_space_use_the_focused_folder() {
        use egui_kittest::kittest::Queryable as _;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(560.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut SshWorkspaceAddUi| {
                    state.show_folder_tree(ui, &catalog);
                },
                fixture(),
            );
        harness.run();
        harness.get_by_label(ROOT).focus();
        harness.run();
        harness.key_press(egui::Key::End);
        harness.run();
        let other = format!("{ROOT}/other");
        assert!(
            harness.get_by_label(&other).is_focused(),
            "End focuses the last visible folder"
        );
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().selected_path.as_ref(), Some(&path(&other)));
        assert!(harness.state().pending_verify.is_some());
        assert!(!harness.state().can_add());
        harness.key_press(egui::Key::Home);
        harness.run();
        assert!(
            harness.get_by_label(ROOT).is_focused(),
            "Home focuses the first visible folder"
        );
        harness.key_press(egui::Key::Space);
        harness.run();
        assert_eq!(harness.state().selected_path.as_ref(), Some(&path(ROOT)));
        assert!(
            harness.state().tree.is_expanded(&path(ROOT)),
            "selection preserves expansion"
        );
    }
    #[test]
    fn connected_picker_keeps_selected_full_path_above_footer_at_both_viewports() {
        use egui_kittest::kittest::Queryable as _;
        for size in [egui::vec2(800.0, 950.0), egui::vec2(390.0, 700.0)] {
            let mut state = fixture();
            state.approved_tree_render_fixture();
            let mut dialog = crate::workspace_add::WorkspaceAddUi::new(
                crate::workspace_add::WorkspaceAddPurpose::SwitchRuntime,
                std::path::PathBuf::new(),
            );
            dialog.open_ssh_draft(state.draft());
            std::mem::swap(dialog.ssh_mut(), &mut state);
            let catalog = i18n::Catalog::load("en-US").unwrap();
            let mut harness = egui_kittest::Harness::builder()
                .with_size(size)
                .build_ui_state(
                    move |ui, dialog: &mut crate::workspace_add::WorkspaceAddUi| {
                        dialog.show(ui.ctx(), &catalog);
                    },
                    dialog,
                );
            harness.run();
            let selected = harness
                .get_by_role_and_label(
                    egui::accesskit::Role::Label,
                    "/home/ubuntu/projects/nomorevibe/apps/web",
                )
                .rect();
            let footer = harness
                .get_by_role_and_label(egui::accesskit::Role::Button, "Add workspace")
                .rect();
            let hint = harness
                .get_by_label("Use arrows to expand · Click a folder name to select")
                .rect();
            assert!(
                selected.top() >= hint.bottom(),
                "selection must stay outside tree scroll: {selected:?} {hint:?}"
            );
            assert!(
                selected.bottom() < footer.top(),
                "full selected path must fit above fixed footer at {size:?}: {selected:?} {footer:?}"
            );
            assert!(
                selected.left() >= 0.0 && selected.right() <= size.x && selected.bottom() <= size.y,
                "full path must be visible inside the viewport: {selected:?} {size:?}"
            );
            let painted = harness
                .output()
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.text() == "/home/ubuntu/projects/nomorevibe/apps/web" =>
                    {
                        Some((
                            shape.clip_rect,
                            egui::Rect::from_min_size(text.pos, text.galley.size()),
                        ))
                    }
                    _ => None,
                })
                .expect("the selected full path must actually be painted");
            assert!(
                painted.0.contains_rect(painted.1),
                "selected full path must not be clipped by the outer body at {size:?}: {painted:?}"
            );
            assert!(
                harness.query_by_label("SSH address").is_none(),
                "connected summary collapses entry form"
            );
            assert!(
                harness
                    .query_by_label("Optional. Defaults to the server folder name.")
                    .is_none(),
                "optional name starts collapsed"
            );
        }
    }
    #[test]
    fn editor_escape_returns_to_breadcrumbs_without_closing_picker() {
        use egui_kittest::kittest::Queryable as _;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 900.0))
            .build_ui_state(
                move |ui, state: &mut (SshWorkspaceAddUi, bool)| {
                    let close = popup::show(
                        ui.ctx(),
                        popup::PopupSpec {
                            id: egui::Id::new("ssh_editor_escape_fixture"),
                            width: 560.0,
                            title: "Choose folder",
                            subtitle: "",
                            close_label: "Close",
                            close_enabled: true,
                        },
                        |ui| {
                            state.0.show_folder_tree(ui, &catalog);
                        },
                    );
                    state.1 |= close;
                },
                (fixture(), false),
            );
        harness.run();
        harness.get_by_label("Enter path").click();
        harness.run();
        assert!(harness.state().0.tree.editing_path);
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(
            !harness.state().1,
            "editor must own Escape before the outer picker modal"
        );
        assert!(!harness.state().0.tree.editing_path);
    }
}
