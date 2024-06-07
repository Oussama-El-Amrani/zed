mod outline_panel_settings;

use std::{
    cmp,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use collections::{hash_map, BTreeSet, HashMap, HashSet};
use db::kvp::KEY_VALUE_STORE;
use editor::{
    items::{entry_git_aware_label_color, entry_label_color},
    scroll::ScrollAnchor,
    Editor, EditorEvent, ExcerptId,
};
use file_icons::FileIcons;
use git::repository::GitFileStatus;
use gpui::{
    actions, anchored, deferred, div, px, relative, uniform_list, Action, AppContext, AssetSource,
    AsyncWindowContext, ClipboardItem, DismissEvent, Div, ElementId, EntityId, EventEmitter,
    FocusHandle, FocusableView, FontStyle, FontWeight, HighlightStyle, InteractiveElement,
    IntoElement, KeyContext, Model, MouseButton, MouseDownEvent, ParentElement, Pixels, Point,
    Render, SharedString, Stateful, Styled, StyledText, Subscription, Task, TextStyle,
    UniformListScrollHandle, View, ViewContext, VisualContext, WeakView, WhiteSpace, WindowContext,
};
use language::{BufferId, OffsetRangeExt, OutlineItem, ToOffset};
use menu::{SelectFirst, SelectLast, SelectNext, SelectPrev};

use outline_panel_settings::{OutlinePanelDockPosition, OutlinePanelSettings};
use project::{EntryKind, File, Fs, Project};
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use theme::{color_alpha, ThemeSettings};
use unicase::UniCase;
use util::{maybe, NumericPrefixWithSuffix, ResultExt, TryFutureExt};
use workspace::{
    dock::{DockPosition, Panel, PanelEvent},
    item::ItemHandle,
    ui::{
        h_flex, v_flex, ActiveTheme, Color, ContextMenu, FluentBuilder, Icon, IconName, IconSize,
        Label, LabelCommon, ListItem, Selectable,
    },
    OpenInTerminal, Workspace,
};
use worktree::{Entry, ProjectEntryId, WorktreeId};

actions!(
    outline_panel,
    [
        ExpandSelectedEntry,
        CollapseSelectedEntry,
        CollapseAllEntries,
        CopyPath,
        CopyRelativePath,
        RevealInFinder,
        Open,
        ToggleFocus,
        UnfoldDirectory,
        FoldDirectory,
        SelectParent,
    ]
);

const OUTLINE_PANEL_KEY: &str = "OutlinePanel";

pub struct OutlinePanel {
    fs: Arc<dyn Fs>,
    width: Option<Pixels>,
    project: Model<Project>,
    active: bool,
    scroll_handle: UniformListScrollHandle,
    context_menu: Option<(View<ContextMenu>, Point<Pixels>, Subscription)>,
    focus_handle: FocusHandle,
    pending_serialization: Task<Option<()>>,
    depth_map: Vec<usize>,
    visible_entries: Vec<OutlinePanelEntry>,
    collapsed_dir_ids: HashMap<WorktreeId, BTreeSet<ProjectEntryId>>,
    // Currently selected entry in a file tree
    selected_entry: Option<OutlinePanelEntry>,
    displayed_item: Option<DisplayedActiveItem>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum OutlinesContainer {
    ExternalFile(BufferId),
    WorktreeFile(WorktreeId, ProjectEntryId),
}

#[derive(Clone, Debug, Eq, Hash)]
enum OutlinePanelEntry {
    ExternalFile(BufferId),
    Directory(WorktreeId, Entry),
    File(WorktreeId, Entry),
    Outline(OutlineItem<language::Anchor>),
}

impl PartialEq for OutlinePanelEntry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ExternalFile(id_a), Self::ExternalFile(id_b)) => id_a == id_b,
            (Self::Directory(id_a, entry_a), Self::Directory(id_b, entry_b)) => {
                id_a == id_b && entry_a.id == entry_b.id
            }
            (Self::File(worktree_a, entry_a), Self::File(worktree_b, entry_b)) => {
                worktree_a == worktree_b && entry_a.id == entry_b.id
            }
            (Self::Outline(item_a), Self::Outline(item_b)) => item_a == item_b,
            _ => false,
        }
    }
}

impl OutlinePanelEntry {
    fn abs_path(&self, project: &Model<Project>, cx: &AppContext) -> Option<PathBuf> {
        match self {
            Self::ExternalFile(buffer_id) => project
                .read(cx)
                .buffer_for_id(*buffer_id)
                .and_then(|buffer| File::from_dyn(buffer.read(cx).file()))
                .and_then(|file| file.worktree.read(cx).absolutize(&file.path).ok()),
            Self::Directory(worktree_id, entry) => project
                .read(cx)
                .worktree_for_id(*worktree_id, cx)?
                .read(cx)
                .absolutize(&entry.path)
                .ok(),
            Self::File(worktree_id, entry) => project
                .read(cx)
                .worktree_for_id(*worktree_id, cx)?
                .read(cx)
                .absolutize(&entry.path)
                .ok(),
            Self::Outline(..) => None,
        }
    }

    fn relative_path<'a>(
        &'a self,
        project: &Model<Project>,
        cx: &'a AppContext,
    ) -> Option<&'a Path> {
        match self {
            Self::ExternalFile(buffer_id) => project
                .read(cx)
                .buffer_for_id(*buffer_id)
                .and_then(|buffer| buffer.read(cx).file())
                .map(|file| file.path().as_ref()),
            Self::Directory(_, entry) => Some(entry.path.as_ref()),
            Self::File(_, entry) => Some(entry.path.as_ref()),
            Self::Outline(..) => None,
        }
    }

    fn outlines_container(&self) -> Option<OutlinesContainer> {
        match self {
            Self::ExternalFile(buffer_id) => Some(OutlinesContainer::ExternalFile(*buffer_id)),
            Self::File(worktree_id, entry) => {
                Some(OutlinesContainer::WorktreeFile(*worktree_id, entry.id))
            }
            Self::Directory(..) => None,
            Self::Outline(..) => None,
        }
    }
}

struct DisplayedActiveItem {
    item_id: EntityId,
    active_editor: WeakView<Editor>,
    _editor_subscrpiption: Option<Subscription>,
}

#[derive(Debug)]
pub enum Event {
    Focus,
}

#[derive(Serialize, Deserialize)]
struct SerializedOutlinePanel {
    width: Option<Pixels>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct EntryDetails {
    filename: String,
    icon: Option<Arc<str>>,
    path: Arc<Path>,
    depth: usize,
    kind: EntryKind,
    is_ignored: bool,
    is_expanded: bool,
    is_selected: bool,
    git_status: Option<GitFileStatus>,
    is_private: bool,
    worktree_id: WorktreeId,
    canonical_path: Option<PathBuf>,
}

pub fn init_settings(cx: &mut AppContext) {
    OutlinePanelSettings::register(cx);
}

pub fn init(assets: impl AssetSource, cx: &mut AppContext) {
    init_settings(cx);
    file_icons::init(assets, cx);

    cx.observe_new_views(|workspace: &mut Workspace, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, cx| {
            workspace.toggle_panel_focus::<OutlinePanel>(cx);
        });
    })
    .detach();
}

