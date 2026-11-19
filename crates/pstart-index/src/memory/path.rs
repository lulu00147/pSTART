//! Slot-based path reconstruction.

use super::*;

impl MemoryIndex {
    pub fn resolve_record_path(&self, frn: Frn) -> Result<String, PathError> {
        let Some(start) = self.by_frn.get(frn.get()) else {
            return Err(PathError::NotFound { frn });
        };
        let slots = match pstart_core::walker::walk_named_flags(
            &self.parent_slots,
            &self.slot_flags,
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

        if let Some(position) = slots
            .iter()
            .position(|slot| !self.slot_live(*slot as usize))
        {
            let missing = slots[position];
            let (child, parent) = if position == 0 {
                (frn, self.slot_frn(missing).unwrap_or(frn))
            } else {
                let child_slot = slots[position - 1] as usize;
                let child_record = self.slots[child_slot];
                (
                    Frn::new(child_record.frn),
                    Frn::new(self.parent_frn_for_slot(child_slot)),
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
            let slot = *slot as usize;
            let name = self.slot_name(slot).ok_or(PathError::MissingParent {
                child: frn,
                parent: self.slot_frn(slot as u32).unwrap_or(frn),
            })?;
            if !crate::path::valid_name(name) {
                return Err(PathError::InvalidName {
                    frn: Frn::new(self.slots[slot].frn),
                });
            }
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path.push_str(name);
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

    fn slot_live(&self, slot: usize) -> bool {
        self.slot_flags.get(slot).copied().unwrap_or(0) & SLOT_PRESENT != 0
    }

    fn slot_frn(&self, slot: u32) -> Option<Frn> {
        if !self.slot_live(slot as usize) {
            return None;
        }
        self.slots
            .get(slot as usize)
            .map(|record| Frn::new(record.frn))
    }

    fn slot_frn_for_error(&self, slot: u32, fallback: Frn) -> Frn {
        if let Some(frn) = self.slot_frn(slot) {
            return frn;
        }
        self.parent_slots
            .iter()
            .enumerate()
            .find_map(|(index, parent)| {
                (*parent == slot).then_some(Frn::new(self.parent_frn_for_slot(index)))
            })
            .unwrap_or(fallback)
    }
}
