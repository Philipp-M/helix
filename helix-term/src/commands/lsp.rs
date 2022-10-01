use helix_lsp::{
    block_on,
    lsp::{self, DiagnosticSeverity, NumberOrString},
    util::{diagnostic_to_lsp_diagnostic, lsp_pos_to_pos, lsp_range_to_range, range_to_lsp_range},
    OffsetEncoding,
};
use tui::text::{Span, Spans};

use super::{align_view, push_jump, Align, Context, Editor, Open};

use helix_core::{path, syntax::LanguageServerFeature, Selection};
use helix_view::{apply_transaction, editor::Action, theme::Style};

use crate::{
    compositor::{self, Compositor},
    ui::{
        self, lsp::SignatureHelp, overlay::overlayed, FileLocation, FilePicker, Popup, PromptEvent,
    },
};

use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

// TODO extend this to support multiple language servers
/// Gets the language server that is attached to a document, and
/// if it's not active displays a status message. Using this macro
/// in a context where the editor automatically queries the LSP
/// (instead of when the user explicitly does so via a keybind like
/// `gd`) will spam the "LSP inactive" status message confusingly.
#[macro_export]
macro_rules! language_server_with_feature {
    ($editor:expr, $doc:expr, $feature:expr) => {
        match $doc.language_servers_with_feature($feature).first() {
            Some(language_server) => &**language_server,
            None => {
                $editor.set_status("Language server not active for current buffer or no language server supports this feature");
                return;
            }
        }
    };
}

#[macro_export]
macro_rules! language_server_by_id {
    ($editor:expr, $id:expr) => {
        match $editor.language_servers.get_by_id($id) {
            Some(language_server) => language_server,
            None => {
                $editor.set_status("Language server not active for current buffer");
                return;
            }
        }
    };
}
impl ui::menu::Item for lsp::Location {
    /// Current working directory.
    type Data = PathBuf;

    fn label(&self, cwdir: &Self::Data) -> Spans {
        let file: Cow<'_, str> = (self.uri.scheme() == "file")
            .then(|| {
                self.uri
                    .to_file_path()
                    .map(|path| {
                        // strip root prefix
                        path.strip_prefix(&cwdir)
                            .map(|path| path.to_path_buf())
                            .unwrap_or(path)
                    })
                    .map(|path| Cow::from(path.to_string_lossy().into_owned()))
                    .ok()
            })
            .flatten()
            .unwrap_or_else(|| self.uri.as_str().into());
        let line = self.range.start.line;
        format!("{}:{}", file, line).into()
    }
}

impl ui::menu::Item for (lsp::SymbolInformation, OffsetEncoding) {
    /// Path to currently focussed document
    type Data = Option<lsp::Url>;

    fn label(&self, current_doc_path: &Self::Data) -> Spans {
        let info = &self.0;
        if current_doc_path.as_ref() == Some(&info.location.uri) {
            info.name.as_str().into()
        } else {
            match info.location.uri.to_file_path() {
                Ok(path) => {
                    let relative_path = helix_core::path::get_relative_path(path.as_path())
                        .to_string_lossy()
                        .into_owned();
                    format!("{} ({})", &info.name, relative_path).into()
                }
                Err(_) => format!("{} ({})", &info.name, &info.location.uri).into(),
            }
        }
    }
}

struct DiagnosticStyles {
    hint: Style,
    info: Style,
    warning: Style,
    error: Style,
}

struct PickerDiagnostic {
    url: lsp::Url,
    diag: lsp::Diagnostic,
    offset_encoding: OffsetEncoding,
}

impl ui::menu::Item for PickerDiagnostic {
    type Data = (DiagnosticStyles, DiagnosticsFormat);

