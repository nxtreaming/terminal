#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteAction {
    NewTask,
    PreviousWork,
    ChangeBrowser,
    Context,
    Goal,
    ChooseModel,
    Authenticate,
    SyncCookies,
    Reload,
    Update,
    Exit,
    Feedback,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PaletteItem {
    pub(crate) command: &'static str,
    pub(crate) description: &'static str,
    pub(crate) action: PaletteAction,
}

const VISIBLE_ITEMS: [PaletteItem; 8] = [
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
        command: "/context",
        description: "inspect context window attribution",
        action: PaletteAction::Context,
    },
    PaletteItem {
        command: "/model",
        description: "choose model and provider",
        action: PaletteAction::ChooseModel,
    },
    PaletteItem {
        command: "/goal",
        description: "set or view the goal for a long-running task",
        action: PaletteAction::Goal,
    },
    PaletteItem {
        command: "/sync-cookies",
        description: "sync local cookies",
        action: PaletteAction::SyncCookies,
    },
    PaletteItem {
        command: "/feedback",
        description: "report a bug or share feedback",
        action: PaletteAction::Feedback,
    },
];

const HIDDEN_ITEMS: [PaletteItem; 4] = [
    PaletteItem {
        command: "/auth",
        description: "sign in to a provider",
        action: PaletteAction::Authenticate,
    },
    PaletteItem {
        command: "/reload",
        description: "restart the UI in this terminal",
        action: PaletteAction::Reload,
    },
    PaletteItem {
        command: "/update",
        description: "install the latest release",
        action: PaletteAction::Update,
    },
    PaletteItem {
        command: "/exit",
        description: "quit browser-use terminal",
        action: PaletteAction::Exit,
    },
];

pub(crate) const fn max_item_count() -> usize {
    VISIBLE_ITEMS.len()
}

pub(crate) fn items_filtered(filter: &str) -> Vec<PaletteItem> {
    let trimmed = filter.trim_start_matches('/').to_ascii_lowercase();
    if trimmed.is_empty() {
        return VISIBLE_ITEMS.to_vec();
    }
    VISIBLE_ITEMS
        .iter()
        .copied()
        .chain(HIDDEN_ITEMS.iter().copied())
        .filter(|item| item.command[1..].to_ascii_lowercase().contains(&trimmed))
        .collect()
}

pub(crate) fn selected_action(filter: &str, selected_row: usize) -> Option<PaletteAction> {
    items_filtered(filter)
        .get(selected_row)
        .map(|item| item.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_is_available_as_hidden_command() {
        assert_eq!(selected_action("/reload", 0), Some(PaletteAction::Reload));
    }

    #[test]
    fn sync_cookies_is_available_from_short_filter() {
        assert_eq!(
            selected_action("/sync", 0),
            Some(PaletteAction::SyncCookies)
        );
    }

    #[test]
    fn goal_is_available_from_palette() {
        assert_eq!(selected_action("/goal", 0), Some(PaletteAction::Goal));
    }

    #[test]
    fn context_is_available_from_palette() {
        assert_eq!(selected_action("/context", 0), Some(PaletteAction::Context));
    }
}
