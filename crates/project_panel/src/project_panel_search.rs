use std::{ops::Range, path::Path, pin::pin, time::Duration};

use collections::HashSet;
use editor::{Editor, EditorEvent, MultiBufferOffset, SelectionEffects, scroll::Autoscroll};
use file_icons::FileIcons;
use futures::StreamExt as _;
use gpui::{
    App, ClickEvent, Context, Entity, FocusHandle, Focusable, HighlightStyle, ListSizingBehavior,
    PromptLevel, Render, StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, uniform_list,
};
use language::{Anchor, Buffer, BufferId, BufferSnapshot, Point, ToOffset as _, ToPoint as _};
use project::{
    Project, ProjectItem as _, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use search::{SearchOption, SearchOptions, project_search::split_glob_patterns};
use settings::Settings as _;
use ui::{ScrollbarRevealPolicy, Scrollbars, Tooltip, WithScrollbar, prelude::*};
use util::paths::PathMatcher;
use workspace::{PreviewTabsSettings, Workspace, notifications::DetachAndPromptErr as _};

use crate::project_panel_settings::ProjectPanelScrollbarProxy;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_PREVIEW_PREFIX_LEN: usize = 24;
const MAX_PREVIEW_LEN: usize = 200;

/// Project-wide search rendered inside the project panel, in place of the file
/// tree, with results listed per file that open in a preview tab when clicked.
pub(crate) struct ProjectPanelSearch {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    replacement_editor: Entity<Editor>,
    included_files_editor: Entity<Editor>,
    excluded_files_editor: Entity<Editor>,
    search_options: SearchOptions,
    replace_enabled: bool,
    filters_enabled: bool,
    files: Vec<FileMatches>,
    collapsed_files: HashSet<BufferId>,
    rows: Vec<ResultRow>,
    status: SearchStatus,
    active_query: Option<SearchQuery>,
    scroll_handle: UniformListScrollHandle,
    search_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

struct FileMatches {
    buffer: Entity<Buffer>,
    buffer_id: BufferId,
    project_path: ProjectPath,
    file_name: SharedString,
    directory: SharedString,
    icon: Option<SharedString>,
    matches: Vec<MatchPreview>,
}

struct MatchPreview {
    range: Range<Anchor>,
    line: SharedString,
    highlight: Range<usize>,
}

#[derive(Clone, Copy)]
enum ResultRow {
    File(usize),
    Match {
        file_index: usize,
        match_index: usize,
    },
}

enum SearchStatus {
    Idle,
    Searching,
    Finished { limit_reached: bool },
    InvalidQuery(SharedString),
}

impl ProjectPanelSearch {
    pub(crate) fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let new_input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };
        let query_editor = new_input("Search", window, cx);
        let replacement_editor = new_input("Replace", window, cx);
        let included_files_editor = new_input("e.g. src/**/*.rs", window, cx);
        let excluded_files_editor = new_input("e.g. vendor/*, *.lock", window, cx);

        let subscriptions = [
            &query_editor,
            &included_files_editor,
            &excluded_files_editor,
        ]
        .into_iter()
        .map(|editor| {
            cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::BufferEdited = event {
                    this.schedule_search(true, cx);
                }
            })
        })
        .collect();

        Self {
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            query_editor,
            replacement_editor,
            included_files_editor,
            excluded_files_editor,
            search_options: SearchOptions::NONE,
            replace_enabled: true,
            filters_enabled: true,
            files: Vec::new(),
            collapsed_files: HashSet::default(),
            rows: Vec::new(),
            status: SearchStatus::Idle,
            active_query: None,
            scroll_handle: UniformListScrollHandle::new(),
            search_task: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn focus_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.query_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.search_options.toggle(option);
        self.schedule_search(false, cx);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.schedule_search(false, cx);
    }

    fn schedule_search(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.search_task = cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            }
            let Some(results) = this
                .update(cx, |this, cx| this.start_search(cx))
                .ok()
                .flatten()
            else {
                return;
            };

            // Dropping the task handle cancels the search, so hold it until the
            // stream has been drained.
            let _search_task = results.task_handle;
            let mut chunks = pin!(results.rx.ready_chunks(256));
            let mut limit_reached = false;
            while let Some(chunk) = chunks.next().await {
                let updated = this.update(cx, |this, cx| {
                    for result in chunk {
                        match result {
                            SearchResult::Buffer { buffer, ranges } => {
                                this.push_file_matches(buffer, ranges, cx);
                            }
                            SearchResult::LimitReached => limit_reached = true,
                            SearchResult::WaitingForScan | SearchResult::Searching => {}
                        }
                    }
                    this.rebuild_rows();
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }

            this.update(cx, |this, cx| {
                this.status = SearchStatus::Finished { limit_reached };
                cx.notify();
            })
            .ok();
        });
    }

    fn start_search(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<project::project_search::SearchResults<SearchResult>> {
        self.files.clear();
        self.collapsed_files.clear();
        self.rows.clear();
        self.active_query = None;
        cx.notify();

        let query_text = self.query_editor.read(cx).text(cx);
        if query_text.is_empty() {
            self.status = SearchStatus::Idle;
            return None;
        }

        let query = self
            .parse_path_matcher(&self.included_files_editor, cx)
            .and_then(|included_files| {
                let excluded_files = self.parse_path_matcher(&self.excluded_files_editor, cx)?;
                let match_full_paths = self.project.read(cx).visible_worktrees(cx).count() > 1;
                self.search_options.build_query(
                    query_text,
                    included_files,
                    excluded_files,
                    match_full_paths,
                    None,
                )
            });
        match query {
            Ok(query) => {
                self.status = SearchStatus::Searching;
                self.active_query = Some(query.clone());
                Some(
                    self.project
                        .update(cx, |project, cx| project.search(query, cx)),
                )
            }
            Err(error) => {
                self.status = SearchStatus::InvalidQuery(error.to_string().into());
                None
            }
        }
    }

    fn parse_path_matcher(&self, editor: &Entity<Editor>, cx: &App) -> anyhow::Result<PathMatcher> {
        if !self.filters_enabled {
            return Ok(PathMatcher::default());
        }
        let text = editor.read(cx).text(cx);
        let globs = split_glob_patterns(&text)
            .into_iter()
            .map(str::trim)
            .filter(|glob| !glob.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Ok(PathMatcher::new(
            &globs,
            self.project.read(cx).path_style(cx),
        )?)
    }

    fn push_file_matches(&mut self, buffer: Entity<Buffer>, ranges: Vec<Range<Anchor>>, cx: &App) {
        let buffer_ref = buffer.read(cx);
        let (Some(file), Some(project_path)) = (buffer_ref.file(), buffer_ref.project_path(cx))
        else {
            return;
        };
        let path_style = file.path_style(cx);
        let file_name = file.file_name(cx).to_string();
        let directory = if self.project.read(cx).visible_worktrees(cx).count() > 1 {
            file.full_path(cx)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            file.path()
                .parent()
                .map(|parent| parent.display(path_style).into_owned())
                .unwrap_or_default()
        };
        let snapshot = buffer_ref.snapshot();
        let matches = ranges
            .into_iter()
            .map(|range| match_preview(&snapshot, range))
            .collect();

        self.files.push(FileMatches {
            buffer_id: buffer_ref.remote_id(),
            icon: FileIcons::get_icon(Path::new(&file_name), cx),
            buffer,
            project_path,
            file_name: file_name.into(),
            directory: directory.into(),
            matches,
        });
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (file_index, file) in self.files.iter().enumerate() {
            self.rows.push(ResultRow::File(file_index));
            if !self.collapsed_files.contains(&file.buffer_id) {
                self.rows
                    .extend((0..file.matches.len()).map(|match_index| ResultRow::Match {
                        file_index,
                        match_index,
                    }));
            }
        }
    }

    fn toggle_file_collapsed(&mut self, file_index: usize, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(file_index) else {
            return;
        };
        if !self.collapsed_files.remove(&file.buffer_id) {
            self.collapsed_files.insert(file.buffer_id);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn open_match(
        &mut self,
        file_index: usize,
        match_index: usize,
        allow_preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.files.get(file_index) else {
            return;
        };
        let Some(preview) = file.matches.get(match_index) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let buffer = file.buffer.clone();
        let range = preview.range.clone();
        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_path_preview(
                file.project_path.clone(),
                None,
                true,
                allow_preview,
                true,
                window,
                cx,
            )
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = open_task.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    let snapshot = buffer.read(cx).snapshot();
                    let range = range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| {
                            selections.select_ranges([
                                MultiBufferOffset(range.start)..MultiBufferOffset(range.end)
                            ]);
                        },
                    );
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(query) = self.active_query.clone() else {
            return;
        };
        let replacement = self.replacement_editor.read(cx).text(cx);
        let query = query.with_replacement(replacement.clone());
        let match_count: usize = self.files.iter().map(|file| file.matches.len()).sum();
        if match_count == 0 {
            return;
        }
        let file_count = self.files.len();
        let matches_by_buffer = self
            .files
            .iter()
            .map(|file| {
                let ranges = file
                    .matches
                    .iter()
                    .map(|preview| preview.range.clone())
                    .collect::<Vec<_>>();
                (file.buffer.clone(), ranges)
            })
            .collect::<Vec<_>>();

        let message = format!(
            "Replace {match_count} {} across {file_count} {} with '{replacement}'?",
            if match_count == 1 {
                "occurrence"
            } else {
                "occurrences"
            },
            if file_count == 1 { "file" } else { "files" },
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Replace", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            let edited_buffers = cx.update(|_, cx| {
                for (buffer, ranges) in &matches_by_buffer {
                    buffer.update(cx, |buffer, cx| {
                        let snapshot = buffer.snapshot();
                        let edits = ranges
                            .iter()
                            .filter_map(|range| {
                                let offsets = range.start.to_offset(&snapshot)
                                    ..range.end.to_offset(&snapshot);
                                let (line, hit) = replacement_context(&snapshot, &offsets);
                                let replacement = query.replacement_for(&line, hit)?;
                                Some((offsets, replacement.into_owned()))
                            })
                            .collect::<Vec<_>>();
                        buffer.edit(edits, None, cx);
                    });
                }
                matches_by_buffer
                    .into_iter()
                    .map(|(buffer, _)| buffer)
                    .collect::<HashSet<_>>()
            })?;
            this.update(cx, |this, cx| {
                this.project
                    .update(cx, |project, cx| project.save_buffers(edited_buffers, cx))
            })?
            .await?;
            this.update(cx, |this, cx| this.schedule_search(false, cx))?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to replace", window, cx, |_, _, _| None);
    }

    fn open_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query_editor.read(cx).text(cx);
        let (included_files, excluded_files) = if self.filters_enabled {
            (
                Some(self.included_files_editor.read(cx).text(cx)),
                Some(self.excluded_files_editor.read(cx).text(cx)),
            )
        } else {
            (None, None)
        };
        let action = workspace::DeploySearch {
            replace_enabled: self.replace_enabled,
            included_files,
            excluded_files,
            query: Some(query),
            regex: Some(self.search_options.contains(SearchOptions::REGEX)),
            case_sensitive: Some(self.search_options.contains(SearchOptions::CASE_SENSITIVE)),
            whole_word: Some(self.search_options.contains(SearchOptions::WHOLE_WORD)),
            include_ignored: Some(self.search_options.contains(SearchOptions::INCLUDE_IGNORED)),
        };
        window.dispatch_action(Box::new(action), cx);
    }

    fn render_option_button(
        &self,
        option: SearchOption,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let options = option.as_options();
        IconButton::new(option.label(), option.icon())
            .size(ButtonSize::Compact)
            .icon_size(IconSize::Small)
            .toggle_state(self.search_options.contains(options))
            .tooltip(Tooltip::text(option.label()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_search_option(options, cx);
            }))
    }

    fn render_input(
        &self,
        editor: &Entity<Editor>,
        has_error: bool,
        window: &Window,
        cx: &App,
    ) -> Div {
        let colors = cx.theme().colors();
        h_flex()
            .min_h_7()
            .pl_2()
            .pr_0p5()
            .gap_1()
            .border_1()
            .border_color(if has_error {
                Color::Error.color(cx)
            } else if editor.focus_handle(cx).contains_focused(window, cx) {
                colors.border_focused
            } else {
                colors.border
            })
            .rounded_md()
            .bg(colors.editor_background)
            .child(div().flex_1().min_w_0().child(editor.clone()))
    }

    fn render_inputs(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query_error = matches!(self.status, SearchStatus::InvalidQuery(_));
        let can_replace = self.active_query.is_some() && !self.files.is_empty();

        h_flex()
            .w_full()
            .items_start()
            .gap_0p5()
            .child(
                IconButton::new(
                    "project-panel-search-toggle-replace",
                    if self.replace_enabled {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                )
                .size(ButtonSize::Compact)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Toggle Replace"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_enabled = !this.replace_enabled;
                    cx.notify();
                })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        self.render_input(&self.query_editor, has_query_error, window, cx)
                            .child(self.render_option_button(SearchOption::CaseSensitive, cx))
                            .child(self.render_option_button(SearchOption::WholeWord, cx))
                            .child(self.render_option_button(SearchOption::Regex, cx)),
                    )
                    .when(self.replace_enabled, |this| {
                        this.child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    self.render_input(&self.replacement_editor, false, window, cx)
                                        .flex_1(),
                                )
                                .child(
                                    IconButton::new(
                                        "project-panel-search-replace-all",
                                        IconName::ReplaceAll,
                                    )
                                    .size(ButtonSize::Compact)
                                    .icon_size(IconSize::Small)
                                    .disabled(!can_replace)
                                    .tooltip(Tooltip::text("Replace All"))
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.replace_all(window, cx);
                                        },
                                    )),
                                ),
                        )
                    }),
            )
    }

    fn render_filters(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label =
            |text: &'static str| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        v_flex()
            .gap_1()
            .pl_6()
            .child(label("files to include"))
            .child(self.render_input(&self.included_files_editor, false, window, cx))
            .child(label("files to exclude"))
            .child(
                self.render_input(&self.excluded_files_editor, false, window, cx)
                    .child(self.render_option_button(SearchOption::IncludeIgnored, cx)),
            )
    }

    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let match_count: usize = self.files.iter().map(|file| file.matches.len()).sum();
        let summary: SharedString = match &self.status {
            SearchStatus::Idle => "".into(),
            SearchStatus::Searching if self.files.is_empty() => "Searching…".into(),
            SearchStatus::InvalidQuery(error) => error.clone(),
            SearchStatus::Searching | SearchStatus::Finished { .. } if match_count == 0 => {
                "No results found".into()
            }
            SearchStatus::Searching | SearchStatus::Finished { .. } => format!(
                "{match_count} {} in {} {}{}",
                if match_count == 1 {
                    "result"
                } else {
                    "results"
                },
                self.files.len(),
                if self.files.len() == 1 {
                    "file"
                } else {
                    "files"
                },
                if matches!(
                    self.status,
                    SearchStatus::Finished {
                        limit_reached: true
                    }
                ) {
                    " (limit reached)"
                } else {
                    ""
                },
            )
            .into(),
        };

        h_flex()
            .pl_6()
            .gap_1()
            .min_h_5()
            .child(
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(if matches!(self.status, SearchStatus::InvalidQuery(_)) {
                        Color::Error
                    } else {
                        Color::Muted
                    })
                    .truncate(),
            )
            .when(self.active_query.is_some(), |this| {
                this.child(Label::new("·").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        Button::new("project-panel-search-open-in-editor", "Open in Editor")
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Transparent)
                            .color(Color::Accent)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_in_editor(window, cx);
                            })),
                    )
            })
    }

    fn render_row(
        &self,
        row_index: usize,
        row: ResultRow,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let base = h_flex()
            .id(("project-panel-search-row", row_index))
            .w_full()
            .h_6()
            // Keeps the match count clear of the scrollbar's track.
            .pr_4()
            .gap_1()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover));

        match row {
            ResultRow::File(file_index) => {
                let Some(file) = self.files.get(file_index) else {
                    return base.into_any_element();
                };
                let collapsed = self.collapsed_files.contains(&file.buffer_id);
                base.pl_1()
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .when_some(file.icon.clone(), |this, icon| {
                        this.child(
                            Icon::from_path(icon)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .child(Label::new(file.file_name.clone()).single_line())
                            .child(
                                Label::new(file.directory.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_1p5()
                            .rounded_full()
                            .bg(colors.element_background)
                            .child(
                                Label::new(file.matches.len().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_file_collapsed(file_index, cx);
                    }))
                    .into_any_element()
            }
            ResultRow::Match {
                file_index,
                match_index,
            } => {
                let Some(preview) = self
                    .files
                    .get(file_index)
                    .and_then(|file| file.matches.get(match_index))
                else {
                    return base.into_any_element();
                };
                let mut text_style = window.text_style();
                text_style.color = colors.text_muted;
                let highlight = HighlightStyle {
                    color: Some(colors.text),
                    background_color: Some(colors.search_match_background),
                    ..HighlightStyle::default()
                };
                base.pl(px(44.))
                    .child(div().flex_1().min_w_0().truncate().child(
                        StyledText::new(preview.line.clone()).with_default_highlights(
                            &text_style,
                            [(preview.highlight.clone(), highlight)],
                        ),
                    ))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let allow_preview = PreviewTabsSettings::get_global(cx)
                            .enable_preview_from_project_panel
                            && event.click_count() < 2;
                        this.open_match(file_index, match_index, allow_preview, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }
}

/// Builds the single-line preview shown for a match, keeping the match visible
/// by eliding leading context when it starts far into the line.
fn match_preview(snapshot: &BufferSnapshot, range: Range<Anchor>) -> MatchPreview {
    let start = range.start.to_point(snapshot);
    let end = range.end.to_point(snapshot);
    let line = snapshot
        .text_for_range(
            Point::new(start.row, 0)..Point::new(start.row, snapshot.line_len(start.row)),
        )
        .collect::<String>();
    let match_start = (start.column as usize).min(line.len());
    let match_end = if end.row == start.row {
        (end.column as usize).min(line.len())
    } else {
        line.len()
    };

    let mut display_start = (line.len() - line.trim_start().len()).min(match_start);
    let mut prefix = "";
    if match_start - display_start > MAX_PREVIEW_PREFIX_LEN {
        display_start = floor_char_boundary(&line, match_start - MAX_PREVIEW_PREFIX_LEN);
        prefix = "…";
    }
    let display_end = floor_char_boundary(
        &line,
        (display_start + MAX_PREVIEW_LEN)
            .max(match_end)
            .min(line.len()),
    );

    let text = format!("{prefix}{}", &line[display_start..display_end]);
    let highlight_start = prefix.len() + match_start - display_start;
    let highlight_end = prefix.len() + match_end.min(display_end) - display_start;
    MatchPreview {
        range,
        line: text.into(),
        highlight: highlight_start..highlight_end,
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Returns the text a replacement is computed against, mirroring the editor's
/// own replace so regex lookarounds and capture groups behave the same.
fn replacement_context(snapshot: &BufferSnapshot, hit: &Range<usize>) -> (String, Range<usize>) {
    let start = hit.start.to_point(snapshot);
    let end = hit.end.to_point(snapshot);
    if start.row == end.row {
        let line_start = Point::new(start.row, 0);
        let line_end = Point::new(start.row, snapshot.line_len(start.row));
        (
            snapshot.text_for_range(line_start..line_end).collect(),
            start.column as usize..end.column as usize,
        )
    } else {
        let text = snapshot.text_for_range(hit.clone()).collect::<String>();
        let len = text.len();
        (text, 0..len)
    }
}

#[cfg(test)]
impl ProjectPanelSearch {
    pub(crate) fn is_query_focused(&self, window: &Window, cx: &App) -> bool {
        self.query_editor.focus_handle(cx).is_focused(window)
    }

    pub(crate) fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query_editor
            .update(cx, |editor, cx| editor.set_text(query, window, cx));
    }

    pub(crate) fn set_replacement(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replacement_editor
            .update(cx, |editor, cx| editor.set_text(replacement, window, cx));
    }

    pub(crate) fn results_scroll_handle(&self) -> gpui::ScrollHandle {
        self.scroll_handle.0.borrow().base_handle.clone()
    }

    /// Returns each matching file's name with its match previews, marking the
    /// highlighted part of each preview with «».
    pub(crate) fn results_summary(&self) -> Vec<(String, Vec<String>)> {
        self.files
            .iter()
            .map(|file| {
                let matches = file
                    .matches
                    .iter()
                    .map(|preview| {
                        let line = preview.line.as_ref();
                        format!(
                            "{}«{}»{}",
                            &line[..preview.highlight.start],
                            &line[preview.highlight.clone()],
                            &line[preview.highlight.end..]
                        )
                    })
                    .collect();
                (file.file_name.to_string(), matches)
            })
            .collect()
    }
}

impl Focusable for ProjectPanelSearch {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ProjectPanelSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("project-panel-search")
            .key_context("ProjectPanelSearch")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .child(
                v_flex()
                    .flex_none()
                    .p_1p5()
                    .gap_1()
                    .child(self.render_inputs(window, cx))
                    .child(
                        h_flex().justify_end().child(
                            IconButton::new(
                                "project-panel-search-toggle-details",
                                IconName::Ellipsis,
                            )
                            .size(ButtonSize::Compact)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.filters_enabled)
                            .tooltip(Tooltip::text("Toggle Search Details"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.filters_enabled = !this.filters_enabled;
                                this.schedule_search(false, cx);
                                cx.notify();
                            })),
                        ),
                    )
                    .when(self.filters_enabled, |this| {
                        this.child(self.render_filters(window, cx))
                    })
                    .child(self.render_summary(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "project-panel-search-results",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, window, cx| {
                                range
                                    .filter_map(|row_index| {
                                        let row = *this.rows.get(row_index)?;
                                        Some(this.render_row(row_index, row, window, cx))
                                    })
                                    .collect()
                            }),
                        )
                        .with_sizing_behavior(ListSizingBehavior::Infer)
                        .track_scroll(&self.scroll_handle)
                        .flex_1(),
                    )
                    .custom_scrollbars(
                        Scrollbars::for_settings::<ProjectPanelScrollbarProxy>()
                            .tracked_scroll_handle(&self.scroll_handle)
                            .reveal_policy(ScrollbarRevealPolicy::ScrollOrTrackHover)
                            .notify_content(),
                        window,
                        cx,
                    ),
            )
    }
}