    fn label(&self, (styles, format): &Self::Data) -> Spans {
        let mut style = self
            .diag
            .severity
            .map(|s| match s {
                DiagnosticSeverity::HINT => styles.hint,
                DiagnosticSeverity::INFORMATION => styles.info,
                DiagnosticSeverity::WARNING => styles.warning,
                DiagnosticSeverity::ERROR => styles.error,
                _ => Style::default(),
            })
            .unwrap_or_default();

        // remove background as it is distracting in the picker list
        style.bg = None;

        let code = self
            .diag
            .code
            .as_ref()
            .map(|c| match c {
                NumberOrString::Number(n) => n.to_string(),
                NumberOrString::String(s) => s.to_string(),
            })
            .map(|code| format!(" ({})", code))
            .unwrap_or_default();

        let path = match format {
            DiagnosticsFormat::HideSourcePath => String::new(),
            DiagnosticsFormat::ShowSourcePath => {
                let path = path::get_truncated_path(self.url.path())
                    .to_string_lossy()
                    .into_owned();
                format!("{}: ", path)
            }
        };

        Spans::from(vec![
            Span::raw(path),
            Span::styled(&self.diag.message, style),
            Span::styled(code, style),
        ])
    }
}

fn location_to_file_location(location: &lsp::Location) -> FileLocation {
    let path = location.uri.to_file_path().unwrap();
    let line = Some((
        location.range.start.line as usize,
        location.range.end.line as usize,
    ));
    (path, line)
}

// TODO: share with symbol picker(symbol.location)
fn jump_to_location(
    editor: &mut Editor,
    location: &lsp::Location,
    offset_encoding: OffsetEncoding,
    action: Action,
) {
    let (view, doc) = current!(editor);
    push_jump(view, doc);

    let path = match location.uri.to_file_path() {
        Ok(path) => path,
        Err(_) => {
            let err = format!("unable to convert URI to filepath: {}", location.uri);
            editor.set_error(err);
            return;
        }
    };
    match editor.open(&path, action) {
        Ok(_) => (),
        Err(err) => {
            let err = format!("failed to open path: {:?}: {:?}", location.uri, err);
            editor.set_error(err);
            return;
        }
    }
    let (view, doc) = current!(editor);
    let definition_pos = location.range.start;
    // TODO: convert inside server
    let new_pos = if let Some(new_pos) = lsp_pos_to_pos(doc.text(), definition_pos, offset_encoding)
    {
        new_pos
    } else {
        return;
    };
    doc.set_selection(view.id, Selection::point(new_pos));
    align_view(doc, view, Align::Center);
}

type SymbolPicker = FilePicker<(lsp::SymbolInformation, OffsetEncoding)>;

fn sym_picker(
    symbols: Vec<(lsp::SymbolInformation, OffsetEncoding)>,
    current_path: Option<lsp::Url>,
) -> SymbolPicker {
    // TODO: drop current_path comparison and instead use workspace: bool flag?
    FilePicker::new(
        symbols,
        current_path.clone(),
        move |cx, (symbol, offset_encoding), action| {
            if current_path.as_ref() == Some(&symbol.location.uri) {
                let (view, doc) = current!(cx.editor);
                push_jump(view, doc);
            } else {
                let uri = &symbol.location.uri;
                let path = match uri.to_file_path() {
                    Ok(path) => path,
                    Err(_) => {
                        let err = format!("unable to convert URI to filepath: {}", uri);
                        log::error!("{}", err);
                        cx.editor.set_error(err);
                        return;
                    }
                };
                if let Err(err) = cx.editor.open(&path, action) {
                    let err = format!("failed to open document: {}: {}", uri, err);
                    log::error!("{}", err);
                    cx.editor.set_error(err);
                    return;
                }
            }

            let (view, doc) = current!(cx.editor);

            if let Some(range) =
                lsp_range_to_range(doc.text(), symbol.location.range, *offset_encoding)
            {
                // we flip the range so that the cursor sits on the start of the symbol
                // (for example start of the function).
                doc.set_selection(view.id, Selection::single(range.head, range.anchor));
                align_view(doc, view, Align::Center);
            }
        },
        move |_editor, (symbol, _)| Some(location_to_file_location(&symbol.location)),
    )
    .truncate_start(false)
}

#[derive(Copy, Clone, PartialEq)]
enum DiagnosticsFormat {
    ShowSourcePath,
    HideSourcePath,
}

