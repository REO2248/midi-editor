//! Menu content as data: each open dropdown (and its cascading submenu) is
//! built as a `Vec<MenuRow>`, so the same model drives mouse rendering and
//! keyboard navigation (arrows, Enter, Escape) without duplicating items.

use crate::{EditorView, Sub, TopMenu};
use gpui_kit::*;
use std::rc::Rc;

/// Menubar entries: (menu, i18n label key, label width). The bar starts at
/// x=96 (app-title gutter); dropdown x origins derive from these widths.
pub const MENUS: [(TopMenu, &str, f32); 7] = [
    (TopMenu::File, "menu.file", 46.0),
    (TopMenu::Edit, "menu.edit", 46.0),
    (TopMenu::View, "menu.view", 52.0),
    (TopMenu::Track, "menu.track", 58.0),
    (TopMenu::Output, "menu.output", 62.0),
    (TopMenu::Transport, "menu.transport", 90.0),
    (TopMenu::Help, "menu.help", 50.0),
];

/// Left edge of the dropdown under menubar entry `m`.
pub fn menu_x(m: TopMenu) -> f32 {
    let i = MENUS.iter().position(|(mm, _, _)| *mm == m).unwrap_or(0);
    96.0 + MENUS[..i].iter().map(|(_, _, w)| w).sum::<f32>()
}

/// A row action shared by mouse click and keyboard activation.
pub type MenuAct = Rc<dyn Fn(&mut EditorView, &mut Window, &mut Context<EditorView>)>;

/// One row in an open dropdown or cascading submenu.
#[derive(Clone)]
pub enum MenuRow {
    Sep,
    Head(SharedString),
    /// Row that opens a cascading submenu.
    Sub {
        id: ElementId,
        label: SharedString,
        sub: Sub,
    },
    Leaf(LeafRow),
}

/// An activatable dropdown row.
#[derive(Clone)]
pub struct LeafRow {
    /// Stable id so tests and AT tools can locate the row by item name.
    pub id: ElementId,
    pub label: SharedString,
    /// Right-aligned hint text (shortcut or status badge).
    pub shortcut: SharedString,
    pub check: Option<bool>,
    /// Color for `shortcut` when it carries status (plugin badges).
    pub badge_color: Option<u32>,
    /// Extra detail rendered as a tooltip on the row (plugin failures).
    pub detail: Option<SharedString>,
    pub enabled: bool,
    pub act: MenuAct,
}

impl MenuRow {
    /// Keyboard selection can land on submenus and enabled leaves.
    pub fn selectable(&self) -> bool {
        match self {
            MenuRow::Sep | MenuRow::Head(_) => false,
            MenuRow::Sub { .. } => true,
            MenuRow::Leaf(l) => l.enabled,
        }
    }
}

/// Next selectable row after `from` (exclusive) walking `dir` (+1/-1),
/// wrapping around the ends. `from = None` starts at the first/last row.
pub fn next_selectable(rows: &[MenuRow], from: Option<usize>, dir: i32) -> Option<usize> {
    let n = rows.len();
    if n == 0 {
        return None;
    }
    let step = |i: usize| (i as i32 + dir).rem_euclid(n as i32) as usize;
    let mut i = match from {
        Some(f) => step(f),
        None => {
            if dir >= 0 {
                0
            } else {
                n - 1
            }
        }
    };
    for _ in 0..n {
        if rows[i].selectable() {
            return Some(i);
        }
        i = step(i);
    }
    None
}

/// Painted row heights — the keyboard-opened cascade needs the parent row's
/// y to align its top, so heights must match the styles in render.rs.
pub fn row_h(row: &MenuRow) -> f32 {
    match row {
        MenuRow::Sep => 9.0,
        MenuRow::Head(_) => 18.0,
        _ => 24.0,
    }
}

/// Window-space y of the center of row `i` inside its dropdown (dropdown
/// top is window y=0 with 4px vertical padding).
pub fn row_y(rows: &[MenuRow], i: usize) -> f32 {
    4.0 + rows[..i.min(rows.len())].iter().map(row_h).sum::<f32>() + 12.0
}

#[cfg(test)]
mod tests {
    use super::{next_selectable, row_y, LeafRow, MenuAct, MenuRow};
    use crate::EditorView;
    use gpui_kit::{Context, Window};
    use std::rc::Rc;

    fn leaf_row() -> LeafRow {
        LeafRow {
            id: "test.row".into(),
            label: "x".into(),
            shortcut: "".into(),
            check: None,
            badge_color: None,
            detail: None,
            enabled: true,
            act: Rc::new(|_: &mut EditorView, _: &mut Window, _: &mut Context<EditorView>| {})
                as MenuAct,
        }
    }

    fn leaf() -> MenuRow {
        MenuRow::Leaf(leaf_row())
    }

    #[test]
    fn next_selectable_skips_seps_heads_and_disabled() {
        let mut dis = leaf_row();
        dis.enabled = false;
        let rows = vec![
            MenuRow::Sep,
            MenuRow::Head("h".into()),
            leaf(),
            MenuRow::Sep,
            MenuRow::Leaf(dis),
            leaf(),
        ];
        // only indexes 2 and 5 are selectable
        assert_eq!(next_selectable(&rows, None, 1), Some(2));
        assert_eq!(next_selectable(&rows, Some(2), 1), Some(5));
        assert_eq!(next_selectable(&rows, Some(5), 1), Some(2)); // wraps
        assert_eq!(next_selectable(&rows, None, -1), Some(5));
        assert_eq!(next_selectable(&rows, Some(2), -1), Some(5));
    }

    #[test]
    fn next_selectable_empty_and_all_inert() {
        assert_eq!(next_selectable(&[], None, 1), None);
        let rows = vec![MenuRow::Sep, MenuRow::Head("h".into())];
        assert_eq!(next_selectable(&rows, None, 1), None);
    }

    #[test]
    fn row_y_stacks_painted_heights() {
        let rows = vec![
            leaf(),                    // 24
            MenuRow::Sep,              // 9
            MenuRow::Head("h".into()), // 18
            leaf(),                    // 24
        ];
        assert_eq!(row_y(&rows, 0), 16.0);
        assert_eq!(row_y(&rows, 1), 40.0);
        assert_eq!(row_y(&rows, 2), 49.0);
        assert_eq!(row_y(&rows, 3), 67.0);
    }
}
