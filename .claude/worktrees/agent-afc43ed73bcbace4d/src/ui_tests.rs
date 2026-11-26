//! UI tests: drive the real `App` with egui_kittest (headless, no Everything).
//!
//! Widgets are found through the AccessKit tree by their visible label. The UI
//! language is a process-wide setting that another test flips, so lookups accept
//! the Spanish or the English label.

use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::Queryable;
use egui_kittest::{Harness, Node};
use std::time::Instant;

use crate::app::{self, App, Col};
use crate::tree::{Builder, Tree};

// A drive name that can't exist, so nothing touches a real disk.
const D: &str = "Q9:";

/// Q9:\ big.iso (1 000 000) · docs\a.pdf (5000) · docs\b.PDF (100) · docs\sub\c.txt (10)
///      media\clip.mp4 (2000) · noext (1)
fn sample() -> Tree {
    let mut b = Builder::new();
    b.add(true, "", D, None, 1);
    b.add(true, D, "docs", None, 2);
    b.add(false, "Q9:\\docs", "a.pdf", Some(5000), 3);
    b.add(false, "Q9:\\docs", "b.PDF", Some(100), 4);
    b.add(false, "Q9:\\docs\\sub", "c.txt", Some(10), 5);
    b.add(true, D, "media", None, 6);
    b.add(false, "Q9:\\media", "clip.mp4", Some(2000), 7);
    b.add(false, D, "big.iso", Some(1_000_000), 8);
    b.add(false, D, "noext", Some(1), 9);
    b.finish(&[], Instant::now(), "2026-01-01T00:00:00".into())
}

fn harness() -> Harness<'static, App> {
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .with_max_steps(20)
        // The unlabelled drive / "Prefer" combo boxes would fail kittest's
        // accessible-name check; that is not what these tests are about.
        .with_accessibility_check(false)
        .build_eframe(|cc| App::for_test(&cc.egui_ctx, sample()));
    h.run();
    h
}

/// The node labelled `es` or `en` (exactly one must exist).
#[track_caller]
fn get<'a>(h: &'a Harness<'_, App>, es: &'static str, en: &'static str) -> Node<'a> {
    h.query_by_label(en).or_else(|| h.query_by_label(es)).unwrap_or_else(|| panic!("no widget labelled {en:?} / {es:?}"))
}

fn has(h: &Harness<'_, App>, es: &'static str, en: &'static str) -> bool {
    h.query_by_label(en).is_some() || h.query_by_label(es).is_some()
}

/// The node whose label contains `es` or `en`.
#[track_caller]
fn get_contains<'a>(h: &'a Harness<'_, App>, es: &'static str, en: &'static str) -> Node<'a> {
    h.query_by_label_contains(en)
        .or_else(|| h.query_by_label_contains(es))
        .unwrap_or_else(|| panic!("no widget whose label contains {en:?} / {es:?}"))
}

fn names(h: &Harness<'_, App>) -> Vec<String> {
    let app = h.state();
    let t = app.t_tree();
    app.t_rows().iter().skip(1).map(|&i| t.name(i).to_string()).collect()
}

// ------------------------------------------------------------------ tree view

#[test]
fn tree_lists_folders_first_then_files() {
    let h = harness();
    // Default order is size descending, but folders come before loose files,
    // even though big.iso is larger than both folders.
    assert_eq!(names(&h), ["docs", "media", "big.iso", "noext"]);
    // And that is also how they are drawn, top to bottom.
    let y = |n: &str| h.get_by_label(n).rect().center().y;
    assert!(y("docs") < y("media") && y("media") < y("big.iso") && y("big.iso") < y("noext"));
}