fn diag_picker(
    cx: &Context,
    diagnostics: BTreeMap<lsp::Url, Vec<(lsp::Diagnostic, OffsetEncoding)>>,
    current_path: Option<lsp::Url>,
    format: DiagnosticsFormat,
) -> FilePicker<PickerDiagnostic> {
    // TODO: drop current_path comparison and instead use workspace: bool flag?

    // flatten the map to a vec of (url, diag) pairs
    let mut flat_diag = Vec::new();
    for (url, diags) in diagnostics {
        flat_diag.reserve(diags.len());
        for (diag, offset_encoding) in diags {
            flat_diag.push(PickerDiagnostic {
                url: url.clone(),
                diag,
                offset_encoding,
            });
        }
    }

    let styles = DiagnosticStyles {
        hint: cx.editor.theme.get("hint"),
        info: cx.editor.theme.get("info"),
        warning: cx.editor.theme.get("warning"),
        error: cx.editor.theme.get("error"),
    };

    FilePicker::new(
        flat_diag,
        (styles, format),
        move |cx,
              PickerDiagnostic {
                  url,
                  diag,
                  offset_encoding,
              },
              action| {
            if current_path.as_ref() == Some(url) {
                let (view, doc) = current!(cx.editor);
                push_jump(view, doc);
            } else {
                let path = url.to_file_path().unwrap();
                cx.editor.open(&path, action).expect("editor.open failed");
            }

            let (view, doc) = current!(cx.editor);

            if let Some(range) = lsp_range_to_range(doc.text(), diag.range, *offset_encoding) {
                // we flip the range so that the cursor sits on the start of the symbol
                // (for example start of the function).
                doc.set_selection(view.id, Selection::single(range.head, range.anchor));
                align_view(doc, view, Align::Center);
            }
        },
        move |_editor,
              PickerDiagnostic {
                  url,
                  diag,
                  offset_encoding: _,
              }| {
            let location = lsp::Location::new(url.clone(), diag.range);
            Some(location_to_file_location(&location))
        },
    )
    .truncate_start(false)
}

pub fn symbol_picker(cx: &mut Context) {
    fn nested_to_flat(
        list: &mut Vec<lsp::SymbolInformation>,
        file: &lsp::TextDocumentIdentifier,
        symbol: lsp::DocumentSymbol,
    ) {
        #[allow(deprecated)]
        list.push(lsp::SymbolInformation {
            name: symbol.name,
            kind: symbol.kind,
            tags: symbol.tags,
            deprecated: symbol.deprecated,
            location: lsp::Location::new(file.uri.clone(), symbol.selection_range),
            container_name: None,
        });
        for child in symbol.children.into_iter().flatten() {
            nested_to_flat(list, file, child);
        }
    }
    let doc = doc!(cx.editor);

    let mut requests = Vec::new();
    let current_url = doc.url();

    for ls in doc.language_servers_with_feature(LanguageServerFeature::DocumentSymbols) {
        requests.push((ls.document_symbols(doc.identifier()), ls.offset_encoding()));
    }

    // TODO if the symbol picker was closed before another lsp has sent its symbols,
    // the symbol picker opens again... This could be solved by using a mutex similar as in code_action
    for (future, offset_encoding) in requests {
        let current_url = current_url.clone();

        cx.callback(
            future,
            move |editor, compositor, response: Option<lsp::DocumentSymbolResponse>| {
                if let Some(symbols) = response {
                    // lsp has two ways to represent symbols (flat/nested)
                    // convert the nested variant to flat, so that we have a homogeneous list
                    let symbols = match symbols {
                        lsp::DocumentSymbolResponse::Flat(symbols) => symbols,
                        lsp::DocumentSymbolResponse::Nested(symbols) => {
                            let doc = doc!(editor);
                            let mut flat_symbols = Vec::new();
                            for symbol in symbols {
                                nested_to_flat(&mut flat_symbols, &doc.identifier(), symbol)
                            }
                            flat_symbols
                        }
                    }
                    .into_iter()
                    .map(|s| (s, offset_encoding))
                    .collect();

                    let symbol_picker = compositor.find::<ui::Overlay<SymbolPicker>>();
                    match symbol_picker {
                        Some(picker) => picker.content.add_options(symbols),
                        None => {
                            let picker = overlayed(sym_picker(symbols, current_url));
                            compositor.push(Box::new(picker));
                        }
                    }
                }
            },
        )
    }
}

