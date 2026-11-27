use super::*;

#[derive(Default)]
pub(super) struct LiveSources {
    registry: Vec<(String, WeakEntity<Editor>)>,
    editors: std::collections::HashMap<(String, VaultPath), WeakEntity<Editor>>,
    resolvers: std::collections::HashMap<String, aero_core::links::Resolver>,
    generations: std::collections::HashMap<String, u64>,
}
impl Global for LiveSources {}

pub(super) struct IdentityOverlay {
    path: VaultPath,
    document: Document,
    stamp: FileStamp,
}

pub(super) fn identity_source(
    vault: &Vault,
    path: &VaultPath,
    overlays: Vec<IdentityOverlay>,
) -> Result<(aero_core::links::Resolver, Document), String> {
    let (mut resolver, notes) = aero_core::block_refs::vault_snapshot(vault)?;
    let disk_bytes = |path: &VaultPath| {
        let index = notes.binary_search_by(|note| note.path.cmp(path)).ok()?;
        notes[index].before.as_deref()
    };
    let disk = disk_bytes(path).ok_or("来源文档已移除或不在笔记库中")?;
    let mut source = None;
    for IdentityOverlay { path: overlay, mut document, stamp } in overlays {
        let Some(bytes) = disk_bytes(&overlay) else { continue };
        if aero_core::hash_bytes(bytes) != stamp.hash {
            if &overlay == path {
                return Err("来源文档已在磁盘上变化，请重新选择".into());
            }
            continue;
        }
        if document.is_parse_stale() {
            document.reparse();
        }
        resolver.register_identity(&overlay, &document);
        if &overlay == path {
            source = Some(document);
        }
    }
    let source = match source {
        Some(document) => document,
        None => Document::from_bytes(disk).map_err(|e| e.to_string())?,
    };
    Ok((resolver, source))
}

