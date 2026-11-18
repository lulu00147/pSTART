//! Slot-based path reconstruction.

use super::*;

impl MemoryIndex {
    pub fn resolve_record_path(&self, frn: Frn) -> Result<String, PathError> {
        let Some(start) = self.by_frn.get(frn.get()) else {
            return Err(PathError::NotFound { frn });
        };
        let slots = match walk_named(
            &self.parent_slots,
            &self.slot_present,
            &self.slot_name_valid,
            ROOT_SLOT,
            start,
            self.max_depth,
        ) {
            Ok(slots) => slots,
            Err(failure) => match failure.error {
                NamedWalkError::DepthExceeded => {
                    return Err(PathError::DepthExceeded {
                        max: self.max_depth,
                    });
                }
                NamedWalkError::Cycle { node } => {
                    return Err(PathError::Cycle {
                        frn: self.slot_frn_for_error(node, frn),
                    });
                }
                NamedWalkError::MissingParent { node } if node == MISSING_SLOT => {
                    return crate::path::resolve_path(
                        self,
                        &self.root,
                        self.root_frn,
                        frn,
                        self.max_depth,
                    );
                }
                NamedWalkError::MissingParent { node } => {
                    let child = failure
                        .visited
                        .last()
                        .and_then(|slot| self.slot_frn(*slot))
                        .unwrap_or(frn);
                    return Err(PathError::MissingParent {
                        child,
                        parent: self.slot_frn_for_error(node, frn),
                    });
                }
                NamedWalkError::InvalidName { node } => {
                    return Err(PathError::InvalidName {
                        frn: self.slot_frn(node).unwrap_or(frn),
                    });
                }
            },
        };

        if let Some(position) = slots.iter().position(|slot| {
            self.slots
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .is_none()
        }) {
            let missing = slots[position];
            let (child, parent) = if position == 0 {
                (frn, self.slot_frn(missing).unwrap_or(frn))
            } else {
                let child_record = self
                    .slots
                    .get(slots[position - 1] as usize)
                    .and_then(Option::as_ref)
                    .expect("path prefix is live");
                (
                    Frn::new(child_record.frn),
                    Frn::new(child_record.parent_frn),
                )
            };
            return Err(PathError::MissingParent { child, parent });
        }

        let normalized_root = self.root.trim_end_matches(['\\', '/']);
        let normalized_root = if normalized_root.is_empty() {
            "\\"
        } else {
            normalized_root
        };
        let mut path = String::from(normalized_root);
        for slot in slots.iter().rev() {
            let record = self
                .slots
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .ok_or(PathError::MissingParent {
                    child: frn,
                    parent: self.slot_frn(*slot).unwrap_or(frn),
                })?;
            if !crate::path::valid_name(&record.name) {
                return Err(PathError::InvalidName {
                    frn: Frn::new(record.frn),
                });
            }
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path.push_str(&record.name);
        }
        let length = path.encode_utf16().count();
        if length > MAX_PATH_UTF16 {
            return Err(PathError::PathTooLong {
                actual: length,
                max: MAX_PATH_UTF16,
            });
        }
        Ok(path)
    }

    fn slot_frn(&self, slot: u32) -> Option<Frn> {
        self.slots
            .get(slot as usize)
            .and_then(Option::as_ref)
            .map(|record| Frn::new(record.frn))
    }

    fn slot_frn_for_error(&self, slot: u32, fallback: Frn) -> Frn {
        if let Some(frn) = self.slot_frn(slot) {
            return frn;
        }
        self.slots
            .iter()
            .enumerate()
            .find_map(|(index, record)| {
                let record = record.as_ref()?;
                (self.parent_slots.get(index).copied() == Some(slot))
                    .then_some(Frn::new(record.parent_frn))
            })
            .unwrap_or(fallback)
    }

    pub(super) fn slot_name(&self, slot: usize) -> &str {
        self.slots
            .get(slot)
            .and_then(Option::as_ref)
            .map_or("", |record| record.name.as_str())
    }
}
