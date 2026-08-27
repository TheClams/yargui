use yarig::rifgen::{Field, InterruptRegKind, RegDef};

use crate::apply_pending::EditAction;

/// Live buffer for a derived (enable/mask/pending) interrupt register's own description editor
pub struct RegIntrDescEditor {
    pub rif_type: String,
    /// The *base* register's type name (a derived register has no `RegDef` of its own).
    pub reg_type: String,
    pub kind: InterruptRegKind,
    pub desc: String,
    pub desc_orig: String,
}

impl RegIntrDescEditor {
    pub fn from_regdef(rif_type: String, reg_type: String, def: &RegDef, kind: InterruptRegKind) -> Self {
        let desc = def.interrupt.first()
            .map(|intr| match kind {
                InterruptRegKind::Enable => intr.description.enable.get(true),
                InterruptRegKind::Mask => intr.description.mask.get(true),
                InterruptRegKind::Pending => intr.description.pending.get(true),
                _ => String::new(),
            })
            .unwrap_or_default();
        RegIntrDescEditor { rif_type, reg_type, kind, desc: desc.clone(), desc_orig: desc }
    }

    pub fn is_unchanged(&self) -> bool {
        self.desc == self.desc_orig
    }

    pub fn build_action(&self) -> EditAction {
        EditAction::UpdateRegIntrDesc {
            rif_type: self.rif_type.clone(),
            reg_type: self.reg_type.clone(),
            kind: self.kind,
            desc: self.desc.clone(),
        }
    }
}

/// Same as `RegIntrDescEditor`, at field level
pub struct FieldIntrDescEditor {
    pub rif_type: String,
    pub reg_type: String,
    pub field_name: String,
    pub kind: InterruptRegKind,
    pub desc: String,
    pub desc_orig: String,
}

impl FieldIntrDescEditor {
    pub fn from_field(rif_type: String, reg_type: String, field_name: String, def: &Field, kind: InterruptRegKind) -> Self {
        let desc = def.intr_desc.as_ref()
            .map(|d| match kind {
                InterruptRegKind::Enable => d.enable.get(true),
                InterruptRegKind::Mask => d.mask.get(true),
                InterruptRegKind::Pending => d.pending.get(true),
                _ => String::new(),
            })
            .unwrap_or_default();
        FieldIntrDescEditor { rif_type, reg_type, field_name, kind, desc: desc.clone(), desc_orig: desc }
    }

    pub fn is_unchanged(&self) -> bool {
        self.desc == self.desc_orig
    }

    pub fn build_action(&self) -> EditAction {
        EditAction::UpdateFieldIntrDesc {
            rif_type: self.rif_type.clone(),
            reg_type: self.reg_type.clone(),
            field_name: self.field_name.clone(),
            kind: self.kind,
            desc: self.desc.clone(),
        }
    }
}
