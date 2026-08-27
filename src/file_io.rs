use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use yarig::comp::comp_inst::Comp;
use yarig::parser::{ParserCfg, RifGenSrc, RifGenTop, RsvdKeywordSel, find_rif_file, get_rif, remove_rif};
use yarig::rifgen::{
    DescBlockKind, EnumDef, EnumEntry, Field, FieldProp, RegDef,
    RegInst, RegOverrideProp, RegProp, Rif, RifPage, RifProp,
};

use crate::apply_pending::RifClockLineKind;
use crate::RifViewer;

impl RifViewer {
    pub fn open_file(&mut self) {
        self.selected.path.clear();
        self.validated_selection = self.selected.clone();
        self.pending = None;
        self.edit_err = None;
        self.confirm_delete_reg = None;
        self.confirm_reg_addr_conflict = None;
        self.confirm_switch_rif = None;
        self.clear_pending_edits();
        self.confirm_reload = false;
        let rsvd_sel = RsvdKeywordSel { sv: false, vhdl: false, error: false};
        let parser_cfg = ParserCfg::new(rsvd_sel, false);
        match RifGenSrc::from_file(&self.file_path, &[], &parser_cfg) {
            Ok(src) => {
                // TODO: parameters/suffixes could come from a json file ?
                match Comp::compile(&src, &self.suffixes, &self.params) {
                    Ok(o) => {
                        self.selected.path.push(o.get_name().to_owned());
                        self.rif_comp = Some(o);
                        self.last_err = None;
                    },
                    Err(e) => {
                        self.rif_comp = None;
                        self.last_err = Some(e);
                    },
                }
                // Save source (in the future the GUI should be able to edit the source)
                self.rif_src = Some(src);
            }
            Err(e) => {
                self.rif_src = None;
                self.rif_comp = None;
                self.last_err = Some(e.to_string());
            },
        }
    }

    /// Recompile the component view from the (possibly edited) source.
    /// On failure the previous view is kept and the error surfaced via `edit_err`.
    pub fn recompile(&mut self) {
        let Some(src) = self.rif_src.as_ref() else { return; };
        match Comp::compile(src, &self.suffixes, &self.params) {
            Ok(o) => { self.rif_comp = Some(o); self.edit_err = None; }
            Err(e) => { self.edit_err = Some(e); }
        }
    }

    /// Resolve the `.rif` file that declares `rif_type`.
    fn resolve_rif_file(&self, rif_type: &str) -> Option<PathBuf> {
        let src = self.rif_src.as_ref()?;
        let top_name = match &src.top {
            RifGenTop::Rif(n) | RifGenTop::Rifmux(n) => Some(n.as_str()),
            RifGenTop::None => None,
        };
        if top_name == Some(rif_type) {
            return Some(self.file_path.clone());
        }
        let dir = src.paths.get(remove_rif(rif_type))?;
        find_rif_file(rif_type, std::slice::from_ref(dir))
    }

    /// Save RIF modification to its original source
    pub fn save_file(&mut self) {
        // Apply all pending edition
        self.flush_field_editor();
        self.flush_reg_editor();
        self.flush_reg_intr_desc_editor();
        self.flush_field_intr_desc_editor();
        self.flush_reg_override_editor();
        self.flush_field_override_editor();
        self.flush_rif_editor();
        // Check for error after flush
        if self.confirm_reg_addr_conflict.is_some()
            || self.field_editor.as_ref().is_some_and(|e| e.parse_err.is_some())
            || self.reg_editor.as_ref().is_some_and(|e| e.parse_err.is_some())
            || self.reg_override_editor.as_ref().is_some_and(|e| e.parse_err.is_some())
            || self.field_override_editor.as_ref().is_some_and(|e| e.parse_err.is_some())
            || self.rif_editor.as_ref().is_some_and(|e| e.parse_err.is_some())
        {
            return;
        }
        if !self.has_unsaved() {
            return;
        }
        let Some(rif_type) = self.editing_rif.clone() else {
            self.edit_err = Some("Internal error: unsaved edits are pending but no RIF is tracked as being edited".to_owned());
            return;
        };
        if let Err(e) = self.save_rif_file(&rif_type) {
            self.edit_err = Some(e);
            return;
        }
        // Re-parse the whole tree from the top
        let rsvd_sel = RsvdKeywordSel { sv: false, vhdl: false, error: false };
        let parser_cfg = ParserCfg::new(rsvd_sel, false);
        self.clear_pending_edits();
        match RifGenSrc::from_file(&self.file_path, &[], &parser_cfg) {
            Ok(new_src) => match Comp::compile(&new_src, &self.suffixes, &self.params) {
                Ok(comp) => { self.rif_comp = Some(comp); self.rif_src = Some(new_src); self.edit_err = None; }
                Err(e) => self.edit_err = Some(format!("Saved, but recompile failed: {e}")),
            },
            Err(e) => self.edit_err = Some(format!("Saved, but re-parse failed: {e}")),
        }
    }