#[test]
fn right_click_on_tree_row_opens_context_menu() {
    let mut h = harness();
    assert!(!h.state().t_menu_open());
    h.get_by_label("docs").click_secondary();
    h.run();
    assert!(h.state().t_menu_open(), "right-click on a row must open the context menu");
    get(&h, "Copiar ruta completa", "Copy full path");
    get(&h, "Copiar nombre", "Copy name");
    get_contains(&h, "Excluir de duplicados", "Exclude from duplicates");

    // Copy full path puts the row's path on the clipboard and closes the menu.
    get(&h, "Copiar ruta completa", "Copy full path").click();
    h.step();
    let copied: Vec<&String> = h
        .output()
        .platform_output
        .commands
        .iter()
        .filter_map(|c| match c {
            egui::OutputCommand::CopyText(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(copied, ["Q9:\\docs"]);
    h.run();
    assert!(!h.state().t_menu_open());
    assert!(!has(&h, "Copiar ruta completa", "Copy full path"));
}

#[test]
fn right_click_on_file_row_has_no_exclude_entry() {
    let mut h = harness();
    h.get_by_label("big.iso").click_secondary();
    h.run();
    get(&h, "Copiar ruta completa", "Copy full path");
    assert!(h.query_by_label_contains("Exclude from duplicates").is_none() && h.query_by_label_contains("Excluir de duplicados").is_none());
    // Escape closes it.
    h.key_press(egui::Key::Escape);
    h.run();
    assert!(!h.state().t_menu_open());
}

#[test]
fn header_click_sorts_reverses_and_clears() {
    let mut h = harness();
    assert_eq!(h.state().t_tree_sort(), None);

    get(&h, "Tamaño", "Size").click();
    h.run();
    assert_eq!(h.state().t_tree_sort(), Some((Col::Size, false)));
    assert_eq!(names(&h), ["docs", "media", "big.iso", "noext"]);

    get_contains(&h, "Tamaño", "Size").click();
    h.run();
    assert_eq!(h.state().t_tree_sort(), Some((Col::Size, true)));
    // Ascending, still folders first.
    assert_eq!(names(&h), ["media", "docs", "noext", "big.iso"]);

    get_contains(&h, "Tamaño", "Size").click();
    h.run();
    assert_eq!(h.state().t_tree_sort(), None, "third click must clear the sort");
    assert_eq!(names(&h), ["docs", "media", "big.iso", "noext"]);
}

#[test]
fn sorting_by_folder_name_mixes_files_and_folders() {
    let mut h = harness();
    get(&h, "Carpeta", "Folder").click();
    h.run();
    assert_eq!(h.state().t_tree_sort(), Some((Col::Name, true)));
    assert_eq!(names(&h), ["big.iso", "docs", "media", "noext"]);
    // The "clear sort" button next to the tabs also resets it.
    get(&h, "✖ quitar orden", "✖ clear sort").click();
    h.run();
    assert_eq!(h.state().t_tree_sort(), None);
    assert_eq!(names(&h), ["docs", "media", "big.iso", "noext"]);
}

// ------------------------------------------------------------------ options / Groq key

#[test]
fn options_menu_opens_groq_key_dialog_that_accepts_typing() {
    let mut h = harness();
    assert!(!has(&h, "Cargar clave de Groq…", "Enter Groq key…"));
    get(&h, "⚙ Opciones", "⚙ Options").click();
    h.run();
    get(&h, "Cargar clave de Groq…", "Enter Groq key…").click();
    h.run();
    assert!(h.state().t_key_dialog().0, "the key dialog must open");
    // The menu closed so the dialog can take the keyboard.
    assert!(!has(&h, "Cargar clave de Groq…", "Enter Groq key…"));

    let field = h.get_by_role(Role::PasswordInput);
    assert!(field.is_focused(), "the key field should have keyboard focus");
    field.type_text("gsk_test123");
    h.run();
    assert_eq!(h.state().t_key_dialog(), (true, "gsk_test123"));

    get(&h, "Cancelar", "Cancel").click();
    h.run();
    assert_eq!(h.state().t_key_dialog(), (false, ""));
    assert!(h.query_by_role(Role::PasswordInput).is_none());
}

// ------------------------------------------------------------------ duplicates tab

fn open_dups(h: &mut Harness<'_, App>) {
    get(h, "Duplicados", "Duplicates").click();
    h.run();
}

#[test]
fn duplicates_mode_buttons_switch_mode() {
    let mut h = harness();
    open_dups(&mut h);
    assert_eq!(h.state().t_dup_mode(), (true, false), "Advanced is the default");

    get(&h, "Rápido", "Quick").click();
    h.run();
    assert_eq!(h.state().t_dup_mode(), (false, false));

    get(&h, "Versiones", "Versions").click();
    h.run();
    assert!(h.state().t_dup_mode().1);

    get(&h, "Avanzado", "Advanced").click();
    h.run();
    assert_eq!(h.state().t_dup_mode(), (true, false));

    get(&h, "Versiones", "Versions").click();
    h.run();
    get(&h, "Rápido", "Quick").click();
    h.run();
    assert_eq!(h.state().t_dup_mode(), (false, false), "Quick must leave Versions mode");
}

#[test]
fn duplicates_excluded_folders_editor_types_and_clears() {
    let mut h = harness();
    open_dups(&mut h);
    assert!(h.query_by_role(Role::MultilineTextInput).is_none());
    get_contains(&h, "Carpetas excluidas", "Excluded folders").click();
    h.run();

    let edit = h.get_by_role(Role::MultilineTextInput);
    edit.click();
    h.run();
    h.get_by_role(Role::MultilineTextInput).type_text("Q9:\\docs\\");
    h.run();
    h.key_press(egui::Key::Enter);
    h.run();
    h.get_by_role(Role::MultilineTextInput).type_text("Q9:/media");
    h.run();
    let (list, text) = h.state().t_dup_exclude();
    assert_eq!(text, "Q9:\\docs\\\nQ9:/media");
    // Normalised: backslashes, no trailing separator.
    assert_eq!(list, ["Q9:\\docs", "Q9:\\media"]);
    // The counter in the toggle follows.
    get_contains(&h, "Carpetas excluidas (2)", "Excluded folders (2)");

    get(&h, "✖ Vaciar", "✖ Clear").click();
    h.run();
    let (list, text) = h.state().t_dup_exclude();
    assert!(list.is_empty() && text.is_empty());
    get_contains(&h, "Carpetas excluidas (0)", "Excluded folders (0)");
}

// ------------------------------------------------------------------ file types panel

#[test]
fn clicking_file_type_category_highlights_it() {
    let mut h = harness();
    h.state_mut().t_show_exts(true);
    h.run();
    let docs = app::CATEGORIES.iter().position(|c| c.1 == "Documents").unwrap();
    assert_eq!(h.state().t_highlight_cat(), None);

    get(&h, "Documentos", "Documents").click();
    h.run();
    assert_eq!(h.state().t_highlight_cat(), Some(docs));
    assert!(h.state().t_ext_open(docs), "the category also expands");
    // Its extensions are listed now.
    assert!(h.query_by_label_contains(".pdf").is_some() || h.query_by_label_contains("pdf").is_some());

    // Clicking it again clears the highlight.
    get(&h, "Documentos", "Documents").click();
    h.run();
    assert_eq!(h.state().t_highlight_cat(), None);
}
