use super::*;

const BACKGROUND_BYTES: usize = 256 * 1024;
const LOCAL_BYTES: usize = 64 * 1024;

impl Editor {
    pub(super) fn refresh_parse(&mut self, cx: &mut Context<Self>) {
        let facts = crate::diagnostics::Facts {
            doc_id: crate::diagnostics::document_id(&self.vault, &self.path),
            version: self.doc.version(),
            bytes: self.doc.len() as u64,
            ..Default::default()
        };
        let edits = self.doc.take_edits();
        self.shift_empty_slots(&edits);
        // Positional patches retain unit identities, indexes and viewport rows.
        // Container ranges follow the same left/right biases as document anchors.
        for edit in &edits {
            for unit in &mut self.units {
                edit.shift_range(&mut unit.range);
                let mut list = unit.list.as_mut();
                while let Some(context) = list {
                    edit.shift_range(&mut context.item_range);
                    list = context.parent.as_deref_mut();
                }
            }
        }
        if crate::diagnostics::measure("parse_local", facts, || self.doc.try_reparse_local(LOCAL_BYTES)) {
            // A successful local parse proves that structure and metadata are unchanged.
            return;
        }
        if self.doc.len() < BACKGROUND_BYTES || self.focused.is_none() {
            crate::diagnostics::measure("parse_sync", facts, || self.doc.reparse());
            crate::diagnostics::measure("parse_apply", facts, || {
                self.update_local_link_names();
                self.refresh_units();
            });
        } else {
            self.schedule_parse(cx);
        }
    }

    fn schedule_parse(&mut self, cx: &mut Context<Self>) {
        if self.parse_task.is_some() || !self.doc.is_parse_stale() {
            return;
        }
        let snapshot = self.doc.snapshot();
        let epoch = self.io.epoch;
        let path = self.path.clone();
        let window = self.parse_window;
        let operation = crate::diagnostics::Operation::new(
            "parse_background",
            crate::diagnostics::Facts {
                doc_id: crate::diagnostics::document_id(&self.vault, &path),
                version: snapshot.version(),
                epoch,
                bytes: self.doc.len() as u64,
                ..Default::default()
            },
        );
        let queued = std::time::Instant::now();
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let worker_operation = operation.clone();
            let (version, parsed) = cx
                .background_spawn(async move {
                    worker_operation.stage(
                        "running",
                        crate::diagnostics::Facts {
                            queue_us: queued.elapsed().as_micros() as u64,
                            ..Default::default()
                        },
                    );
                    let result = worker_operation.scope(|| (snapshot.version(), snapshot.parse()));
                    worker_operation.stage("parsed", Default::default());
                    result
                })
                .await;
            if let Some(window) = window {
                let _ = cx.update_window(window, |_, window, cx| {
                    this.update(cx, |this, cx| {
                        let applied = operation
                            .scope(|| this.finish_parse(version, parsed, epoch, &path, Some(window), cx));
                        operation.finish(if applied {
                            crate::diagnostics::Outcome::Ok
                        } else {
                            crate::diagnostics::Outcome::Stale
                        });
                    })
                });
            } else {
                let _ = this.update(cx, |this, cx| {
                    let applied =
                        operation.scope(|| this.finish_parse(version, parsed, epoch, &path, None, cx));
                    operation.finish(if applied {
                        crate::diagnostics::Outcome::Ok
                    } else {
                        crate::diagnostics::Outcome::Stale
                    });
                });
            }
        }));
    }

    fn finish_parse(
        &mut self,
        version: u64,
        parsed: aero_core::ParseOutput,
        epoch: u64,
        path: &VaultPath,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.parse_task = None;
        let applied = self.io.epoch == epoch && &self.path == path && self.doc.apply_parse(version, parsed);
        if applied {
            tracing::debug!(target: "aero::diagnostics", event = "parse_apply", outcome = "ok", version, epoch);
            self.update_local_link_names();
            self.refresh_units();
            self.clamp_selection();
            if let Some(focused) = &mut self.focused {
                focused.id = unit_at(&self.units, focused.range.start)
                    .filter(|&index| {
                        let unit = &self.units[index];
                        unit.range.start == focused.range.start
                            || (focused.id == Some(unit.id) && unit.range.end >= focused.range.end)
                    })
                    .map(|index| self.units[index].id);
                if let Some(id) = focused.id.and_then(|id| self.unit_indices.get(&id).copied()) {
                    focused.enter_newline = self.units[id].enter_inserts_newline();
                }
            }
            if let Some(window) = window
                && !self.input_composing(window, cx)
            {
                self.promote_typed_prefix(window, cx);
                self.refresh_markup(cx);
            }
            cx.notify();
        } else {
            tracing::info!(target: "aero::diagnostics", event = "parse_apply", outcome = "stale", version, epoch);
        }
        self.schedule_parse(cx);
        applied
    }

    pub(super) fn ensure_current_parse(&mut self) {
        if self.doc.is_parse_stale() {
            crate::diagnostics::measure(
                "parse_ensure",
                crate::diagnostics::Facts {
                    bytes: self.doc.len() as u64,
                    version: self.doc.version(),
                    ..Default::default()
                },
                || self.doc.reparse(),
            );
            self.update_local_link_names();
            self.refresh_units();
        }
    }
}