    /// Write one RIF's pending field/enum edits back to its own source file
    fn save_rif_file(&self, rif_type: &str) -> Result<(), String> {
        let src = self.rif_src.as_ref().ok_or("No source loaded")?;
        let rif = get_rif(&src.rifs, rif_type).ok_or_else(|| format!("Unknown RIF '{rif_type}'"))?;
        let file_path = self.resolve_rif_file(rif_type)
            .ok_or_else(|| format!("Could not locate the source file defining '{rif_type}'"))?;
        let content = std::fs::read_to_string(&file_path).map_err(|e| format!("Read failed: {e}"))?;
        let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };
        let orig: Vec<String> = content.split(newline).map(str::to_owned).collect();

        debug_assert_eq!(Some(rif_type), self.editing_rif.as_deref(), "save_rif_file must only ever target the single RIF type with edits pending");
        let dirty = &self.dirty;
        let field_deleted = &self.deleted;
        let prop_edits = &self.prop_edits;
        let desc_block_edits = &self.desc_block_edits;
        let enum_dirty = &self.enum_dirty;
        let enum_deleted = &self.enum_deleted;
        let reg_dirty = &self.reg_dirty;
        let reg_prop_edits = &self.reg_prop_edits;
        let reg_desc_block_edits = &self.reg_desc_block_edits;
        let reg_inst_dirty = &self.reg_inst_dirty;
        let reg_deleted = &self.reg_deleted;
        let reg_inst_deleted = &self.reg_inst_deleted;
        let reg_intr_desc_block_edits = &self.reg_intr_desc_block_edits;
        let field_intr_desc_block_edits = &self.field_intr_desc_block_edits;
        let reg_override_edits = &self.reg_override_edits;
        let reg_override_desc_block_edits = &self.reg_override_desc_block_edits;
        let field_override_edits = &self.field_override_edits;
        let rif_dirty = &self.rif_dirty;
        let rif_prop_edits = &self.rif_prop_edits;
        let clock_line_edits = &self.rif_clock_line_edits;
        let reset_deleted = &self.rif_reset_deleted;
        let reset_new_lines = &self.rif_reset_new_lines;
        let param_edits = &self.rif_param_edits;
        let generic_edits = &self.rif_generic_edits;
        let param_inserts = &self.rif_param_inserts;
        let generic_inserts = &self.rif_generic_inserts;

        // Operations keyed on ORIGINAL 0-based indices, so shifts never invalidate each other
        let mut replacement: HashMap<usize, String> = HashMap::new();
        let mut insert_before: HashMap<usize, Vec<String>> = HashMap::new();
        let mut deleted: HashSet<usize> = HashSet::new();

