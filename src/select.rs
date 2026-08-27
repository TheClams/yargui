#[derive(Clone, Debug, Default)]
pub enum SelectedItem {#[default]
    None, Reg(String), Field(String)
}

impl SelectedItem {
    pub fn name(&self) -> Option<&str> {
        match self {
            SelectedItem::None => None,
            SelectedItem::Reg(s) => Some(s),
            SelectedItem::Field(s) => Some(s),
        }
    }

    pub fn is_reg(&self) -> bool {
        matches!(self,Self::Reg(_))
    }

    pub fn is_field(&self) -> bool {
        matches!(self,Self::Field(_))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub path: Vec<String>,
    pub item: SelectedItem,
    pub updt: usize,
}

/// A navigation the user attempted while a different RIF type still had edits pending
#[derive(Clone, Debug)]
pub struct PendingRifSwitch {
    pub target: Selection,
    pub from_rif: String,
}

impl Selection {
    /// Set current path and set the updated flag to ensure scrolling
    pub fn set_path(&mut self, path: Vec<String>) {
        self.path = path;
        self.updt = 1;
    }

    /// Flag when a path is partially matching the selected one
    pub fn matching(&self, path: &[String]) -> bool {
        self.updt != 0 &&
        self.path.len() >= path.len() &&
        self.path.iter()
            .zip(path)
            .all(|(x, y)| x == y)
    }
}