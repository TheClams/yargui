use std::ops::{Deref, DerefMut};

use yarig::comp::comp_inst::Comp;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SearchMatch {
    pub path: Vec<String>,
    pub is_description: bool,
    pub field_name: Option<String>,  // If a field matched, store its name
}

impl SearchMatch {
    /// Creates a SearchMatch for a register name match
    pub fn reg(path: &[String]) -> Self {
        SearchMatch {
            path: path.to_vec(),
            is_description: false,
            field_name: None,
        }
    }

    /// Creates a SearchMatch for a register description match
    pub fn reg_desc(path: &[String]) -> Self {
        SearchMatch {
            path: path.to_vec(),
            is_description: true,
            field_name: None,
        }
    }

    /// Creates a SearchMatch for a field name match
    pub fn field(path: &[String], field_name: &str) -> Self {
        SearchMatch {
            path: path.to_vec(),
            is_description: false,
            field_name: Some(field_name.to_owned()),
        }
    }

    /// Creates a SearchMatch for a field description match
    pub fn field_desc(path: &[String], field_name: &str) -> Self {
        SearchMatch {
            path: path.to_vec(),
            is_description: true,
            field_name: Some(field_name.to_owned()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchMatches(Vec<SearchMatch>);

impl Deref for SearchMatches {
    type Target = Vec<SearchMatch>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SearchMatches {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl SearchMatches {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Search through a component hierarchy for matches in names and/or descriptions
    // TODO: when a match occurs early in the path remove this levek when checking name in sub-level (starting at component level)
    pub fn search(comp: &Comp, current_path: Vec<String>, query: &str, search_name: bool, search_desc: bool) -> Self {
        let mut results = Self::new();
        let query_lower = query.to_lowercase();

        match comp {
            Comp::Rifmux(rifmux) => {
                let mut path = current_path.clone();
                path.push(rifmux.inst_name.clone());

                // Search in components
                for comp in &rifmux.components {
                    results.0.extend(Self::search(&comp.inst, path.clone(), query, search_name, search_desc).0);
                }
            }
            Comp::Rif(rif) => {
                let mut path = current_path.clone();
                path.push(rif.inst_name.clone());

                // Search in pages
                for page in &rif.pages {
                    let mut page_path = path.clone();
                    if rif.pages.len() > 1 {
                        page_path.push(page.name.clone());
                    }

                    // Search in registers
                    for reg in &page.regs {
                        let mut reg_path = page_path.clone();
                        reg_path.push(reg.reg_name.clone());

                        // Check register name and description
                        let reg_path_str = reg_path[1..].join(".");
                        let mut reg_name_matched = false;
                        if search_name && reg_path_str.to_lowercase().contains(&query_lower) {
                            results.0.push(SearchMatch::reg(&reg_path));
                            reg_name_matched = true;
                        }
                        if search_desc {
                            let desc = reg.description.get(false);
                            if desc.to_lowercase().contains(&query_lower) {
                                results.0.push(SearchMatch::reg_desc(&reg_path));
                            }
                        }

                        // Search in fields
                        for field in &reg.fields {
                            // Check field name and description
                            if search_name && !reg_name_matched {
                                let field_path = format!("{reg_path_str}.{}", field.name);
                                if field_path.to_lowercase().contains(&query_lower) {
                                    results.0.push(SearchMatch::field(&reg_path, &field.name));
                                }
                            }
                            if search_desc {
                                let desc = field.description.get(false);
                                if desc.to_lowercase().contains(&query_lower) {
                                    results.0.push(SearchMatch::field_desc(&reg_path, &field.name));
                                }
                            }
                        }
                    }
                }
            }
            Comp::External(_) => {
                // External components don't have registers or fields to search
            }
        }

        results
    }
}