        // Rif-level declaration line
        if let Some(line) = rif.src.decl_line
            && (1..=orig.len()).contains(&line)
        {
            if rif_dirty.contains(&line) {
                replacement.insert(line - 1, rif.fmt_decl());
            }
            for prop in RifProp::ALL {
                let Some(body) = rif_prop_edits.get(&prop) else { continue };
                match (rif.src.prop_lines.get(&prop).copied(), body) {
                    (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                        let indent = leading_ws(&orig[pl - 1]);
                        replacement.insert(pl - 1, format!("{indent}{text}"));
                    }
                    (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                        deleted.insert(pl - 1);
                    }
                    (None, Some(text)) => {
                        let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                        insert_before.entry(line).or_default().push(format!("{indent}{text}"));
                    }
                    _ => {}
                }
            }
            {
                let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                reconcile_desc_block(&orig, rif.src.desc_blocks.get(&DescBlockKind::Public).copied(), &self.rif_desc_block_edits, line, &insert_indent, &mut deleted, &mut insert_before);
            }
            // Clocking
            let sw_clock_line = rif.src.prop_lines.get(&RifProp::SwClock).copied();
            let hw_clock_line = rif.src.prop_lines.get(&RifProp::HwClock).copied();
            let sw_clock_anchor = sw_clock_line.unwrap_or(line);
            let hw_clock_anchor = hw_clock_line.unwrap_or(line);
            for (kind, existing_line, anchor) in [
                (RifClockLineKind::SwClock, sw_clock_line, line),
                (RifClockLineKind::HwClock, hw_clock_line, line),
                (RifClockLineKind::SwClkEn, rif.src.prop_lines.get(&RifProp::SwClkEn).copied(), sw_clock_anchor),
                (RifClockLineKind::HwClkEn, rif.src.prop_lines.get(&RifProp::HwClkEn).copied(), hw_clock_anchor),
                (RifClockLineKind::SwClear, rif.src.prop_lines.get(&RifProp::SwClear).copied(), sw_clock_anchor),
                (RifClockLineKind::HwClear, rif.src.prop_lines.get(&RifProp::HwClear).copied(), hw_clock_anchor),
            ] {
                let Some(body) = clock_line_edits.get(&kind) else { continue };
                match (existing_line, body) {
                    (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                        let indent = leading_ws(&orig[pl - 1]);
                        replacement.insert(pl - 1, format!("{indent}{text}"));
                    }
                    (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                        deleted.insert(pl - 1);
                    }
                    (None, Some(text)) => {
                        let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                        insert_before.entry(anchor).or_default().push(format!("{indent}{text}"));
                    }
                    _ => {}
                }
            }
            // Per-clock `swReset:`/`hwReset:`
            for &pl in reset_deleted {
                if (1..=orig.len()).contains(&pl) {
                    deleted.insert(pl - 1);
                }
            }
            for (hw_flag, anchor) in [(false, sw_clock_anchor), (true, hw_clock_anchor)] {
                let Some(lines) = reset_new_lines.get(&hw_flag) else { continue };
                if !lines.is_empty() {
                    let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                    insert_before.entry(anchor).or_default().extend(lines.iter().map(|t| format!("{indent}{t}")));
                }
            }
            // Parameters/generics
            let body_indent = format!("{}  ", leading_ws(&orig[line - 1]));
            let entry_indent = format!("{body_indent}  ");
            for &pl in param_edits.keys() {
                if !(1..=orig.len()).contains(&pl) { continue; }
                match &param_edits[&pl] {
                    Some(text) => {
                        let indent = leading_ws(&orig[pl - 1]);
                        replacement.insert(pl - 1, format!("{indent}{text}"));
                    }
                    None => { deleted.insert(pl - 1); }
                }
            }
            if !param_inserts.is_empty() {
                let new_lines: Vec<String> = param_inserts.iter().map(|t| format!("{entry_indent}{t}")).collect();
                match rif.src.param_lines.values().copied().max().or(rif.src.params_header_line) {
                    Some(anchor) if (1..=orig.len()).contains(&anchor) => {
                        insert_before.entry(anchor).or_default().extend(new_lines);
                    }
                    // No `parameters:` section exists yet — emit the header too.
                    _ => {
                        let mut lines = vec![format!("{body_indent}parameters :")];
                        lines.extend(new_lines);
                        insert_before.entry(line).or_default().extend(lines);
                    }
                }
            }
            for &pl in generic_edits.keys() {
                if !(1..=orig.len()).contains(&pl) { continue; }
                match &generic_edits[&pl] {
                    Some(text) => {
                        let indent = leading_ws(&orig[pl - 1]);
                        replacement.insert(pl - 1, format!("{indent}{text}"));
                    }
                    None => { deleted.insert(pl - 1); }
                }
            }
            if !generic_inserts.is_empty() {
                let new_lines: Vec<String> = generic_inserts.iter().map(|t| format!("{entry_indent}{t}")).collect();
                match rif.src.generic_lines.values().copied().max().or(rif.src.generics_header_line) {
                    Some(anchor) if (1..=orig.len()).contains(&anchor) => {
                        insert_before.entry(anchor).or_default().extend(new_lines);
                    }
                    _ => {
                        let mut lines = vec![format!("{body_indent}generics :")];
                        lines.extend(new_lines);
                        insert_before.entry(line).or_default().extend(lines);
                    }
                }
            }
        }