pub fn workspace_symbol_picker(cx: &mut Context) {
    let doc = doc!(cx.editor);
    let current_url = doc.url();

    let mut requests = Vec::new();

    for ls in doc.language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols) {
        requests.push((ls.workspace_symbols("".to_string()), ls.offset_encoding()));
    }

    // TODO if the symbol picker was closed before another lsp has sent its symbols,
    // the symbol picker opens again... This could be solved by using a mutex similar as in code_action
    for (future, offset_encoding) in requests {
        let current_url = current_url.clone();

        cx.callback(
            future,
            move |_editor, compositor, response: Option<Vec<lsp::SymbolInformation>>| {
                if let Some(symbols) = response {
                    let symbols = symbols.into_iter().map(|s| (s, offset_encoding)).collect();
                    let symbol_picker = compositor.find::<ui::Overlay<SymbolPicker>>();
                    match symbol_picker {
                        Some(picker) => picker.content.add_options(symbols),
                        None => {
                            let picker = overlayed(sym_picker(symbols, current_url));
                            compositor.push(Box::new(picker));
                        }
                    }
                }
            },
        )
    }
}

pub fn diagnostics_picker(cx: &mut Context) {
    let doc = doc!(cx.editor);
    if let Some(current_url) = doc.url() {
        let diagnostics = cx
            .editor
            .diagnostics
            .get(&current_url)
            .cloned()
            .unwrap_or_default();
        let picker = diag_picker(
            cx,
            [(current_url.clone(), diagnostics)].into(),
            Some(current_url),
            DiagnosticsFormat::HideSourcePath,
        );
        cx.push_layer(Box::new(overlayed(picker)));
    }
}

pub fn workspace_diagnostics_picker(cx: &mut Context) {
    let doc = doc!(cx.editor);
    let current_url = doc.url();
    let diagnostics = cx.editor.diagnostics.clone();
    let picker = diag_picker(
        cx,
        diagnostics,
        current_url,
        DiagnosticsFormat::ShowSourcePath,
    );
    cx.push_layer(Box::new(overlayed(picker)));
}

impl ui::menu::Item for (lsp::CodeActionOrCommand, OffsetEncoding) {
    type Data = ();
    fn label(&self, _data: &Self::Data) -> Spans {
        match &self.0 {
            lsp::CodeActionOrCommand::CodeAction(action) => action.title.as_str().into(),
            lsp::CodeActionOrCommand::Command(command) => command.title.as_str().into(),
        }
    }
}

pub fn code_action(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    let selection_range = doc.selection(view.id).primary();

    // this ensures, that a previously opened menu that doesn't have anything to do with this command will be replaced with a new menu
    let code_actions_menu_open = Arc::new(Mutex::new(false));

    let mut requests = Vec::new();

    for language_server in doc.language_servers_with_feature(LanguageServerFeature::CodeAction) {
        let offset_encoding = language_server.offset_encoding();
        let range = range_to_lsp_range(doc.text(), selection_range, offset_encoding);

        requests.push((
            language_server.code_actions(
                doc.identifier(),
                range,
                // Filter and convert overlapping diagnostics
                lsp::CodeActionContext {
                    diagnostics: doc
                        .diagnostics()
                        .iter()
                        .filter(|&diag| {
                            selection_range
                                .overlaps(&helix_core::Range::new(diag.range.start, diag.range.end))
                        })
                        .map(|diag| diagnostic_to_lsp_diagnostic(doc.text(), diag, offset_encoding))
                        .collect(),
                    only: None,
                },
            ),
            offset_encoding,
            language_server.id(),
        ));
    }

    for (future, offset_encoding, lsp_id) in requests {
        let code_actions_menu_open = code_actions_menu_open.clone();

        cx.callback(
            future,
            move |editor, compositor, response: Option<lsp::CodeActionResponse>| {
                let actions = match response {
                    Some(a) => a
                        .into_iter()
                        .map(|a| (a, offset_encoding))
                        .collect::<Vec<_>>(),
                    None => return,
                };

                let mut code_actions_menu_open = if let Ok(lock) = code_actions_menu_open.lock() {
                    lock
                } else {
                    editor.set_status("Mutex is poisoned, something is seriously wrong here");
                    return;
                };

                if actions.is_empty() && !*code_actions_menu_open {
                    editor.set_status("No code actions available");
                    return;
                }

                let code_actions_menu = compositor.find_id::<Popup<
                    ui::Menu<(lsp::CodeActionOrCommand, OffsetEncoding)>,
                >>("code-action");

                if !*code_actions_menu_open || code_actions_menu.is_none() {
                    let mut picker =
                        ui::Menu::new(actions, (), move |editor, code_action, event| {
                            if event != PromptEvent::Validate {
                                return;
                            }

                            // always present here
                            let code_action = code_action.unwrap();

                            match code_action {
                                (lsp::CodeActionOrCommand::Command(command), _encoding) => {
                                    log::debug!("code action command: {:?}", command);
                                    execute_lsp_command(editor, lsp_id, command.clone());
                                }
                                (
                                    lsp::CodeActionOrCommand::CodeAction(code_action),
                                    offset_encoding,
                                ) => {
                                    log::debug!("code action: {:?}", code_action);
                                    if let Some(ref workspace_edit) = code_action.edit {
                                        log::debug!("edit: {:?}", workspace_edit);
                                        apply_workspace_edit(
                                            editor,
                                            *offset_encoding,
                                            workspace_edit,
                                        );
                                    }

                                    // if code action provides both edit and command first the edit
                                    // should be applied and then the command
                                    if let Some(command) = &code_action.command {
                                        execute_lsp_command(editor, lsp_id, command.clone());
                                    }
                                }
                            }
                        });
                    picker.move_down(); // pre-select the first item

                    let popup = Popup::new("code-action", picker).auto_close(true);
                    compositor.replace_or_push("code-action", popup);
                    *code_actions_menu_open = true;
                } else if let Some(code_actions_menu) = code_actions_menu {
                    let picker = code_actions_menu.contents_mut();
                    picker.add_options(actions)
                }
            },
        )
    }
}
pub fn execute_lsp_command(editor: &mut Editor, language_server_id: usize, cmd: lsp::Command) {
    let language_server = language_server_by_id!(editor, language_server_id);

    // the command is executed on the server and communicated back
    // to the client asynchronously using workspace edits
    let command_future = language_server.command(cmd);
    tokio::spawn(async move {
        let res = command_future.await;

        if let Err(e) = res {
            log::error!("execute LSP command: {}", e);
        }
    });
}

