pub struct TreeNodeBuilder<'a> {
    id_str: &'a str,
    label: &'a str,
    is_selected: bool,
    default_open: bool,
    scroll: bool,
}

impl<'a> TreeNodeBuilder<'a> {
    pub fn new(id_str: &'a str, label: &'a str) -> Self {
        Self {
            id_str,
            label,
            is_selected: false,
            default_open: false,
            scroll: false,
        }
    }

    pub fn selected(mut self, is_selected: bool) -> Self {
        self.is_selected = is_selected;
        self
    }

    pub fn default_open(mut self, default_open: bool) -> Self {
        self.default_open = default_open;
        self
    }

    pub fn scroll(mut self, scroll: bool) -> Self {
        self.scroll = scroll;
        self
    }

    pub fn build<F>(
        self,
        ui: &mut egui::Ui,
        on_select: impl FnOnce(),
        add_contents: F
    ) where F: FnOnce(&mut egui::Ui) {
        let id = ui.make_persistent_id(format!("tree_node_{}", self.id_str));
        let rsp = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, self.default_open)
            .show_header(ui, |ui| {
                if ui.selectable_label(self.is_selected, self.label).clicked() {
                    on_select();
                }
            })
            .body(add_contents).0;
        if self.scroll {
            rsp.scroll_to_me(None);
        }
    }
}