        // Deleted fields: drop their whole source block
        for &line in field_deleted {
            if (1..=orig.len()).contains(&line) {
                let last = field_block_last_idx(&orig, line - 1);
                for idx in (line - 1)..=last {
                    deleted.insert(idx);
                }
            }
        }
        // Deleted registers
        for &line in reg_deleted {
            if (1..=orig.len()).contains(&line) {
                let last = field_block_last_idx(&orig, line - 1);
                for idx in (line - 1)..=last {
                    deleted.insert(idx);
                }
            }
        }
        for regdef in rif.pages.iter()
            .flat_map(|p| p.registers.iter())
            .filter_map(|r| r.get_regdef())
        {
            // Register (type) declaration line + pulse sub-properties.
            if let Some(line) = regdef.src.decl_line
                && (1..=orig.len()).contains(&line)
            {
                if reg_dirty.contains(&line) {
                    let indent = leading_ws(&orig[line - 1]);
                    replacement.insert(line - 1, regdef.fmt_decl(&indent));
                }
                if let Some(edits) = reg_prop_edits.get(&line) {
                    for prop in RegProp::ALL {
                        let Some(body) = edits.get(&prop) else { continue };
                        match (regdef.src.prop_lines.get(&prop).copied(), body) {
                            (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                                let indent = leading_ws(&orig[pl - 1]);
                                replacement.insert(pl - 1, format!("{indent}{text}"));
                            }
                            (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                                deleted.insert(pl - 1);
                            }
                            (None, Some(text)) => {
                                // Add alt register after primary interrupt declaration
                                let anchor = if prop == RegProp::InterruptAlt {
                                    regdef.src.prop_lines.get(&RegProp::Interrupt).copied().unwrap_or(line)
                                } else {
                                    line
                                };
                                let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                                insert_before.entry(anchor).or_default().push(format!("{indent}{text}"));
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(body) = reg_desc_block_edits.get(&line) {
                    let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                    reconcile_desc_block(&orig, regdef.src.desc_blocks.get(&DescBlockKind::Public).copied(), body, line, &insert_indent, &mut deleted, &mut insert_before);
                }
                if let Some(edits) = reg_intr_desc_block_edits.get(&line) {
                    let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                    for (&kind, body) in edits {
                        reconcile_desc_block(&orig, regdef.src.intr_desc_range(kind), body, line, &insert_indent, &mut deleted, &mut insert_before);
                    }
                }
            }
            for f in &regdef.fields {
                let Some(line) = f.src.decl_line else { continue };
                if !(1..=orig.len()).contains(&line) { continue; }
                // Edited declaration lines regenerated in place
                if dirty.contains(&line) {
                    let indent = leading_ws(&orig[line - 1]);
                    replacement.insert(line - 1, f.fmt_decl(&indent));
                }
                // Sub-property line reconciliation: replace / delete / insert
                if let Some(edits) = prop_edits.get(&line) {
                    for (prop, body) in edits {
                        match (f.src.prop_lines.get(prop).copied(), body) {
                            (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                                let indent = leading_ws(&orig[pl - 1]);
                                replacement.insert(pl - 1, format!("{indent}{text}"));
                                // A newly-picked enum name may itself be a brand new definition
                                // (never written): its entries piggyback right after the header.
                                if *prop == FieldProp::EnumKind
                                    && let Some(name) = f.enum_kind.name()
                                {
                                    let entry_indent = format!("{indent}  ");
                                    let lines = new_enum_entry_lines(&entry_indent, name, &rif.enum_defs);
                                    insert_before.entry(pl).or_default().extend(lines);
                                }
                            }
                            (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                                deleted.insert(pl - 1);
                            }
                            (None, Some(text)) => {
                                let sub_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                                let at = field_block_last_idx(&orig, line - 1) + 1;
                                let mut lines_to_insert = vec![format!("{sub_indent}{text}")];
                                if *prop == FieldProp::EnumKind
                                    && let Some(name) = f.enum_kind.name()
                                {
                                    let entry_indent = format!("{sub_indent}  ");
                                    lines_to_insert.extend(new_enum_entry_lines(&entry_indent, name, &rif.enum_defs));
                                }
                                insert_before.entry(at).or_default().extend(lines_to_insert);
                            }
                            _ => {} // no existing line to delete, or an out-of-range index
                        }
                    }
                }
                if let Some(body) = desc_block_edits.get(&line) {
                    let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                    let at = field_block_last_idx(&orig, line - 1) + 1;
                    reconcile_desc_block(&orig, f.src.desc_blocks.get(&DescBlockKind::Public).copied(), body, at, &insert_indent, &mut deleted, &mut insert_before);
                }
                if let Some(edits) = field_intr_desc_block_edits.get(&line) {
                    let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                    let at = field_block_last_idx(&orig, line - 1) + 1;
                    for (&kind, body) in edits {
                        reconcile_desc_block(&orig, f.src.intr_desc_range(kind), body, at, &insert_indent, &mut deleted, &mut insert_before);
                    }
                }
            }
            // New fields inserted after the register's last existing field block
            let new_fields: Vec<&Field> = regdef.fields.iter().filter(|f| f.src.decl_line.is_none()).collect();
            if !new_fields.is_empty()
                && let Some((at, indent)) = field_block_end(&orig, regdef)
            {
                let sub_indent = format!("{indent}  ");
                let new_lines: Vec<String> = new_fields.iter().flat_map(|f| {
                    let mut ls = vec![f.fmt_decl(&indent)];
                    ls.extend(f.fmt_prop_all(&sub_indent));
                    if let Some(block) = f.fmt_desc_block(&sub_indent) {
                        ls.extend(block);
                    }
                    if let Some(name) = f.enum_kind.name() {
                        let entry_indent = format!("{sub_indent}  ");
                        ls.extend(new_enum_entry_lines(&entry_indent, name, &rif.enum_defs));
                    }
                    ls
                }).collect();
                insert_before.entry(at).or_default().extend(new_lines);
            }
        }

        // Deleted instances: drop their whole source block
        for &line in reg_inst_deleted {
            if (1..=orig.len()).contains(&line) {
                let last = field_block_last_idx(&orig, line - 1);
                for idx in (line - 1)..=last {
                    deleted.insert(idx);
                }
            }
        }
        for page in &rif.pages {
            // Check instances whose declaration changed
            for inst in &page.instances {
                let Some(line) = inst.src.decl_line else { continue };
                if !(1..=orig.len()).contains(&line) { continue; }
                if reg_inst_dirty.contains(&line)
                    && let Some(text) = inst.fmt_decl(&leading_ws(&orig[line - 1]))
                {
                    replacement.insert(line - 1, text);
                }
                // Whole-register-override sub-property lines
                if let Some(ovr) = inst.reg_override.get(&None) {
                    if let Some(edits) = reg_override_edits.get(&line) {
                        for prop in RegOverrideProp::ALL {
                            let Some(body) = edits.get(&prop) else { continue };
                            match (ovr.src.prop_lines.get(&prop).copied(), body) {
                                (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                                    let indent = leading_ws(&orig[pl - 1]);
                                    replacement.insert(pl - 1, format!("{indent}{text}"));
                                }
                                (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                                    deleted.insert(pl - 1);
                                }
                                (None, Some(text)) => {
                                    let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                                    insert_before.entry(line).or_default().push(format!("{indent}{text}"));
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(body) = reg_override_desc_block_edits.get(&line) {
                        let insert_indent = format!("{}  ", leading_ws(&orig[line - 1]));
                        reconcile_desc_block(&orig, ovr.src.desc_blocks.get(&DescBlockKind::Public).copied(), body, line, &insert_indent, &mut deleted, &mut insert_before);
                    }
                    // Field-override lines
                    if let Some(edits) = field_override_edits.get(&line) {
                        for ((field_name, prop), body) in edits {
                            let existing_pl = ovr.fields.get(field_name).and_then(|fo| fo.src.prop_lines.get(prop).copied());
                            match (existing_pl, body) {
                                (Some(pl), Some(text)) if (1..=orig.len()).contains(&pl) => {
                                    let indent = leading_ws(&orig[pl - 1]);
                                    replacement.insert(pl - 1, format!("{indent}{text}"));
                                }
                                (Some(pl), None) if (1..=orig.len()).contains(&pl) => {
                                    deleted.insert(pl - 1);
                                }
                                (None, Some(text)) => {
                                    let indent = format!("{}  ", leading_ws(&orig[line - 1]));
                                    insert_before.entry(line).or_default().push(format!("{indent}{text}"));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            // Check page for new register declaration
            let new_regs: Vec<&RegDef> = page.registers.iter().filter_map(|r| r.get_regdef())
                .filter(|d| d.src.decl_line.is_none()).collect();
            if !new_regs.is_empty()
                && let Some((at, indent)) = page_registers_block_end(&orig, page)
            {
                let sub_indent = format!("{indent}  ");
                let field_prop_indent = format!("{sub_indent}  ");
                let new_lines: Vec<String> = new_regs.iter().flat_map(|d| {
                    let mut ls = vec![d.fmt_decl(&indent)];
                    ls.extend(d.fmt_prop_all(&sub_indent));
                    if let Some(block) = d.fmt_desc_block(&sub_indent) {
                        ls.extend(block);
                    }
                    for f in &d.fields {
                        ls.push(f.fmt_decl(&sub_indent));
                        ls.extend(f.fmt_prop_all(&field_prop_indent));
                        if let Some(block) = f.fmt_desc_block(&field_prop_indent) {
                            ls.extend(block);
                        }
                        if let Some(name) = f.enum_kind.name() {
                            let entry_indent = format!("{field_prop_indent}  ");
                            ls.extend(new_enum_entry_lines(&entry_indent, name, &rif.enum_defs));
                        }
                    }
                    ls
                }).collect();
                insert_before.entry(at).or_default().extend(new_lines);
            }
            // Check pag for new instance
            let new_instances: Vec<&RegInst> = page.instances.iter().filter(|i| i.src.decl_line.is_none()).collect();
            if new_instances.is_empty() {
                continue;
            }
            let Some(header_line) = page.instances_decl.decl_line else { continue };
            if !(1..=orig.len()).contains(&header_line) {
                continue;
            }
            let header_indent = leading_ws(&orig[header_line - 1]);
            let canonical_header = format!("{header_indent}instances:");
            if orig[header_line - 1] != canonical_header {
                replacement.insert(header_line - 1, canonical_header);
            }
            if let Some((at, inst_indent)) = page_instances_block_end(&orig, page) {
                let sub_indent = format!("{inst_indent}  ");
                let lines: Vec<String> = new_instances.iter().flat_map(|i| {
                    let Some(decl) = i.fmt_decl(&inst_indent) else { return Vec::new() };
                    let mut ls = vec![decl];
                    if let Some(ovr) = i.reg_override.get(&None) {
                        ls.extend(ovr.fmt_prop_all(&sub_indent));
                        if let Some(block) = ovr.fmt_desc_block(&sub_indent) {
                            ls.extend(block);
                        }
                        // Field overrides need the field's own reset value for the disable-omission rule
                        if let Some(def) = rif.pages.iter().flat_map(|p| p.registers.iter())
                            .filter_map(|r| r.get_regdef()).find(|d| d.name == i.type_name)
                        {
                            for (field_name, field_ovr) in &ovr.fields {
                                if let Some(field_reset) = def.fields.iter().find(|f| &f.name == field_name).and_then(|f| f.reset.first()) {
                                    ls.extend(field_ovr.fmt_prop_all(field_name, &sub_indent, field_reset));
                                }
                            }
                        }
                    }
                    ls
                }).collect();
                insert_before.entry(at).or_default().extend(lines);
            }
        }

        // Enum entry edits for definitions whose header already exists in the file
        for def in &rif.enum_defs {
            for e in &def.values {
                let Some(line) = e.src.decl_line else { continue };
                if !(1..=orig.len()).contains(&line) { continue; }
                if enum_dirty.contains(&line) {
                    let indent = leading_ws(&orig[line - 1]);
                    replacement.insert(line - 1, format!("{indent}{}", e.to_rif()));
                }
            }
            let new_entries: Vec<&EnumEntry> = def.values.iter().filter(|e| e.src.decl_line.is_none()).collect();
            if !new_entries.is_empty()
                && let Some((at, indent)) = enum_block_end(&orig, def)
            {
                let lines: Vec<String> = new_entries.iter().map(|e| format!("{indent}{}", e.to_rif())).collect();
                insert_before.entry(at).or_default().extend(lines);
            }
        }
        for &line in enum_deleted {
            if (1..=orig.len()).contains(&line) {
                deleted.insert(line - 1);
            }
        }

        // Single rebuild pass
        let mut out: Vec<String> = Vec::with_capacity(orig.len());
        for (idx, line) in orig.iter().enumerate() {
            if let Some(ins) = insert_before.get(&idx) {
                out.extend(ins.iter().cloned());
            }
            if deleted.contains(&idx) {
                continue;
            }
            match replacement.get(&idx) {
                Some(r) => out.push(r.clone()),
                None => out.push(line.clone()),
            }
        }
        if let Some(ins) = insert_before.get(&orig.len()) {
            out.extend(ins.iter().cloned());
        }

        std::fs::write(&file_path, out.join(newline)).map_err(|e| format!("Save failed: {e}"))
    }

    fn rif_has_new_fields(rif: &Rif) -> bool {
        rif.pages.iter()
            .flat_map(|p| p.registers.iter())
            .filter_map(|r| r.get_regdef())
            .any(|d| d.fields.iter().any(|f| f.src.decl_line.is_none()))
    }

    fn rif_has_new_enum_entries(rif: &Rif) -> bool {
        rif.enum_defs.iter().any(|d| d.values.iter().any(|e| e.src.decl_line.is_none()))
    }

    /// True when any page has a register definition with no source line yet
    fn rif_has_new_registers(rif: &Rif) -> bool {
        rif.pages.iter()
            .flat_map(|p| p.registers.iter())
            .filter_map(|r| r.get_regdef())
            .any(|d| d.src.decl_line.is_none())
    }

    /// True when any page has an `instances:` entry with no source line yet
    fn rif_has_new_instances(rif: &Rif) -> bool {
        rif.pages.iter().any(|p| p.instances.iter().any(|i| i.src.decl_line.is_none()))
    }

    /// True when any RIF (top or any Rifmux sub-RIF) has a field created this session that is
    /// not yet in its file.
    fn has_new_fields(&self) -> bool {
        let Some(src) = self.rif_src.as_ref() else { return false; };
        src.rifs.values().any(Self::rif_has_new_fields)
    }

    /// True when any RIF (top or any Rifmux sub-RIF) has an enum entry (in any definition)
    /// created this session that is not yet in its file — mirrors `has_new_fields`.
    fn has_new_enum_entries(&self) -> bool {
        let Some(src) = self.rif_src.as_ref() else { return false; };
        src.rifs.values().any(Self::rif_has_new_enum_entries)
    }

    /// True when any RIF has a page converted to manual addressing this session that is not yet
    /// in its file — mirrors `has_new_fields`.
    fn has_new_instances(&self) -> bool {
        let Some(src) = self.rif_src.as_ref() else { return false; };
        src.rifs.values().any(Self::rif_has_new_instances)
    }

    /// True when any RIF has a register created this session that is not yet in its file —
    /// mirrors `has_new_fields`.
    fn has_new_registers(&self) -> bool {
        let Some(src) = self.rif_src.as_ref() else { return false; };
        src.rifs.values().any(Self::rif_has_new_registers)
    }

    /// True when there are edits not yet written to disk.
    pub fn has_unsaved(&self) -> bool {
        !self.dirty.is_empty() || !self.deleted.is_empty()
            || !self.prop_edits.is_empty() || !self.desc_block_edits.is_empty() || self.has_new_fields()
            || !self.enum_dirty.is_empty() || !self.enum_deleted.is_empty() || self.has_new_enum_entries()
            || !self.reg_dirty.is_empty() || !self.reg_prop_edits.is_empty() || !self.reg_desc_block_edits.is_empty()
            || self.has_new_instances()
            || !self.reg_inst_dirty.is_empty() || self.has_new_registers()
            || !self.reg_deleted.is_empty() || !self.reg_inst_deleted.is_empty()
            || !self.reg_intr_desc_block_edits.is_empty() || !self.field_intr_desc_block_edits.is_empty()
            || !self.reg_override_edits.is_empty() || !self.reg_override_desc_block_edits.is_empty()
            || !self.field_override_edits.is_empty()
            || !self.rif_dirty.is_empty() || !self.rif_prop_edits.is_empty() || self.rif_desc_block_edits.is_some()
            || !self.rif_clock_line_edits.is_empty()
            || !self.rif_reset_deleted.is_empty() || !self.rif_reset_new_lines.is_empty()
            || !self.rif_param_edits.is_empty() || !self.rif_param_inserts.is_empty()
            || !self.rif_generic_edits.is_empty() || !self.rif_generic_inserts.is_empty()
    }

    /// RIF type that currently owns a pending edit
    pub fn pending_edit_owner(&self) -> Option<String> {
        self.editing_rif.clone().or_else(|| {
            [
                self.rif_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.reg_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.field_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.reg_override_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.field_override_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.reg_intr_desc_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
                self.field_intr_desc_editor.as_ref().map(|e| (e.rif_type.as_str(), e.is_unchanged())),
            ]
            .into_iter()
            .flatten()
            .find(|(_, unchanged)| !unchanged)
            .map(|(rt, _)| rt.to_owned())
        })
    }

    /// Clears every pending-edit map and closes every inline editor
    pub fn clear_pending_edits(&mut self) {
        self.editing_rif = None;
        self.dirty.clear();
        self.deleted.clear();
        self.prop_edits.clear();
        self.desc_block_edits.clear();
        self.enum_dirty.clear();
        self.enum_deleted.clear();
        self.reg_dirty.clear();
        self.reg_prop_edits.clear();
        self.reg_desc_block_edits.clear();
        self.reg_inst_dirty.clear();
        self.reg_deleted.clear();
        self.reg_inst_deleted.clear();
        self.reg_intr_desc_block_edits.clear();
        self.field_intr_desc_block_edits.clear();
        self.reg_override_edits.clear();
        self.reg_override_desc_block_edits.clear();
        self.field_override_edits.clear();
        self.rif_dirty.clear();
        self.rif_prop_edits.clear();
        self.rif_desc_block_edits = None;
        self.rif_clock_line_edits.clear();
        self.rif_reset_deleted.clear();
        self.rif_reset_new_lines.clear();
        self.rif_param_edits.clear();
        self.rif_generic_edits.clear();
        self.rif_param_inserts.clear();
        self.rif_generic_inserts.clear();
        self.field_editor = None;
        self.enum_editor = None;
        self.reg_editor = None;
        self.reg_add_editor = None;
        self.reg_intr_desc_editor = None;
        self.field_intr_desc_editor = None;
        self.reg_override_editor = None;
        self.field_override_editor = None;
        self.rif_editor = None;
        self.rif_clocking_editor = None;
        self.rif_params_editor = None;
    }

}

/// Leading whitespace (indentation) of a source line.
fn leading_ws(s: &str) -> String {
    s.chars().take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r').collect()
}

/// 0-based index of the last source line belonging to the field declared at `decl_idx`:
fn field_block_last_idx(lines: &[String], decl_idx: usize) -> usize {
    let decl_w = leading_ws(&lines[decl_idx]).len();
    let mut last = decl_idx;
    let mut i = decl_idx + 1;
    while i < lines.len() {
        let line = &lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if leading_ws(line).len() > decl_w {
            last = i;
            i += 1;
        } else {
            break;
        }
    }
    last
}

/// Determine where to insert new field declarations
fn field_block_end(lines: &[String], regdef: &RegDef) -> Option<(usize, String)> {
    let last_line = regdef.fields.iter().filter_map(|f| f.src.decl_line).max()?;
    if last_line < 1 || last_line > lines.len() {
        return None;
    }
    let indent = leading_ws(&lines[last_line - 1]);
    let at = field_block_last_idx(lines, last_line - 1) + 1;
    Some((at, indent))
}

/// Reconcile a pending `description:` block edit against its tracked source range
fn reconcile_desc_block(
    orig: &[String],
    existing: Option<(usize, usize)>,
    body: &Option<Vec<String>>,
    insert_at: usize,
    insert_indent: &str,
    deleted: &mut HashSet<usize>,
    insert_before: &mut HashMap<usize, Vec<String>>,
) {
    match (existing, body) {
        (Some((s, e)), _) if (1..=orig.len()).contains(&s) && (1..=orig.len()).contains(&e) => {
            for idx in (s - 1)..=(e - 1) {
                deleted.insert(idx);
            }
            if let Some(lines) = body {
                let indent = leading_ws(&orig[s - 1]);
                insert_before.entry(s - 1).or_default().extend(lines.iter().map(|l| format!("{indent}{l}")));
            }
        }
        (None, Some(lines)) => {
            insert_before.entry(insert_at).or_default()
                .extend(lines.iter().map(|l| format!("{insert_indent}{l}")));
        }
        _ => {}
    }
}

/// Determine where to insert new entries for an enum definition already existig
fn enum_block_end(lines: &[String], def: &EnumDef) -> Option<(usize, String)> {
    let decl_line = def.src.decl_line?;
    if decl_line < 1 || decl_line > lines.len() {
        return None;
    }
    match def.values.iter().filter_map(|e| e.src.decl_line).filter(|&l| (1..=lines.len()).contains(&l)).max() {
        Some(last) => Some((last, leading_ws(&lines[last - 1]))),
        None => Some((decl_line, format!("{}  ", leading_ws(&lines[decl_line - 1])))),
    }
}

/// Determine where to insert a new register declaration in `page`'s `registers:` block
fn page_registers_block_end(lines: &[String], page: &RifPage) -> Option<(usize, String)> {
    let last_line = page.registers.iter().filter_map(|r| r.get_regdef()).filter_map(|d| d.src.decl_line).max()?;
    if last_line < 1 || last_line > lines.len() {
        return None;
    }
    let indent = leading_ws(&lines[last_line - 1]);
    let at = field_block_last_idx(lines, last_line - 1) + 1;
    Some((at, indent))
}

/// Determine where to insert a new instance declaration in `page`'s `instances:` block
fn page_instances_block_end(lines: &[String], page: &RifPage) -> Option<(usize, String)> {
    let header_line = page.instances_decl.decl_line?;
    if header_line < 1 || header_line > lines.len() {
        return None;
    }
    match page.instances.iter().filter_map(|i| i.src.decl_line).filter(|&l| (1..=lines.len()).contains(&l)).max() {
        Some(last) => Some((last, leading_ws(&lines[last - 1]))),
        None => Some((header_line, format!("{}  ", leading_ws(&lines[header_line - 1])))),
    }
}

/// Entry lines to emit right after a newly-inserted `enum [name]` header line
fn new_enum_entry_lines(indent: &str, name: &str, enum_defs: &[EnumDef]) -> Vec<String> {
    let Some(def) = enum_defs.iter().find(|d| d.name == name) else { return Vec::new(); };
    if def.src.decl_line.is_some() {
        return Vec::new();
    }
    def.values.iter().map(|e| format!("{indent}{}", e.to_rif())).collect()
}