pub fn apply_document_resource_op(op: &lsp::ResourceOp) -> std::io::Result<()> {
    use lsp::ResourceOp;
    use std::fs;
    match op {
        ResourceOp::Create(op) => {
            let path = op.uri.to_file_path().unwrap();
            let ignore_if_exists = op.options.as_ref().map_or(false, |options| {
                !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
            });
            if ignore_if_exists && path.exists() {
                Ok(())
            } else {
                // Create directory if it does not exist
                if let Some(dir) = path.parent() {
                    if !dir.is_dir() {
                        fs::create_dir_all(&dir)?;
                    }
                }

                fs::write(&path, [])
            }
        }
        ResourceOp::Delete(op) => {
            let path = op.uri.to_file_path().unwrap();
            if path.is_dir() {
                let recursive = op
                    .options
                    .as_ref()
                    .and_then(|options| options.recursive)
                    .unwrap_or(false);

                if recursive {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_dir(&path)
                }
            } else if path.is_file() {
                fs::remove_file(&path)
            } else {
                Ok(())
            }
        }
        ResourceOp::Rename(op) => {
            let from = op.old_uri.to_file_path().unwrap();
            let to = op.new_uri.to_file_path().unwrap();
            let ignore_if_exists = op.options.as_ref().map_or(false, |options| {
                !options.overwrite.unwrap_or(false) && options.ignore_if_exists.unwrap_or(false)
            });
            if ignore_if_exists && to.exists() {
                Ok(())
            } else {
                fs::rename(&from, &to)
            }
        }
    }
}

