#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    NewTask,
    PreviousWork,
    ChangeBrowser,
    ChooseModel,
    Authenticate,
    Update,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PaletteItem {
    pub(crate) command: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: PaletteAction,
}

const ITEMS: [PaletteItem; 6] = [
    PaletteItem {
        command: "/task",
        description: "start a new task",
        action: PaletteAction::NewTask,
    },
    PaletteItem {
        command: "/history",
        description: "browse previous tasks",
        action: PaletteAction::PreviousWork,
    },
    PaletteItem {
        command: "/browser",
        description: "change browser backend",
        action: PaletteAction::ChangeBrowser,
    },
    PaletteItem {
        command: "/model",
        description: "choose model and provider",
        action: PaletteAction::ChooseModel,
    },
    PaletteItem {
        command: "/auth",
        description: "sign in to a provider",
        action: PaletteAction::Authenticate,
    },
    PaletteItem {
        command: "/update",
        description: "install the latest release",
        action: PaletteAction::Update,
    },
];

pub(crate) const fn max_item_count() -> usize {
    ITEMS.len()
}

pub(crate) fn items_filtered(filter: &str) -> Vec<PaletteItem> {
    let trimmed = filter.trim_start_matches('/').to_ascii_lowercase();
    if trimmed.is_empty() {
        return ITEMS.to_vec();
    }
    ITEMS
        .iter()
        .copied()
        .filter(|item| item.command[1..].to_ascii_lowercase().contains(&trimmed))
        .collect()
}

pub(crate) fn selected_action(filter: &str, selected_row: usize) -> Option<PaletteAction> {
    items_filtered(filter)
        .get(selected_row)
        .map(|item| item.action)
}
