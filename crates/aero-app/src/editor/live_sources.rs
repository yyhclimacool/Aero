use super::*;

#[derive(Default)]
pub(super) struct LiveSources {
    registry: Vec<(String, WeakEntity<Editor>)>,
    editors: std::collections::HashMap<(String, VaultPath), WeakEntity<Editor>>,
    resolvers: std::collections::HashMap<String, aero_core::links::Resolver>,
    generations: std::collections::HashMap<String, u64>,
    status_watchers: std::collections::HashMap<EntityId, (Option<VaultPath>, [Subscription; 2])>,
}
impl Global for LiveSources {}

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
    let watcher = if !cx.global::<LiveSources>().status_watchers.contains_key(&editor.entity_id()) {
        editor.upgrade().map(|entity| {
            let status = cx.observe(&entity, |entity, cx| {
                let source = entity.read(cx);
                let (vault, conflict) = (source.vault.id().to_owned(), source.source_conflict());
                let changed = cx.global::<LiveSources>().status_watchers.get(&entity.entity_id())
                    .is_some_and(|(previous, _)| previous != &conflict);
                if changed {
                    cx.update_global::<LiveSources, _>(|sources, _| {
                        if let Some((previous, _)) = sources.status_watchers.get_mut(&entity.entity_id()) {
                            *previous = conflict;
                        }
                        *sources.generations.entry(vault).or_default() += 1;
                    });
                }
            });
            let id = entity.entity_id();
            let release = cx.observe_release(&entity, move |source, cx| {
                if !cx.has_global::<LiveSources>() { return; }
                let vault = source.vault.id().to_owned();
                super::source_cache::invalidate(&vault, std::slice::from_ref(&source.path));
                cx.update_global::<LiveSources, _>(|sources, _| {
                    sources.registry.retain(|(_, weak)| weak.entity_id() != id);
                    let key = (vault.clone(), source.path.clone());
                    if sources.editors.get(&key).is_some_and(|weak| weak.entity_id() == id) {
                        sources.editors.remove(&key);
                        if let Some(resolver) = sources.resolvers.get_mut(&vault) {
                            resolver.remove(&source.path);
                        }
                    }
                    sources.status_watchers.remove(&id);
                    *sources.generations.entry(vault).or_default() += 1;
                });
            });
            [status, release]
        })
    } else { None };
    cx.update_global::<LiveSources, _>(|sources, _| {
        if let Some(watcher) = watcher {
            sources.status_watchers.insert(editor.entity_id(), (None, watcher));
        }
        *sources.generations.entry(vault.id().into()).or_default() += 1;
        sources
            .registry
            .retain(|(_, weak)| weak.upgrade().is_some() && weak.entity_id() != editor.entity_id());
        sources.registry.push((vault.id().into(), editor.clone()));
        sources.status_watchers.retain(|id, _| sources.registry.iter().any(|(_, weak)| weak.entity_id() == *id));
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
    pub(super) fn source_conflict(&self) -> Option<VaultPath> {
        match &self.banner {
            Some(Banner::Conflict { copy }) => Some(copy.clone()),
            _ => None,
        }
    }

    pub(super) fn local_source_view(&self, fragment: Option<&str>) -> embeds_ui::EmbedView {
        if self.is_deleted() {
            return embeds_ui::EmbedView::Failed("同步来源已删除".into());
        }
        self.cached_source(&self.path, fragment)
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

    pub(super) fn live_embed(
        &self,
        target: &str,
        fragment: Option<&str>,
        origin: Option<&VaultPath>,
        cx: &App,
    ) -> Option<embeds_ui::EmbedView> {
        use aero_core::links::Resolution;
        let sources = cx.try_global::<LiveSources>()?;
        let origin = origin.unwrap_or(&self.path);
        if target.trim().strip_prefix("id:").is_some_and(aero_core::identity::is_document_id)
            && sources.resolvers.get(self.vault.id()).is_some_and(|resolver|
                matches!(resolver.resolve(target, fragment, origin.parent()), Resolution::Ambiguous { .. }))
        {
            return Some(embeds_ui::EmbedView::Failed("同步来源身份有歧义".into()));
        }
        let Some(resolver) = self.embeds.resolver.as_ref() else {
            return Some(embeds_ui::EmbedView::Loading);
        };
        let resolution = if target.is_empty() {
            Resolution::Resolved { path: origin.clone(), fragment: fragment.map(str::to_owned) }
        } else {
            resolver.resolve(target, fragment, origin.parent())
        };
        if let Resolution::Resolved { path, .. } = &resolution {
            let deleted = if path == &self.path {
                self.is_deleted()
            } else {
                sources.editors.get(&(self.vault.id().into(), path.clone()))
                    .and_then(WeakEntity::upgrade).is_some_and(|source| source.read(cx).is_deleted())
            };
            if deleted { return Some(embeds_ui::EmbedView::Failed("同步来源已删除".into())); }
        }
        if !self.embeds_current(cx) {
            return Some(embeds_ui::EmbedView::Loading);
        }
        Some(match resolution {
            Resolution::Resolved { path, fragment } => self.cached_source(&path, fragment.as_deref()),
            Resolution::Ambiguous { .. } => embeds_ui::EmbedView::Failed("同步来源身份有歧义".into()),
            Resolution::Unresolved => embeds_ui::EmbedView::Failed(format!("找不到同步来源：{target}").into()),
        })
    }
}