pub fn apply_workspace_edit(
    editor: &mut Editor,
    offset_encoding: OffsetEncoding,
    workspace_edit: &lsp::WorkspaceEdit,
) {
    let mut apply_edits = |uri: &helix_lsp::Url, text_edits: Vec<lsp::TextEdit>| {
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                let err = format!("unable to convert URI to filepath: {}", uri);
                log::error!("{}", err);
                editor.set_error(err);
                return;
            }
        };

        let current_view_id = view!(editor).id;
        let doc_id = match editor.open(&path, Action::Load) {
            Ok(doc_id) => doc_id,
            Err(err) => {
                let err = format!("failed to open document: {}: {}", uri, err);
                log::error!("{}", err);
                editor.set_error(err);
                return;
            }
        };

        let doc = doc_mut!(editor, &doc_id);

        // Need to determine a view for apply/append_changes_to_history
        let selections = doc.selections();
        let view_id = if selections.contains_key(&current_view_id) {
            // use current if possible
            current_view_id
        } else {
            // Hack: we take the first available view_id
            selections
                .keys()
                .next()
                .copied()
                .expect("No view_id available")
        };

        let transaction = helix_lsp::util::generate_transaction_from_edits(
            doc.text(),
            text_edits,
            offset_encoding,
        );
        apply_transaction(&transaction, doc, view_mut!(editor, view_id));
        doc.append_changes_to_history(view_id);
    };

    if let Some(ref changes) = workspace_edit.changes {
        log::debug!("workspace changes: {:?}", changes);
        for (uri, text_edits) in changes {
            let text_edits = text_edits.to_vec();
            apply_edits(uri, text_edits)
        }
        return;
        // Not sure if it works properly, it'll be safer to just panic here to avoid breaking some parts of code on which code actions will be used
        // TODO: find some example that uses workspace changes, and test it
        // for (url, edits) in changes.iter() {
        //     let file_path = url.origin().ascii_serialization();
        //     let file_path = std::path::PathBuf::from(file_path);
        //     let file = std::fs::File::open(file_path).unwrap();
        //     let mut text = Rope::from_reader(file).unwrap();
        //     let transaction = edits_to_changes(&text, edits);
        //     transaction.apply(&mut text);
        // }
    }

    if let Some(ref document_changes) = workspace_edit.document_changes {
        match document_changes {
            lsp::DocumentChanges::Edits(document_edits) => {
                for document_edit in document_edits {
                    let edits = document_edit
                        .edits
                        .iter()
                        .map(|edit| match edit {
                            lsp::OneOf::Left(text_edit) => text_edit,
                            lsp::OneOf::Right(annotated_text_edit) => {
                                &annotated_text_edit.text_edit
                            }
                        })
                        .cloned()
                        .collect();
                    apply_edits(&document_edit.text_document.uri, edits);
                }
            }
            lsp::DocumentChanges::Operations(operations) => {
                log::debug!("document changes - operations: {:?}", operations);
                for operation in operations {
                    match operation {
                        lsp::DocumentChangeOperation::Op(op) => {
                            apply_document_resource_op(op).unwrap();
                        }

                        lsp::DocumentChangeOperation::Edit(document_edit) => {
                            let edits = document_edit
                                .edits
                                .iter()
                                .map(|edit| match edit {
                                    lsp::OneOf::Left(text_edit) => text_edit,
                                    lsp::OneOf::Right(annotated_text_edit) => {
                                        &annotated_text_edit.text_edit
                                    }
                                })
                                .cloned()
                                .collect();
                            apply_edits(&document_edit.text_document.uri, edits);
                        }
                    }
                }
            }
        }
    }
}

fn goto_impl(
    editor: &mut Editor,
    compositor: &mut Compositor,
    locations: Vec<lsp::Location>,
    offset_encoding: OffsetEncoding,
) {
    let cwdir = std::env::current_dir().unwrap_or_default();

    match locations.as_slice() {
        [location] => {
            jump_to_location(editor, location, offset_encoding, Action::Replace);
        }
        [] => {
            editor.set_error("No definition found.");
        }
        _locations => {
            let picker = FilePicker::new(
                locations,
                cwdir,
                move |cx, location, action| {
                    jump_to_location(cx.editor, location, offset_encoding, action)
                },
                move |_editor, location| Some(location_to_file_location(location)),
            );
            compositor.push(Box::new(overlayed(picker)));
        }
    }
}

fn to_locations(definitions: Option<lsp::GotoDefinitionResponse>) -> Vec<lsp::Location> {
    match definitions {
        Some(lsp::GotoDefinitionResponse::Scalar(location)) => vec![location],
        Some(lsp::GotoDefinitionResponse::Array(locations)) => locations,
        Some(lsp::GotoDefinitionResponse::Link(locations)) => locations
            .into_iter()
            .map(|location_link| lsp::Location {
                uri: location_link.target_uri,
                range: location_link.target_range,
            })
            .collect(),
        None => Vec::new(),
    }
}