impl OutlinePanel {
    pub async fn load(
        workspace: WeakView<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<View<Self>> {
        let serialized_panel = cx
            .background_executor()
            .spawn(async move { KEY_VALUE_STORE.read_kvp(OUTLINE_PANEL_KEY) })
            .await
            .context("loading outline panel")
            .log_err()
            .flatten()
            .map(|panel| serde_json::from_str::<SerializedOutlinePanel>(&panel))
            .transpose()
            .log_err()
            .flatten();

        workspace.update(&mut cx, |workspace, cx| {
            let panel = Self::new(workspace, cx);
            if let Some(serialized_panel) = serialized_panel {
                panel.update(cx, |panel, cx| {
                    panel.width = serialized_panel.width.map(|px| px.round());
                    cx.notify();
                });
            }
            panel
        })
    }

    fn new(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) -> View<Self> {
        let project = workspace.project().clone();
        let outline_panel = cx.new_view(|cx| {
            let focus_handle = cx.focus_handle();
            let focus_subscription = cx.on_focus(&focus_handle, Self::focus_in);
            let workspace_subscription = cx.subscribe(
                &workspace
                    .weak_handle()
                    .upgrade()
                    .expect("have a &mut Workspace"),
                move |outline_panel, workspace, event, cx| {
                    if let workspace::Event::ActiveItemChanged = event {
                        if let Some(new_active_editor) = workspace
                            .read(cx)
                            .active_item(cx)
                            .and_then(|item| item.act_as::<Editor>(cx))
                        {
                            let active_editor_updated = outline_panel
                                .displayed_item
                                .as_ref()
                                .map_or(true, |displayed_item| {
                                    displayed_item.item_id != new_active_editor.item_id()
                                });
                            if active_editor_updated {
                                outline_panel.replace_visible_entries(new_active_editor, cx);
                            }
                        } else {
                            outline_panel.displayed_item = None;
                            outline_panel.visible_entries.clear();
                            outline_panel.depth_map.clear();
                            cx.notify();
                        }
                    }
                },
            );

            let icons_subscription = cx.observe_global::<FileIcons>(|_, cx| {
                cx.notify();
            });

            let mut outline_panel_settings = *OutlinePanelSettings::get_global(cx);
            let settings_subscription = cx.observe_global::<SettingsStore>(move |_, cx| {
                let new_settings = *OutlinePanelSettings::get_global(cx);
                if outline_panel_settings != new_settings {
                    outline_panel_settings = new_settings;
                    cx.notify();
                }
            });

            let mut outline_panel = Self {
                active: false,
                project: project.clone(),
                fs: workspace.app_state().fs.clone(),
                scroll_handle: UniformListScrollHandle::new(),
                focus_handle,
                visible_entries: Vec::new(),
                depth_map: Vec::new(),
                collapsed_dir_ids: HashMap::default(),
                selected_entry: None,
                context_menu: None,
                width: None,
                pending_serialization: Task::ready(None),
                displayed_item: None,
                _subscriptions: vec![
                    settings_subscription,
                    icons_subscription,
                    focus_subscription,
                    workspace_subscription,
                ],
            };
            if let Some(editor) = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
            {
                outline_panel.update_visible_entries(&editor, HashSet::default(), None, cx);
            }
            outline_panel
        });

        outline_panel
    }

    fn serialize(&mut self, cx: &mut ViewContext<Self>) {
        let width = self.width;
        self.pending_serialization = cx.background_executor().spawn(
            async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        OUTLINE_PANEL_KEY.into(),
                        serde_json::to_string(&SerializedOutlinePanel { width })?,
                    )
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    fn dispatch_context(&self, _: &ViewContext<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("OutlinePanel");
        dispatch_context.add("menu");
        dispatch_context
    }

    fn unfold_directory(&mut self, _: &UnfoldDirectory, cx: &mut ViewContext<Self>) {
        let Some(editor) = self
            .displayed_item
            .as_ref()
            .and_then(|item| item.active_editor.upgrade())
        else {
            return;
        };
        if let Some(OutlinePanelEntry::Directory(_, _selected_entry)) = &self.selected_entry {
            self.update_visible_entries(&editor, HashSet::default(), None, cx);
            self.autoscroll(cx);
            cx.notify();
        }
    }

    fn fold_directory(&mut self, _: &FoldDirectory, cx: &mut ViewContext<Self>) {
        if let Some(_selected_dir @ OutlinePanelEntry::Directory(..)) = &self.selected_entry {
            let Some(editor) = self
                .displayed_item
                .as_ref()
                .and_then(|item| item.active_editor.upgrade())
            else {
                return;
            };

            self.update_visible_entries(&editor, HashSet::default(), None, cx);
            self.autoscroll(cx);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, cx: &mut ViewContext<Self>) {
        if let Some(selection) = &self.selected_entry {
            let mut entries = self
                .visible_entries
                .iter()
                .skip_while(|entry| entry != &selection)
                .fuse();
            let _current_entry = entries.next();
            if let Some(next_entry) = entries.next() {
                self.selected_entry = Some(next_entry.clone());
                self.autoscroll(cx);
                cx.notify();
            }
        } else {
            self.select_first(&SelectFirst {}, cx);
        }
    }

    fn select_parent(&mut self, _: &SelectParent, cx: &mut ViewContext<Self>) {
        if let Some(selection) = &self.selected_entry {
            let parent_entry = self
                .visible_entries
                .iter()
                .rev()
                .skip_while(|entry| entry != &selection)
                .skip(1)
                .find(|entry| match selection {
                    OutlinePanelEntry::ExternalFile(..) => false,
                    OutlinePanelEntry::Directory(worktree_id, child_directory_entry) => {
                        if let OutlinePanelEntry::Directory(
                            directory_worktree_id,
                            directory_entry,
                        ) = entry
                        {
                            directory_worktree_id == worktree_id
                                && directory_contains(directory_entry, child_directory_entry)
                        } else {
                            false
                        }
                    }
                    OutlinePanelEntry::File(worktree_id, file_entry) => {
                        if let OutlinePanelEntry::Directory(
                            directory_worktree_id,
                            directory_entry,
                        ) = entry
                        {
                            directory_worktree_id == worktree_id
                                && directory_contains(directory_entry, file_entry)
                        } else {
                            false
                        }
                    }
                    OutlinePanelEntry::Outline(..) => matches!(
                        entry,
                        OutlinePanelEntry::File(..) | OutlinePanelEntry::ExternalFile(..)
                    ),
                });

            if let Some(parent_entry) = parent_entry {
                self.selected_entry = Some(parent_entry.clone());
                self.autoscroll(cx);
                cx.notify();
            }
        } else {
            self.select_first(&SelectFirst {}, cx);
        }
    }

    fn select_first(&mut self, _: &SelectFirst, cx: &mut ViewContext<Self>) {
        if let Some(first_entry) = self.visible_entries.first() {
            self.selected_entry = Some(first_entry.clone());
            self.autoscroll(cx);
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, cx: &mut ViewContext<Self>) {
        if let Some(last_entry) = self.visible_entries.last() {
            self.selected_entry = Some(last_entry.clone());
            self.autoscroll(cx);
            cx.notify();
        }
    }

    fn autoscroll(&mut self, cx: &mut ViewContext<Self>) {
        if let Some(selected_entry) = &self.selected_entry {
            if let Some(index) = self
                .visible_entries
                .iter()
                .position(|entry| entry == selected_entry)
            {
                self.scroll_handle.scroll_to_item(index);
                cx.notify();
            }
        }
    }

    fn focus_in(&mut self, cx: &mut ViewContext<Self>) {
        if !self.focus_handle.contains_focused(cx) {
            cx.emit(Event::Focus);
        }
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        entry: &OutlinePanelEntry,
        cx: &mut ViewContext<Self>,
    ) {
        let project = self.project.read(cx);

        self.selected_entry = Some(entry.clone());
        let auto_fold_dirs = OutlinePanelSettings::get_global(cx).auto_fold_dirs;
        let is_foldable = auto_fold_dirs && self.is_foldable(entry);
        let is_unfoldable = auto_fold_dirs && self.is_unfoldable(entry);
        let is_read_only = project.is_read_only();

        let context_menu = ContextMenu::build(cx, |menu, _| {
            menu.context(self.focus_handle.clone()).when_else(
                is_read_only,
                |menu| menu.action("Copy Relative Path", Box::new(CopyRelativePath)),
                |menu| {
                    menu.action("Reveal in Finder", Box::new(RevealInFinder))
                        .action("Open in Terminal", Box::new(OpenInTerminal))
                        .when(is_unfoldable, |menu| {
                            menu.action("Unfold Directory", Box::new(UnfoldDirectory))
                        })
                        .when(is_foldable, |menu| {
                            menu.action("Fold Directory", Box::new(FoldDirectory))
                        })
                        .separator()
                        .action("Copy Path", Box::new(CopyPath))
                        .action("Copy Relative Path", Box::new(CopyRelativePath))
                },
            )
        });

        cx.focus_view(&context_menu);
        let subscription = cx.subscribe(&context_menu, |outline_panel, _, _: &DismissEvent, cx| {
            outline_panel.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn is_unfoldable(&self, _entry: &OutlinePanelEntry) -> bool {
        false
    }

    fn is_foldable(&self, _entry: &OutlinePanelEntry) -> bool {
        false
    }

    fn expand_selected_entry(&mut self, _: &ExpandSelectedEntry, cx: &mut ViewContext<Self>) {
        let Some(editor) = self
            .displayed_item
            .as_ref()
            .and_then(|item| item.active_editor.upgrade())
        else {
            return;
        };
        if let Some(OutlinePanelEntry::Directory(worktree_id, selected_dir_entry)) =
            &self.selected_entry
        {
            let expanded = self
                .collapsed_dir_ids
                .get_mut(worktree_id)
                .map_or(false, |hidden_dirs| {
                    hidden_dirs.remove(&selected_dir_entry.id)
                });
            if expanded {
                self.project.update(cx, |project, cx| {
                    project.expand_entry(*worktree_id, selected_dir_entry.id, cx);
                });
                self.update_visible_entries(&editor, HashSet::default(), None, cx);
                cx.notify()
            } else {
                self.select_next(&SelectNext, cx)
            }
        }
    }

    fn collapse_selected_entry(&mut self, _: &CollapseSelectedEntry, cx: &mut ViewContext<Self>) {
        let Some(editor) = self
            .displayed_item
            .as_ref()
            .and_then(|item| item.active_editor.upgrade())
        else {
            return;
        };
        if let Some(dir_entry @ OutlinePanelEntry::Directory(worktree_id, selected_dir_entry)) =
            &self.selected_entry
        {
            self.collapsed_dir_ids
                .entry(*worktree_id)
                .or_default()
                .insert(selected_dir_entry.id);
            self.update_visible_entries(&editor, HashSet::default(), Some(dir_entry.clone()), cx);
            cx.notify();
        }
    }

    pub fn collapse_all_entries(&mut self, _: &CollapseAllEntries, cx: &mut ViewContext<Self>) {
        let Some(editor) = self
            .displayed_item
            .as_ref()
            .and_then(|item| item.active_editor.upgrade())
        else {
            return;
        };

        self.depth_map
            .iter()
            .enumerate()
            .filter(|(_, depth)| depth == &&0)
            .filter_map(|(i, _)| self.visible_entries.get(i))
            .filter_map(|entry| match entry {
                OutlinePanelEntry::Directory(worktree_id, dir_entry) => {
                    Some((*worktree_id, dir_entry))
                }
                _ => None,
            })
            .for_each(|(worktree_id, dir_entry)| {
                self.collapsed_dir_ids
                    .entry(worktree_id)
                    .or_default()
                    .insert(dir_entry.id);
            });
        self.update_visible_entries(&editor, HashSet::default(), None, cx);
        cx.notify();
    }

    fn toggle_expanded(&mut self, entry_id: ProjectEntryId, cx: &mut ViewContext<Self>) {
        let editor = self
            .displayed_item
            .as_ref()
            .and_then(|item| item.active_editor.upgrade());
        let worktree_id = self.project.read(cx).worktree_id_for_entry(entry_id, cx);
        let dir_entry_to_toggle = self.visible_entries.iter().find(|entry| {
            if let OutlinePanelEntry::Directory(directory_worktree_id, directory_entry) = entry {
                Some(directory_worktree_id) == worktree_id.as_ref()
                    && directory_entry.id == entry_id
            } else {
                false
            }
        });
        let Some(((editor, worktree_id), dir_entry_to_toggle)) =
            editor.zip(worktree_id).zip(dir_entry_to_toggle)
        else {
            return;
        };

        match self.collapsed_dir_ids.entry(worktree_id) {
            hash_map::Entry::Occupied(mut o) => {
                let collapsed_dir_ids = o.get_mut();
                if collapsed_dir_ids.remove(&entry_id) {
                    self.project
                        .update(cx, |project, cx| {
                            project.expand_entry(worktree_id, entry_id, cx)
                        })
                        .unwrap_or_else(|| Task::ready(Ok(())))
                        .detach_and_log_err(cx);
                } else {
                    collapsed_dir_ids.insert(entry_id);
                }
            }
            hash_map::Entry::Vacant(v) => {
                v.insert(BTreeSet::new()).insert(entry_id);
            }
        }

        self.update_visible_entries(
            &editor,
            HashSet::default(),
            Some(dir_entry_to_toggle.clone()),
            cx,
        );
        cx.focus(&self.focus_handle);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrev, cx: &mut ViewContext<Self>) {
        if let Some(selection) = &self.selected_entry {
            if let Some(previous_entry) = self
                .visible_entries
                .iter()
                .rev()
                .skip_while(|entry| entry != &selection)
                .skip(1)
                .next()
            {
                self.selected_entry = Some(previous_entry.clone());
                self.autoscroll(cx);
                cx.notify();
            }
        } else {
            self.select_first(&SelectFirst {}, cx);
        }
    }

    fn copy_path(&mut self, _: &CopyPath, cx: &mut ViewContext<Self>) {
        if let Some(clipboard_text) = self.selected_entry.as_ref().and_then(|entry| {
            entry
                .abs_path(&self.project, cx)
                .map(|p| p.to_string_lossy().to_string())
        }) {
            cx.write_to_clipboard(ClipboardItem::new(clipboard_text));
        }
    }

    fn copy_relative_path(&mut self, _: &CopyRelativePath, cx: &mut ViewContext<Self>) {
        if let Some(clipboard_text) = self.selected_entry.as_ref().and_then(|entry| {
            entry
                .relative_path(&self.project, cx)
                .map(|p| p.to_string_lossy().to_string())
        }) {
            cx.write_to_clipboard(ClipboardItem::new(clipboard_text));
        }
    }

    fn reveal_in_finder(&mut self, _: &RevealInFinder, cx: &mut ViewContext<Self>) {
        if let Some(abs_path) = self
            .selected_entry
            .as_ref()
            .and_then(|entry| entry.abs_path(&self.project, cx))
        {
            cx.reveal_path(&abs_path);
        }
    }

    fn open_in_terminal(&mut self, _: &OpenInTerminal, cx: &mut ViewContext<Self>) {
        if let Some((selected_entry, abs_path)) = self
            .selected_entry
            .as_ref()
            .and_then(|entry| Some((entry, entry.abs_path(&self.project, cx)?)))
        {
            let working_directory = match selected_entry {
                OutlinePanelEntry::File(..) | OutlinePanelEntry::ExternalFile(..) => {
                    abs_path.parent().map(|p| p.to_owned())
                }
                OutlinePanelEntry::Directory(..) => Some(abs_path),
                OutlinePanelEntry::Outline(..) => None,
            };
            if let Some(working_directory) = working_directory {
                cx.dispatch_action(workspace::OpenTerminal { working_directory }.boxed_clone())
            }
        }
    }

    fn reveal_entry_for_selection(
        &mut self,
        editor: &View<Editor>,
        cx: &mut ViewContext<'_, Self>,
    ) {
        let Some((outlines_container, outline_item)) = location_for_editor_selection(editor, cx)
        else {
            return;
        };

        let file_entry_to_expand = match &outline_item {
            Some(outline_item) => self
                .visible_entries
                .iter()
                .rev()
                .skip_while(|entry| {
                    if let OutlinePanelEntry::Outline(visible_outline_item) = entry {
                        visible_outline_item != outline_item
                    } else {
                        true
                    }
                })
                .skip(1)
                .find(|entry| match (entry, &outlines_container) {
                    (
                        OutlinePanelEntry::ExternalFile(buffer_id),
                        OutlinesContainer::ExternalFile(container_buffer_id),
                    ) => buffer_id == container_buffer_id,
                    (
                        OutlinePanelEntry::File(file_worktree_id, file_entry),
                        OutlinesContainer::WorktreeFile(worktree_id, id),
                    ) => file_worktree_id == worktree_id && &file_entry.id == id,
                    _ => false,
                }),
            None => self
                .visible_entries
                .iter()
                .find(|entry| match (entry, &outlines_container) {
                    (
                        OutlinePanelEntry::ExternalFile(buffer_id),
                        OutlinesContainer::ExternalFile(container_buffer_id),
                    ) => buffer_id == container_buffer_id,
                    (
                        OutlinePanelEntry::File(file_worktree_id, file_entry),
                        OutlinesContainer::WorktreeFile(worktree_id, id),
                    ) => file_worktree_id == worktree_id && &file_entry.id == id,
                    _ => false,
                }),
        };
        let Some(entry_to_select) = outline_item
            .map(|outline| OutlinePanelEntry::Outline(outline))
            .or_else(|| file_entry_to_expand.cloned())
        else {
            return;
        };

        if self.selected_entry.as_ref() == Some(&entry_to_select) {
            return;
        }

        if let Some(OutlinePanelEntry::File(file_worktree_id, file_entry)) = file_entry_to_expand {
            if let Some(worktree) = self.project.read(cx).worktree_for_id(*file_worktree_id, cx) {
                let parent_entry = {
                    let mut traversal = worktree.read(cx).traverse_from_path(
                        true,
                        true,
                        true,
                        file_entry.path.as_ref(),
                    );
                    if traversal.back_to_parent() {
                        traversal.entry()
                    } else {
                        None
                    }
                    .cloned()
                };
                if let Some(directory_entry) = parent_entry {
                    self.expand_entry(worktree.read(cx).id(), directory_entry.id, cx);
                }
            }
        }

        self.update_visible_entries(&editor, HashSet::default(), Some(entry_to_select), cx);
        self.autoscroll(cx);
        cx.notify();
    }

    fn expand_entry(
        &mut self,
        worktree_id: WorktreeId,
        entry_id: ProjectEntryId,
        cx: &mut AppContext,
    ) {
        if let Some(collapsed_dir_ids) = self.collapsed_dir_ids.get_mut(&worktree_id) {
            if collapsed_dir_ids.remove(&entry_id) {
                self.project
                    .update(cx, |project, cx| {
                        project.expand_entry(worktree_id, entry_id, cx)
                    })
                    .unwrap_or_else(|| Task::ready(Ok(())))
                    .detach_and_log_err(cx)
            }
        }
    }

    fn render_entry(
        &self,
        rendered_entry: &OutlinePanelEntry,
        depth: usize,
        cx: &mut ViewContext<Self>,
    ) -> Stateful<Div> {
        let settings = OutlinePanelSettings::get_global(cx);
        let is_active = self.selected_entry.as_ref() == Some(rendered_entry);
        let (item_id, label_element, icon) = match rendered_entry {
            OutlinePanelEntry::File(worktree_id, entry) => {
                let name = self.entry_name(worktree_id, entry, cx);
                let color =
                    entry_git_aware_label_color(entry.git_status, entry.is_ignored, is_active);
                let icon = if settings.file_icons {
                    FileIcons::get_icon(&entry.path, cx)
                } else {
                    None
                }
                .map(Icon::from_path)
                .map(|icon| icon.color(color));
                (
                    ElementId::from(entry.id.to_proto() as usize),
                    Label::new(name)
                        .single_line()
                        .color(color)
                        .into_any_element(),
                    icon,
                )
            }
            OutlinePanelEntry::Directory(worktree_id, entry) => {
                let name = self.entry_name(worktree_id, entry, cx);

                let is_expanded = self
                    .collapsed_dir_ids
                    .get(worktree_id)
                    .map_or(true, |ids| !ids.contains(&entry.id));
                let color =
                    entry_git_aware_label_color(entry.git_status, entry.is_ignored, is_active);
                let icon = if settings.folder_icons {
                    FileIcons::get_folder_icon(is_expanded, cx)
                } else {
                    FileIcons::get_chevron_icon(is_expanded, cx)
                }
                .map(Icon::from_path)
                .map(|icon| icon.color(color));
                (
                    ElementId::from(entry.id.to_proto() as usize),
                    Label::new(name)
                        .single_line()
                        .color(color)
                        .into_any_element(),
                    icon,
                )
            }
            OutlinePanelEntry::ExternalFile(buffer_id) => {
                let color = entry_label_color(is_active);
                let (icon, name) = match self.project.read(cx).buffer_for_id(*buffer_id) {
                    Some(buffer) => match buffer.read(cx).file() {
                        Some(file) => {
                            let path = file.path();
                            let icon = if settings.file_icons {
                                FileIcons::get_icon(path.as_ref(), cx)
                            } else {
                                None
                            }
                            .map(Icon::from_path)
                            .map(|icon| icon.color(color));
                            (icon, file_name(path.as_ref()))
                        }
                        None => (None, "Untitled".to_string()),
                    },
                    None => (None, "Unknown buffer".to_string()),
                };
                (
                    ElementId::from(buffer_id.to_proto() as usize),
                    Label::new(name)
                        .single_line()
                        .color(color)
                        .into_any_element(),
                    icon,
                )
            }
            OutlinePanelEntry::Outline(outline) => {
                // TODO kb deduplicate
                let settings = ThemeSettings::get_global(cx);
                let text_style = TextStyle {
                    color: cx.theme().colors().text,
                    font_family: settings.buffer_font.family.clone(),
                    font_features: settings.buffer_font.features.clone(),
                    font_size: settings.buffer_font_size(cx).into(),
                    font_weight: FontWeight::NORMAL,
                    font_style: FontStyle::Normal,
                    line_height: relative(1.),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    white_space: WhiteSpace::Normal,
                };

                let mut highlight_style = HighlightStyle::default();
                highlight_style.background_color =
                    Some(color_alpha(cx.theme().colors().text_accent, 0.3));

                (
                    ElementId::from(SharedString::from(format!(
                        "{:?}|{}",
                        outline
                            .range
                            .start
                            .buffer_id
                            .or(outline.range.end.buffer_id),
                        &outline.text,
                    ))),
                    StyledText::new(outline.text.clone())
                        .with_highlights(&text_style, outline.highlight_ranges.clone())
                        .into_any_element(),
                    None,
                )
            }
        };

        let clicked_entry = rendered_entry.clone();
        div()
            .id(item_id.clone())
            .child(
                ListItem::new(item_id)
                    .indent_level(depth)
                    .indent_step_size(px(settings.indent_size))
                    .selected(is_active)
                    .child(if let Some(icon) = icon {
                        h_flex().child(icon)
                    } else {
                        h_flex()
                            .size(IconSize::default().rems())
                            .invisible()
                            .flex_none()
                    })
                    .child(h_flex().h_6().child(label_element).ml_1())
                    .on_click(
                        cx.listener(move |outline_panel, event: &gpui::ClickEvent, cx| {
                            if event.down.button == MouseButton::Right || event.down.first_mouse {
                                return;
                            }

                            let Some(active_editor) = outline_panel
                                .displayed_item
                                .as_ref()
                                .and_then(|item| item.active_editor.upgrade())
                            else {
                                return;
                            };
                            let active_multi_buffer = active_editor.read(cx).buffer().clone();
                            let multi_buffer_snapshot = active_multi_buffer.read(cx).snapshot(cx);

                            match &clicked_entry {
                                OutlinePanelEntry::ExternalFile(buffer_id) => {
                                    let scroll_target = multi_buffer_snapshot.excerpts().find_map(
                                        |(excerpt_id, buffer_snapshot, excerpt_range)| {
                                            if &buffer_snapshot.remote_id() == buffer_id {
                                                multi_buffer_snapshot.anchor_in_excerpt(
                                                    excerpt_id,
                                                    excerpt_range.context.start,
                                                )
                                            } else {
                                                None
                                            }
                                        },
                                    );
                                    if let Some(anchor) = scroll_target {
                                        outline_panel.selected_entry = Some(clicked_entry.clone());
                                        active_editor.update(cx, |editor, cx| {
                                            editor.set_scroll_anchor(
                                                ScrollAnchor {
                                                    offset: Point::new(
                                                        0.0,
                                                        -(editor.file_header_size() as f32),
                                                    ),
                                                    anchor,
                                                },
                                                cx,
                                            );
                                        })
                                    }
                                }
                                OutlinePanelEntry::Directory(_, directory_entry) => {
                                    outline_panel.toggle_expanded(directory_entry.id, cx)
                                }
                                OutlinePanelEntry::File(_, file_entry) => {
                                    let scroll_target = outline_panel
                                        .project
                                        .update(cx, |project, cx| {
                                            project
                                                .path_for_entry(file_entry.id, cx)
                                                .and_then(|path| project.get_open_buffer(&path, cx))
                                        })
                                        .map(|buffer| {
                                            active_multi_buffer
                                                .read(cx)
                                                .excerpts_for_buffer(&buffer, cx)
                                        })
                                        .and_then(|excerpts| {
                                            let (excerpt_id, excerpt_range) = excerpts.first()?;
                                            multi_buffer_snapshot.anchor_in_excerpt(
                                                *excerpt_id,
                                                excerpt_range.context.start,
                                            )
                                        });
                                    if let Some(anchor) = scroll_target {
                                        outline_panel.selected_entry = Some(clicked_entry.clone());
                                        active_editor.update(cx, |editor, cx| {
                                            editor.set_scroll_anchor(
                                                ScrollAnchor {
                                                    offset: Point::new(
                                                        0.0,
                                                        -(editor.file_header_size() as f32),
                                                    ),
                                                    anchor,
                                                },
                                                cx,
                                            );
                                        })
                                    }
                                }
                                OutlinePanelEntry::Outline(outline) => {
                                    let Some(full_buffer_snapshot) = outline
                                        .range
                                        .start
                                        .buffer_id
                                        .and_then(|buffer_id| {
                                            active_multi_buffer.read(cx).buffer(buffer_id)
                                        })
                                        .or_else(|| {
                                            outline.range.end.buffer_id.and_then(|buffer_id| {
                                                active_multi_buffer.read(cx).buffer(buffer_id)
                                            })
                                        })
                                        .map(|buffer| buffer.read(cx).snapshot())
                                    else {
                                        return;
                                    };
                                    let outline_offset_range =
                                        outline.range.to_offset(&full_buffer_snapshot);
                                    let scroll_target = multi_buffer_snapshot
                                        .excerpts()
                                        .filter(|(_, buffer_snapshot, _)| {
                                            let buffer_id = buffer_snapshot.remote_id();
                                            Some(buffer_id) == outline.range.start.buffer_id
                                                || Some(buffer_id) == outline.range.end.buffer_id
                                        })
                                        .min_by_key(|(_, _, excerpt_range)| {
                                            let excerpt_offeset_range = excerpt_range
                                                .context
                                                .to_offset(&full_buffer_snapshot);
                                            ((outline_offset_range.start / 2
                                                + outline_offset_range.end / 2)
                                                as isize
                                                - (excerpt_offeset_range.start / 2
                                                    + excerpt_offeset_range.end / 2)
                                                    as isize)
                                                .abs()
                                        })
                                        .and_then(
                                            |(excerpt_id, excerpt_snapshot, excerpt_range)| {
                                                let location = if outline
                                                    .range
                                                    .start
                                                    .is_valid(excerpt_snapshot)
                                                {
                                                    outline.range.start
                                                } else {
                                                    excerpt_range.context.start
                                                };
                                                multi_buffer_snapshot
                                                    .anchor_in_excerpt(excerpt_id, location)
                                            },
                                        );
                                    if let Some(anchor) = scroll_target {
                                        outline_panel.selected_entry = Some(clicked_entry.clone());
                                        active_editor.update(cx, |editor, cx| {
                                            editor.set_scroll_anchor(
                                                ScrollAnchor {
                                                    offset: Point::default(),
                                                    anchor,
                                                },
                                                cx,
                                            );
                                        })
                                    }
                                }
                            }
                        }),
                    )
                    .on_secondary_mouse_down(cx.listener(
                        move |outline_panel, event: &MouseDownEvent, cx| {
                            // Stop propagation to prevent the catch-all context menu for the project
                            // panel from being deployed.
                            cx.stop_propagation();
                            if let Some(selection) = outline_panel
                                .selected_entry
                                .as_ref()
                                .or_else(|| outline_panel.visible_entries.last())
                                .cloned()
                            {
                                outline_panel.deploy_context_menu(event.position, &selection, cx);
                            }
                        },
                    )),
            )
            .border_1()
            .border_r_2()
            .rounded_none()
            .hover(|style| {
                if is_active {
                    style
                } else {
                    let hover_color = cx.theme().colors().ghost_element_hover;
                    style.bg(hover_color).border_color(hover_color)
                }
            })
            .when(is_active && self.focus_handle.contains_focused(cx), |div| {
                div.border_color(Color::Selected.color(cx))
            })
    }

    fn entry_name(
        &self,
        worktree_id: &WorktreeId,
        entry: &Entry,
        cx: &ViewContext<OutlinePanel>,
    ) -> String {
        let name = match self.project.read(cx).worktree_for_id(*worktree_id, cx) {
            Some(worktree) => {
                let worktree = worktree.read(cx);
                match worktree.snapshot().root_entry() {
                    Some(root_entry) => {
                        if root_entry.id == entry.id {
                            file_name(worktree.abs_path().as_ref())
                        } else {
                            let path = worktree.absolutize(entry.path.as_ref()).ok();
                            let path = path.as_deref().unwrap_or_else(|| entry.path.as_ref());
                            file_name(path)
                        }
                    }
                    None => {
                        let path = worktree.absolutize(entry.path.as_ref()).ok();
                        let path = path.as_deref().unwrap_or_else(|| entry.path.as_ref());
                        file_name(path)
                    }
                }
            }
            None => file_name(entry.path.as_ref()),
        };
        name
    }

    // TODO kb make async, run in the background + debounce
    fn update_visible_entries(
        &mut self,
        active_editor: &View<Editor>,
        new_entries: HashSet<ExcerptId>,
        new_selected_entry: Option<OutlinePanelEntry>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Option<()>> {
        if !self.active {
            return Task::ready(None);
        }

        // TODO kb implement
        // OutlinePanelSettings::get_global(cx).auto_fold_dirs
        let displayed_multi_buffer = active_editor.read(cx).buffer().clone();
        let multi_buffer_snapshot = displayed_multi_buffer.read(cx).snapshot(cx);
        let mut outline_entries =
            HashMap::<OutlinesContainer, Vec<OutlineItem<language::Anchor>>>::default();
        let mut processed_outlines = HashSet::default();
        let mut processed_excernal_buffers = HashSet::default();

        let mut new_workspace_entries = HashMap::<WorktreeId, HashSet<&Entry>>::default();
        let mut external_entries = Vec::default();
        for (excerpt_id, buffer_snapshot, excerpt_range) in multi_buffer_snapshot.excerpts() {
            let is_new = new_entries.contains(&excerpt_id);
            let container = match File::from_dyn(buffer_snapshot.file())
                .and_then(|file| Some(file.worktree.read(cx).id()).zip(file.entry_id))
            {
                Some((worktree_id, id)) => OutlinesContainer::WorktreeFile(worktree_id, id),
                None => OutlinesContainer::ExternalFile(buffer_snapshot.remote_id()),
            };
            outline_entries.entry(container).or_default().extend(
                buffer_snapshot
                    .outline(Some(&cx.theme().syntax()))
                    .map(|outline| outline.items)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|outline| {
                        let intersects = range_contains(
                            &excerpt_range.context,
                            outline.range.start,
                            buffer_snapshot,
                        ) || range_contains(
                            &excerpt_range.context,
                            outline.range.end,
                            buffer_snapshot,
                        );
                        intersects && processed_outlines.insert(outline.clone())
                    }),
            );

            if let Some(file) = File::from_dyn(buffer_snapshot.file()) {
                let worktree = file.worktree.read(cx);
                let worktree_snapshot = worktree.snapshot();
                let collapsed_dir_ids = self.collapsed_dir_ids.entry(worktree.id()).or_default();

                match file
                    .entry_id
                    .and_then(|project_id| worktree.entry_for_id(project_id))
                {
                    Some(entry) => {
                        let mut traversal =
                            worktree.traverse_from_path(true, true, true, entry.path.as_ref());

                        let mut entries_to_add = HashMap::<WorktreeId, HashSet<&Entry>>::default();
                        let mut current_entry = entry;
                        loop {
                            if current_entry.is_dir() {
                                if is_new || worktree_snapshot.root_entry() == Some(current_entry) {
                                    collapsed_dir_ids.remove(&current_entry.id);
                                } else if collapsed_dir_ids.contains(&current_entry.id) {
                                    entries_to_add.clear();
                                    entries_to_add
                                        .entry(worktree.id())
                                        .or_default()
                                        // TODO kb displays does not remove folded dir inside a folded dir
                                        .insert(current_entry);
                                    break;
                                }
                            }

                            let new_entry_added = entries_to_add
                                .entry(worktree.id())
                                .or_default()
                                .insert(current_entry);
                            if new_entry_added && traversal.back_to_parent() {
                                if let Some(parent_entry) = traversal.entry() {
                                    current_entry = parent_entry;
                                    continue;
                                }
                            }
                            break;
                        }
                        for (workspace_id, new_entries) in entries_to_add {
                            new_workspace_entries
                                .entry(workspace_id)
                                .or_default()
                                .extend(new_entries);
                        }
                    }
                    None => {
                        if processed_excernal_buffers.insert(buffer_snapshot.remote_id()) {
                            external_entries
                                .push(OutlinePanelEntry::ExternalFile(buffer_snapshot.remote_id()));
                        }
                    }
                }
            } else if processed_excernal_buffers.insert(buffer_snapshot.remote_id()) {
                external_entries.push(OutlinePanelEntry::ExternalFile(buffer_snapshot.remote_id()));
            }
        }

        external_entries.sort_by(|entry_a, entry_b| match (entry_a, entry_b) {
            (
                OutlinePanelEntry::ExternalFile(buffer_id_a),
                OutlinePanelEntry::ExternalFile(buffer_id_b),
            ) => buffer_id_a.cmp(&buffer_id_b),
            (OutlinePanelEntry::ExternalFile(..), _) => cmp::Ordering::Less,
            (_, OutlinePanelEntry::ExternalFile(..)) => cmp::Ordering::Greater,
            _ => cmp::Ordering::Equal,
        });

        let worktree_entries =
            new_workspace_entries
                .into_iter()
                .filter_map(|(worktree_id, entries)| {
                    let worktree_snapshot = self
                        .project
                        .read(cx)
                        .worktree_for_id(worktree_id, cx)?
                        .read(cx)
                        .snapshot();
                    let mut entries = entries.into_iter().cloned().collect::<Vec<_>>();
                    sort_worktree_entries(&mut entries);
                    worktree_snapshot.propagate_git_statuses(&mut entries);
                    Some((worktree_id, entries))
                });

        let mut worktree_items_with_outlines = Vec::new();
        for (worktree_id, entries) in worktree_entries {
            for entry in entries {
                if entry.is_dir() {
                    worktree_items_with_outlines
                        .push(OutlinePanelEntry::Directory(worktree_id, entry))
                } else {
                    worktree_items_with_outlines.push(OutlinePanelEntry::File(worktree_id, entry))
                }
            }
        }

        let mut outline_depth = None;
        let mut depth = 0;
        let mut parent_entry_stack = Vec::new();
        let mut new_depth_map = Vec::new();
        let new_visible_entries = external_entries
            .into_iter()
            .chain(worktree_items_with_outlines.into_iter())
            .flat_map(|entry| {
                let outlines = entry
                    .outlines_container()
                    .and_then(|container| outline_entries.remove(&container));
                Some(entry).into_iter().chain(
                    outlines
                        .unwrap_or_default()
                        .into_iter()
                        .map(OutlinePanelEntry::Outline),
                )
            })
            .filter(|visible_item| {
                match visible_item {
                    OutlinePanelEntry::Directory(_, dir_entry) => {
                        outline_depth = None;
                        while !parent_entry_stack.is_empty()
                            && !dir_entry
                                .path
                                .starts_with(parent_entry_stack.last().unwrap())
                        {
                            parent_entry_stack.pop();
                            depth -= 1;
                        }
                        parent_entry_stack.push(dir_entry.path.clone());
                        new_depth_map.push(depth);
                        depth += 1;
                    }
                    OutlinePanelEntry::File(_, file_entry) => {
                        outline_depth = None::<usize>;
                        while !parent_entry_stack.is_empty()
                            && !file_entry
                                .path
                                .starts_with(parent_entry_stack.last().unwrap())
                        {
                            parent_entry_stack.pop();
                            depth -= 1;
                        }
                        new_depth_map.push(depth);
                    }
                    OutlinePanelEntry::Outline(outline) => {
                        let mut depth = *new_depth_map.last().unwrap_or(&0);
                        if let Some(outline_depth) = outline_depth {
                            match outline_depth.cmp(&outline.depth) {
                                cmp::Ordering::Less => depth += 1,
                                cmp::Ordering::Equal => {}
                                cmp::Ordering::Greater => depth -= 1,
                            };
                        }

                        outline_depth = Some(outline.depth);
                        new_depth_map.push(depth);
                    }
                    OutlinePanelEntry::ExternalFile(..) => {
                        outline_depth = None;
                        depth = 0;
                        parent_entry_stack.clear();
                        new_depth_map.push(depth);
                    }
                }

                true
            })
            .collect();

        self.visible_entries = new_visible_entries;
        self.depth_map = new_depth_map;
        if new_selected_entry.is_some() {
            self.selected_entry = new_selected_entry;
        }

        Task::ready(Some(()))
    }

    fn replace_visible_entries(
        &mut self,
        new_active_editor: View<Editor>,
        cx: &mut ViewContext<Self>,
    ) {
        let new_entries =
            HashSet::from_iter(new_active_editor.read(cx).buffer().read(cx).excerpt_ids());
        self.displayed_item = Some(DisplayedActiveItem {
            item_id: new_active_editor.item_id(),
            _editor_subscrpiption: subscribe_for_editor_events(&new_active_editor, cx),
            active_editor: new_active_editor.downgrade(),
        });
        self.update_visible_entries(&new_active_editor, new_entries, None, cx);
        cx.notify();
    }
}

fn sort_worktree_entries(entries: &mut Vec<Entry>) {
    entries.sort_by(|entry_a, entry_b| {
        let mut components_a = entry_a.path.components().peekable();
        let mut components_b = entry_b.path.components().peekable();
        loop {
            match (components_a.next(), components_b.next()) {
                (Some(component_a), Some(component_b)) => {
                    let a_is_file = components_a.peek().is_none() && entry_a.is_file();
                    let b_is_file = components_b.peek().is_none() && entry_b.is_file();
                    let ordering = a_is_file.cmp(&b_is_file).then_with(|| {
                        let maybe_numeric_ordering = maybe!({
                            let num_and_remainder_a = Path::new(component_a.as_os_str())
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .and_then(NumericPrefixWithSuffix::from_numeric_prefixed_str)?;
                            let num_and_remainder_b = Path::new(component_b.as_os_str())
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .and_then(NumericPrefixWithSuffix::from_numeric_prefixed_str)?;

                            num_and_remainder_a.partial_cmp(&num_and_remainder_b)
                        });

                        maybe_numeric_ordering.unwrap_or_else(|| {
                            let name_a = UniCase::new(component_a.as_os_str().to_string_lossy());
                            let name_b = UniCase::new(component_b.as_os_str().to_string_lossy());

                            name_a.cmp(&name_b)
                        })
                    });
                    if !ordering.is_eq() {
                        return ordering;
                    }
                }
                (Some(_), None) => break cmp::Ordering::Greater,
                (None, Some(_)) => break cmp::Ordering::Less,
                (None, None) => break cmp::Ordering::Equal,
            }
        }
    });
}

fn file_name(path: &Path) -> String {
    let mut current_path = path;
    loop {
        if let Some(file_name) = current_path.file_name() {
            return file_name.to_string_lossy().into_owned();
        }
        match current_path.parent() {
            Some(parent) => current_path = parent,
            None => return path.to_string_lossy().into_owned(),
        }
    }
}

fn directory_contains(directory_entry: &Entry, chld_entry: &Entry) -> bool {
    debug_assert!(directory_entry.is_dir());
    let Some(relative_path) = chld_entry.path.strip_prefix(&directory_entry.path).ok() else {
        return false;
    };
    relative_path.iter().count() == 1
}

impl Panel for OutlinePanel {
    fn persistent_name() -> &'static str {
        "Outline Panel"
    }

    fn position(&self, cx: &WindowContext) -> DockPosition {
        match OutlinePanelSettings::get_global(cx).dock {
            OutlinePanelDockPosition::Left => DockPosition::Left,
            OutlinePanelDockPosition::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, cx: &mut ViewContext<Self>) {
        settings::update_settings_file::<OutlinePanelSettings>(
            self.fs.clone(),
            cx,
            move |settings| {
                let dock = match position {
                    DockPosition::Left | DockPosition::Bottom => OutlinePanelDockPosition::Left,
                    DockPosition::Right => OutlinePanelDockPosition::Right,
                };
                settings.dock = Some(dock);
            },
        );
    }

    fn size(&self, cx: &WindowContext) -> Pixels {
        self.width
            .unwrap_or_else(|| OutlinePanelSettings::get_global(cx).default_width)
    }

    fn set_size(&mut self, size: Option<Pixels>, cx: &mut ViewContext<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, cx: &WindowContext) -> Option<IconName> {
        OutlinePanelSettings::get_global(cx)
            .button
            .then(|| IconName::ListTree)
    }

    fn icon_tooltip(&self, _: &WindowContext) -> Option<&'static str> {
        Some("Outline Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &WindowContext) -> bool {
        self.displayed_item.is_some()
    }

    fn set_active(&mut self, active: bool, cx: &mut ViewContext<Self>) {
        let old_active = self.active;
        self.active = active;
        if active && old_active != active {
            if let Some(active_editor) = self
                .displayed_item
                .as_ref()
                .and_then(|item| item.active_editor.upgrade())
            {
                self.replace_visible_entries(active_editor, cx);
            }
        }
    }
}

impl FocusableView for OutlinePanel {
    fn focus_handle(&self, _cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<Event> for OutlinePanel {}

impl EventEmitter<PanelEvent> for OutlinePanel {}

impl Render for OutlinePanel {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let project = self.project.read(cx);
        if self.visible_entries.is_empty() {
            v_flex()
                .id("empty-outline_panel")
                .size_full()
                .p_4()
                .track_focus(&self.focus_handle)
                .child(Label::new("No editor outlines available"))
        } else {
            h_flex()
                .id("outline-panel")
                .size_full()
                .relative()
                .key_context(self.dispatch_context(cx))
                .on_action(cx.listener(Self::select_next))
                .on_action(cx.listener(Self::select_prev))
                .on_action(cx.listener(Self::select_first))
                .on_action(cx.listener(Self::select_last))
                .on_action(cx.listener(Self::select_parent))
                .on_action(cx.listener(Self::expand_selected_entry))
                .on_action(cx.listener(Self::collapse_selected_entry))
                .on_action(cx.listener(Self::collapse_all_entries))
                .on_action(cx.listener(Self::copy_path))
                .on_action(cx.listener(Self::copy_relative_path))
                .on_action(cx.listener(Self::unfold_directory))
                .on_action(cx.listener(Self::fold_directory))
                .when(project.is_local(), |el| {
                    el.on_action(cx.listener(Self::reveal_in_finder))
                        .on_action(cx.listener(Self::open_in_terminal))
                })
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |outline_panel, event: &MouseDownEvent, cx| {
                        // When deploying the context menu anywhere below the last project entry,
                        // act as if the user clicked the last visible element (as most of the empty space to click on is below).
                        if let Some(entry) = outline_panel
                            .selected_entry
                            .as_ref()
                            .or_else(|| outline_panel.visible_entries.last())
                            .cloned()
                        {
                            outline_panel.deploy_context_menu(event.position, &entry, cx);
                        }
                    }),
                )
                .track_focus(&self.focus_handle)
                .child(
                    uniform_list(cx.view().clone(), "entries", self.visible_entries.len(), {
                        |outline_panel, range, cx| {
                            outline_panel
                                .visible_entries
                                .get(range.clone())
                                .into_iter()
                                .flatten()
                                .enumerate()
                                .filter_map(|(i, dipslayed_item)| {
                                    let depth = *outline_panel.depth_map.get(range.start + i)?;
                                    Some(outline_panel.render_entry(dipslayed_item, depth, cx))
                                })
                                .collect()
                        }
                    })
                    .size_full()
                    .track_scroll(self.scroll_handle.clone()),
                )
                .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                    deferred(
                        anchored()
                            .position(*position)
                            .anchor(gpui::AnchorCorner::TopLeft)
                            .child(menu.clone()),
                    )
                    .with_priority(1)
                }))
        }
    }
}

fn subscribe_for_editor_events(
    editor: &View<Editor>,
    cx: &mut ViewContext<OutlinePanel>,
) -> Option<Subscription> {
    if OutlinePanelSettings::get_global(cx).auto_reveal_entries {
        Some(cx.subscribe(
            editor,
            |outline_panel, editor, e: &EditorEvent, cx| match e {
                EditorEvent::SelectionsChanged { local: true } => {
                    outline_panel.reveal_entry_for_selection(&editor, cx);
                    cx.notify();
                }
                EditorEvent::ExcerptsAdded { excerpts, .. } => {
                    outline_panel.update_visible_entries(
                        &editor,
                        excerpts.iter().map(|&(excerpt_id, _)| excerpt_id).collect(),
                        None,
                        cx,
                    );
                    cx.notify();
                }
                EditorEvent::ExcerptsRemoved { .. } => {
                    outline_panel.update_visible_entries(&editor, HashSet::default(), None, cx);
                    cx.notify();
                }
                EditorEvent::ExcerptsExpanded { .. } => {
                    outline_panel.update_visible_entries(&editor, HashSet::default(), None, cx);
                    cx.notify();
                }
                EditorEvent::Reparsed => {
                    outline_panel.update_visible_entries(&editor, HashSet::default(), None, cx);
                    cx.notify();
                }
                _ => {}
            },
        ))
    } else {
        None
    }
}

fn location_for_editor_selection(
    editor: &View<Editor>,
    cx: &mut ViewContext<OutlinePanel>,
) -> Option<(OutlinesContainer, Option<OutlineItem<language::Anchor>>)> {
    let selection = editor
        .read(cx)
        .selections
        .newest::<language::Point>(cx)
        .head();
    let multi_buffer = editor.read(cx).buffer();
    let multi_buffer_snapshot = multi_buffer.read(cx).snapshot(cx);
    let selection = multi_buffer_snapshot.anchor_before(selection);
    let buffer_snapshot = multi_buffer_snapshot.buffer_for_excerpt(selection.excerpt_id)?;
    let container = match File::from_dyn(buffer_snapshot.file())
        .and_then(|file| Some(file.worktree.read(cx).id()).zip(file.entry_id))
    {
        Some((worktree_id, id)) => OutlinesContainer::WorktreeFile(worktree_id, id),
        None => OutlinesContainer::ExternalFile(buffer_snapshot.remote_id()),
    };
    let outline_item = buffer_snapshot
        .outline(Some(&cx.theme().syntax()))
        .into_iter()
        .flat_map(|outline| outline.items)
        .filter(|outline_item| {
            range_contains(&outline_item.range, selection.text_anchor, buffer_snapshot)
        })
        .min_by_key(|outline| {
            let range = outline.range.to_offset(&buffer_snapshot);
            let cursor_offset = selection.text_anchor.to_offset(&buffer_snapshot) as isize;
            let distance_to_closest_endpoint = cmp::min(
                (range.start as isize - cursor_offset).abs(),
                (range.end as isize - cursor_offset).abs(),
            );
            distance_to_closest_endpoint
        });
    Some((container, outline_item))
}

fn range_contains(
    range: &Range<language::Anchor>,
    anchor: language::Anchor,
    buffer_snapshot: &language::BufferSnapshot,
) -> bool {
    range.start.cmp(&anchor, buffer_snapshot).is_le()
        && range.end.cmp(&anchor, buffer_snapshot).is_ge()
}

// TODO kb tests