pub(super) fn publish(
    vault: &aero_core::Vault,
    path: &VaultPath,
    doc: &Document,
    editor: WeakEntity<Editor>,
    cx: &mut App,
) {
    if !cx.has_global::<LiveSources>() {
        cx.set_global(LiveSources::default());
    }
    cx.update_global::<LiveSources, _>(|sources, _| {
        *sources.generations.entry(vault.id().into()).or_default() += 1;
        sources
            .registry
            .retain(|(_, weak)| weak.upgrade().is_some() && weak.entity_id() != editor.entity_id());
        sources.registry.push((vault.id().into(), editor.clone()));
        let gone: Vec<_> = sources
            .editors
            .iter()
            .filter(|(key, weak)| {
                weak.upgrade().is_none() || (weak.entity_id() == editor.entity_id() && key.1 != *path)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for (vault, path) in gone {
            sources.editors.remove(&(vault.clone(), path.clone()));
            if let Some(resolver) = sources.resolvers.get_mut(&vault) {
                resolver.remove(&path);
            }
        }
        sources.editors.insert((vault.id().into(), path.clone()), editor);
        let resolver = sources.resolvers.entry(vault.id().into()).or_default();
        resolver.insert(path.clone(), Some(&doc.title_or(path.stem())), &doc.front_matter_aliases());
        resolver.register_identity(path, doc);
    });
}

pub(super) fn generation(vault: &str, cx: &App) -> u64 {
    cx.try_global::<LiveSources>().and_then(|s| s.generations.get(vault)).copied().unwrap_or(0)
}

impl Editor {
    pub(super) fn source_conflict(&self, path: &VaultPath, cx: &App) -> bool {
        if path == &self.path {
            return self.has_source_conflict();
        }
        cx.try_global::<LiveSources>()
            .and_then(|s| s.editors.get(&(self.vault.id().into(), path.clone())))
            .and_then(|editor| editor.upgrade())
            .is_some_and(|editor| editor.read(cx).has_source_conflict())
    }
    /// A committed deletion retires this participant before deferred tab-close
    /// events run. Transient owners must not block the next queued operation.
    pub(crate) fn retire_transaction_source(&self, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        cx.update_global::<LiveSources, _>(|sources, _| {
            sources.registry.retain(|(_, weak)| weak.entity_id() != id);
            let key = (self.vault.id().to_owned(), self.path.clone());
            if sources.editors.get(&key).is_some_and(|weak| weak.entity_id() == id) {
                sources.editors.remove(&key);
                if let Some(resolver) = sources.resolvers.get_mut(self.vault.id()) {
                    resolver.remove(&self.path);
                }
            }
            *sources.generations.entry(self.vault.id().to_owned()).or_default() += 1;
        });
    }

    pub(crate) fn live_editors(vault: &str, cx: &App) -> Vec<Entity<Editor>> {
        cx.try_global::<LiveSources>()
            .into_iter()
            .flat_map(|s| &s.registry)
            .filter(|(id, _)| id == vault)
            .filter_map(|(_, weak)| weak.upgrade())
            .collect()
    }
    pub(super) fn publish_live(&self, cx: &mut Context<Self>) {
        publish(&self.vault, &self.path, &self.doc, cx.weak_entity(), cx);
    }

    pub(super) fn identity_overlays(&self, cx: &Context<Self>) -> Vec<IdentityOverlay> {
        Self::live_editors(self.vault.id(), cx)
            .into_iter()
            .filter(|editor| editor.entity_id() != cx.entity_id())
            .filter_map(|editor| {
                let editor = editor.read(cx);
                (!editor.is_deleted() && editor.path != self.path).then(|| IdentityOverlay {
                    path: editor.path.clone(),
                    document: editor.doc.clone(),
                    stamp: editor.stamp,
                })
            })
            .chain((!self.is_deleted()).then(|| IdentityOverlay {
                path: self.path.clone(),
                document: self.doc.clone(),
                stamp: self.stamp,
            }))
            .collect()
    }

    pub(super) fn live_embed(
        &self,
        target: &str,
        fragment: Option<&str>,
        origin: Option<&VaultPath>,
        cx: &App,
    ) -> Option<embeds_ui::EmbedView> {
        let sources = cx.try_global::<LiveSources>()?;
        let resolver = self.embeds.resolver.as_ref().or_else(|| sources.resolvers.get(self.vault.id()))?;
        let origin = origin.unwrap_or(&self.path);
        let resolution = if target.is_empty() {
            aero_core::links::Resolution::Resolved {
                path: origin.clone(),
                fragment: fragment.map(str::to_owned),
            }
        } else {
            resolver.resolve(target, fragment, origin.parent())
        };
        let (path, fragment) = match resolution {
            aero_core::links::Resolution::Resolved { path, fragment } => (path, fragment),
            aero_core::links::Resolution::Ambiguous { .. } => {
                return Some(embeds_ui::EmbedView::Failed("同步来源身份有歧义".into()));
            }
            aero_core::links::Resolution::Unresolved => return None,
        };
        if path == self.path {
            if self.is_deleted() {
                return Some(embeds_ui::EmbedView::Failed("同步来源已删除".into()));
            }
            return Some(match embeds_ui::slice(&self.doc, fragment.as_deref()) {
                Ok(source) => embeds_ui::EmbedView::Ready { path, fragment, title: self.title(), source },
                Err(error) => embeds_ui::EmbedView::Failed(error.into()),
            });
        }
        let weak = sources.editors.get(&(self.vault.id().into(), path.clone()))?;
        let source = weak.upgrade()?;
        let editor = source.read(cx);
        if editor.path != path || editor.vault.id() != self.vault.id() {
            return None;
        }
        if editor.is_deleted() {
            return Some(embeds_ui::EmbedView::Failed("同步来源已删除".into()));
        }
        Some(match embeds_ui::slice(&editor.doc, fragment.as_deref()) {
            Ok(source) => embeds_ui::EmbedView::Ready { path, fragment, title: editor.title(), source },
            Err(error) => embeds_ui::EmbedView::Failed(error.into()),
        })
    }
}