pub fn goto_definition(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::GotoDefinition);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.goto_definition(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, compositor, response: Option<lsp::GotoDefinitionResponse>| {
            let items = to_locations(response);
            goto_impl(editor, compositor, items, offset_encoding);
        },
    );
}

pub fn goto_type_definition(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::GotoTypeDefinition);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.goto_type_definition(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, compositor, response: Option<lsp::GotoDefinitionResponse>| {
            let items = to_locations(response);
            goto_impl(editor, compositor, items, offset_encoding);
        },
    );
}

pub fn goto_implementation(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::GotoImplementation);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.goto_implementation(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, compositor, response: Option<lsp::GotoDefinitionResponse>| {
            let items = to_locations(response);
            goto_impl(editor, compositor, items, offset_encoding);
        },
    );
}

pub fn goto_reference(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::GotoReference);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.goto_reference(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, compositor, response: Option<Vec<lsp::Location>>| {
            let items = response.unwrap_or_default();
            goto_impl(editor, compositor, items, offset_encoding);
        },
    );
}

#[derive(PartialEq)]
pub enum SignatureHelpInvoked {
    Manual,
    Automatic,
}

pub fn signature_help(cx: &mut Context) {
    signature_help_impl(cx, SignatureHelpInvoked::Manual)
}

pub fn signature_help_impl(cx: &mut Context, invoked: SignatureHelpInvoked) {
    let doc = doc!(cx.editor);
    let was_manually_invoked = invoked == SignatureHelpInvoked::Manual;

    let language_server_id = match doc
        .language_servers_with_feature(LanguageServerFeature::SignatureHelp)
        .first()
    {
        Some(language_server) => language_server.id(),
        None => {
            // Do not show the message if signature help was invoked
            // automatically on backspace, trigger characters, etc.
            if was_manually_invoked {
                cx.editor
                    .set_status("Language server not active for current buffer");
            }
            return;
        }
    };
    signature_help_impl_with_language_server_id(cx, language_server_id, invoked);
}

pub fn signature_help_impl_with_language_server_id(
    cx: &mut Context,
    language_server_id: usize,
    invoked: SignatureHelpInvoked,
) {
    let was_manually_invoked = invoked == SignatureHelpInvoked::Manual;
    let (view, doc) = current!(cx.editor);
    let language_server = language_server_by_id!(cx.editor, language_server_id);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = match language_server.text_document_signature_help(doc.identifier(), pos, None) {
        Some(f) => f,
        None => return,
    };

    cx.callback(
        future,
        move |editor, compositor, response: Option<lsp::SignatureHelp>| {
            let config = &editor.config();

            if !(config.lsp.auto_signature_help
                || SignatureHelp::visible_popup(compositor).is_some()
                || was_manually_invoked)
            {
                return;
            }

            let response = match response {
                // According to the spec the response should be None if there
                // are no signatures, but some servers don't follow this.
                Some(s) if !s.signatures.is_empty() => s,
                _ => {
                    compositor.remove(SignatureHelp::ID);
                    return;
                }
            };
            let doc = doc!(editor);
            let language = doc.language_name().unwrap_or("");

            let signature = match response
                .signatures
                .get(response.active_signature.unwrap_or(0) as usize)
            {
                Some(s) => s,
                None => return,
            };
            let mut contents = SignatureHelp::new(
                signature.label.clone(),
                language.to_string(),
                Arc::clone(&editor.syn_loader),
            );

            let signature_doc = if config.lsp.display_signature_help_docs {
                signature.documentation.as_ref().map(|doc| match doc {
                    lsp::Documentation::String(s) => s.clone(),
                    lsp::Documentation::MarkupContent(markup) => markup.value.clone(),
                })
            } else {
                None
            };

            contents.set_signature_doc(signature_doc);

            let active_param_range = || -> Option<(usize, usize)> {
                let param_idx = signature
                    .active_parameter
                    .or(response.active_parameter)
                    .unwrap_or(0) as usize;
                let param = signature.parameters.as_ref()?.get(param_idx)?;
                match &param.label {
                    lsp::ParameterLabel::Simple(string) => {
                        let start = signature.label.find(string.as_str())?;
                        Some((start, start + string.len()))
                    }
                    lsp::ParameterLabel::LabelOffsets([start, end]) => {
                        // LS sends offsets based on utf-16 based string representation
                        // but highlighting in helix is done using byte offset.
                        use helix_core::str_utils::char_to_byte_idx;
                        let from = char_to_byte_idx(&signature.label, *start as usize);
                        let to = char_to_byte_idx(&signature.label, *end as usize);
                        Some((from, to))
                    }
                }
            };
            contents.set_active_param_range(active_param_range());

            let old_popup = compositor.find_id::<Popup<SignatureHelp>>(SignatureHelp::ID);
            let popup = Popup::new(SignatureHelp::ID, contents)
                .position(old_popup.and_then(|p| p.get_position()))
                .position_bias(Open::Above)
                .ignore_escape_key(true);
            compositor.replace_or_push(SignatureHelp::ID, popup);
        },
    );
}

pub fn hover(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::Hover);
    let offset_encoding = language_server.offset_encoding();

    // TODO: factor out a doc.position_identifier() that returns lsp::TextDocumentPositionIdentifier

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.text_document_hover(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, compositor, response: Option<lsp::Hover>| {
            if let Some(hover) = response {
                // hover.contents / .range <- used for visualizing

                fn marked_string_to_markdown(contents: lsp::MarkedString) -> String {
                    match contents {
                        lsp::MarkedString::String(contents) => contents,
                        lsp::MarkedString::LanguageString(string) => {
                            if string.language == "markdown" {
                                string.value
                            } else {
                                format!("```{}\n{}\n```", string.language, string.value)
                            }
                        }
                    }
                }

                let contents = match hover.contents {
                    lsp::HoverContents::Scalar(contents) => marked_string_to_markdown(contents),
                    lsp::HoverContents::Array(contents) => contents
                        .into_iter()
                        .map(marked_string_to_markdown)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    lsp::HoverContents::Markup(contents) => contents.value,
                };

                // skip if contents empty

                let contents = ui::Markdown::new(contents, editor.syn_loader.clone());
                let popup = Popup::new("hover", contents).auto_close(true);
                compositor.replace_or_push("hover", popup);
            }
        },
    );
}

pub fn rename_symbol(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);
    let primary_selection = doc.selection(view.id).primary();
    let prefill = if primary_selection.len() > 1 {
        primary_selection
    } else {
        use helix_core::textobject::{textobject_word, TextObject};
        textobject_word(text, primary_selection, TextObject::Inside, 1, false)
    }
    .fragment(text)
    .into();
    ui::prompt_with_input(
        cx,
        "rename-to:".into(),
        prefill,
        None,
        ui::completers::none,
        move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }

            let (view, doc) = current!(cx.editor);
            let language_server =
                language_server_with_feature!(cx.editor, doc, LanguageServerFeature::RenameSymbol);
            let offset_encoding = language_server.offset_encoding();

            let pos = doc.position(view.id, offset_encoding);

            let task = language_server.rename_symbol(doc.identifier(), pos, input.to_string());
            match block_on(task) {
                Ok(edits) => apply_workspace_edit(cx.editor, offset_encoding, &edits),
                Err(err) => cx.editor.set_error(err.to_string()),
            }
        },
    );
}

pub fn select_references_to_symbol_under_cursor(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::DocumentHighlight);
    let offset_encoding = language_server.offset_encoding();

    let pos = doc.position(view.id, offset_encoding);

    let future = language_server.text_document_document_highlight(doc.identifier(), pos, None);

    cx.callback(
        future,
        move |editor, _compositor, response: Option<Vec<lsp::DocumentHighlight>>| {
            let document_highlights = match response {
                Some(highlights) if !highlights.is_empty() => highlights,
                _ => return,
            };
            let (view, doc) = current!(editor);
            let text = doc.text();
            let pos = doc.selection(view.id).primary().head;

            // We must find the range that contains our primary cursor to prevent our primary cursor to move
            let mut primary_index = 0;
            let ranges = document_highlights
                .iter()
                .filter_map(|highlight| lsp_range_to_range(text, highlight.range, offset_encoding))
                .enumerate()
                .map(|(i, range)| {
                    if range.contains(pos) {
                        primary_index = i;
                    }
                    range
                })
                .collect();
            let selection = Selection::new(ranges, primary_index);
            doc.set_selection(view.id, selection);
        },
    );
}
