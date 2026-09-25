//! 搜索与隐私：所有偏好与下载操作交给 SemanticService，视图只观察状态。

use aero_embedding::{DOWNLOAD_BYTES, MODEL_ID, MODEL_NAME};
use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase, SemanticService};

pub(crate) fn view(service: Entity<SemanticService>, cx: &mut App) -> Entity<SearchSettings> {
    cx.new(|cx| SearchSettings::new(service, cx))
}

/// 独立实体观察服务，下载进度与开关不依赖对话框重新打开。
pub fn open(service: Entity<SemanticService>, window: &mut Window, cx: &mut App) {
    let view = view(service, cx);
    window.open_dialog(cx, move |dialog, window, _| {
        dialog
            .title("搜索与隐私")
            .width(px(440.).min(window.viewport_size().width - px(32.)))
            .child(view.clone())
            .footer(
                Button::new("close-search-settings")
                    .label("完成")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
    });
}

pub(crate) struct SearchSettings {
    service: Entity<SemanticService>,
    _subscription: Subscription,
}

impl SearchSettings {
    fn new(service: Entity<SemanticService>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&service, |_, _, cx| cx.notify());
        Self { service, _subscription: subscription }
    }

    fn change_preferences(&mut self, change: impl FnOnce(&mut SearchPreferences), cx: &mut Context<Self>) {
        self.service.update(cx, |service, cx| {
            let mut prefs = service.preferences().clone();
            change(&mut prefs);
            service.set_preferences(prefs, cx);
        });
    }
}

fn can_download(prefs: &SearchPreferences, model: &ModelStatus) -> bool {
    prefs.allow_model_downloads && matches!(model, ModelStatus::Missing | ModelStatus::Failed(_))
}

fn model_is_local(model: &ModelStatus) -> bool {
    matches!(model, ModelStatus::Ready | ModelStatus::Loading)
}

fn model_progress(model: &ModelStatus) -> String {
    match model {
        ModelStatus::Missing => "尚未下载本地模型".into(),
        ModelStatus::Downloading { downloaded, total } => {
            if *total == 0 {
                format!("下载中：{downloaded} 字节 · 总大小待确认")
            } else {
                let percent = ((*downloaded as f64 / *total as f64) * 100.).min(100.);
                format!("下载中：{downloaded} / {total} 字节（{percent:.0}%）")
            }
        }
        ModelStatus::Loading => "正在本地加载模型，全文搜索不受影响".into(),
        ModelStatus::Ready => "模型已就绪，可完全离线使用".into(),
        ModelStatus::Failed(error) => format!("模型失败：{error}"),
    }
}

fn index_progress(phase: &SemanticPhase) -> String {
    match phase {
        SemanticPhase::Disabled => "语义索引未开启".into(),
        SemanticPhase::WaitingForModel => "等待本地模型；全文搜索仍可用".into(),
        SemanticPhase::Paused => "语义索引已暂停".into(),
        SemanticPhase::WaitingForIdle => "等待输入空闲后继续索引".into(),
        SemanticPhase::Indexing { done, total } => format!("正在索引：{done} / {total} 篇"),
        SemanticPhase::Ready { notes, chunks } => format!("索引完成：{notes} 篇笔记 · {chunks} 个片段"),
        SemanticPhase::Failed(error) => format!("语义索引出错：{error}"),
    }
}

impl Render for SearchSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let service = self.service.read(cx);
        let prefs = service.preferences().clone();
        let status = service.status();
        let muted = cx.theme().muted_foreground;
        let downloading = matches!(status.model, ModelStatus::Downloading { .. });
        let model_local = model_is_local(&status.model);
        let download_enabled = can_download(&prefs, &status.model);
        let description = |text: String| div().w_full().min_w_0().text_xs().text_color(muted).child(text);

        v_flex().id("search-settings-panel").debug_selector(|| "search-settings-panel".into())
            .w_full().min_w_0().max_h((window.viewport_size().height * 0.65).max(px(160.)))
            .overflow_y_scroll().gap_3().text_sm()
            .child(description("全文始终可用；混合搜索先显示全文，再补充本地语义结果。".into()))
            .child(v_flex().w_full().min_w_0().gap_1()
                .child(div().font_weight(FontWeight::MEDIUM).child("本地模型"))
                .child(div().child(MODEL_NAME))
                .child(description(format!("约 {:.0} MB · 384 维 · 仅 CPU", DOWNLOAD_BYTES as f64 / 1_000_000.)))
                .child(description("int8 · 单线程推理 · 每批 4 个片段 · 最长 256 tokens".into()))
                .child(description("运行时需数百 MB 内存；空闲 60 秒后卸载模型。".into())))
            .child(v_flex().w_full().min_w_0().gap_1()
                .child(h_flex().child(div().debug_selector(|| "allow-model-downloads".into()).child(
                    Switch::new("allow-model-downloads").small().label("允许模型下载")
                        .checked(prefs.allow_model_downloads)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.change_preferences(|prefs| prefs.allow_model_downloads = *checked, cx);
                        })),
                )))
                .child(description("默认关闭。仅下载 Hugging Face 上固定模型文件；下载时对方可见 IP 与模型名称。笔记与查询永不上传。关闭下载许可不影响已有本地模型离线使用。".into())))
            .child(v_flex().w_full().min_w_0().gap_1()
                .child(div().debug_selector(|| "model-progress".into()).w_full().min_w_0()
                    .text_xs().text_color(muted).child(model_progress(&status.model)))
                .child(h_flex().gap_2().flex_wrap()
                    .when(!downloading && !model_local, |el| el.child(
                        div().debug_selector(|| "model-download".into()).child(
                            Button::new("model-download").small()
                                .label(if matches!(status.model, ModelStatus::Failed(_)) { "重试下载" } else { "下载模型" })
                                .disabled(!download_enabled)
                                .tooltip(format!("固定模型：{MODEL_ID}"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.service.update(cx, |service, cx| {
                                        if can_download(service.preferences(), &service.status().model) {
                                            service.download_model(cx);
                                        }
                                    });
                                })),
                        ),
                    ))
                    .when(downloading, |el| el.child(
                        div().debug_selector(|| "model-cancel".into()).child(
                            Button::new("model-cancel").small().label("取消下载")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.service.update(cx, |service, cx| service.cancel_download(cx));
                                })),
                        ),
                    )))
                .when(!prefs.allow_model_downloads && !model_local, |el| el.child(
                    description("下载已禁止；请先明确开启上方许可，再点击下载。".into()),
                )))
            .child(v_flex().w_full().min_w_0().gap_1()
                .child(h_flex().child(div().debug_selector(|| "semantic-enabled".into()).child(
                    Switch::new("semantic-enabled").small().label("启用本地语义搜索")
                        .checked(prefs.semantic_enabled)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.change_preferences(|prefs| prefs.semantic_enabled = *checked, cx);
                        })),
                )))
                .child(description("默认关闭。开启后在本机建立语义索引；没有模型时仍只显示全文结果，不会自动授权下载。".into())))
            .child(v_flex().w_full().min_w_0().gap_1()
                .child(div().font_weight(FontWeight::MEDIUM).child("后台语义索引"))
                .child(description(index_progress(&status.phase)))
                .child(h_flex().child(div().debug_selector(|| "semantic-pause".into()).child(
                    Button::new("semantic-pause").small()
                        .label(if prefs.indexing_paused { "恢复索引" } else { "暂停索引" })
                        .disabled(!prefs.semantic_enabled)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.change_preferences(|prefs| prefs.indexing_paused = !prefs.indexing_paused, cx);
                        })),
                )))
                .child(description("等待输入空闲，低占用分批处理；输入时暂停后台索引。超大文件可能跳过。手动暂停不影响全文搜索。".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::{can_download, index_progress, model_progress};
    use crate::semantic::{ModelStatus, SearchPreferences, SemanticPhase};

    #[test]
    fn privacy_defaults_disallow_download_and_semantics() {
        let prefs = SearchPreferences::default();
        assert!(!prefs.allow_model_downloads);
        assert!(!prefs.semantic_enabled);
        assert!(!can_download(&prefs, &ModelStatus::Missing));
        assert!(!can_download(&prefs, &ModelStatus::Failed("offline".into())));
    }

    #[test]
    fn download_requires_consent_and_a_missing_or_failed_model() {
        let prefs = SearchPreferences { allow_model_downloads: true, ..Default::default() };
        assert!(can_download(&prefs, &ModelStatus::Missing));
        assert!(can_download(&prefs, &ModelStatus::Failed("retry".into())));
        for model in
            [ModelStatus::Ready, ModelStatus::Loading, ModelStatus::Downloading { downloaded: 1, total: 2 }]
        {
            assert!(!can_download(&prefs, &model));
        }
    }

    #[test]
    fn loading_model_uses_offline_ui_without_download_controls() {
        // This is the render predicate for both the download button and permission prompt.
        for model in [ModelStatus::Loading, ModelStatus::Ready] {
            assert!(super::model_is_local(&model), "local loading must hide download controls: {model:?}");
            for allow_model_downloads in [false, true] {
                let prefs = SearchPreferences { allow_model_downloads, ..Default::default() };
                assert!(!can_download(&prefs, &model));
            }
        }
        for model in [ModelStatus::Missing, ModelStatus::Failed("retry".into())] {
            assert!(!super::model_is_local(&model), "missing models still need the download UI");
        }
        let loading = model_progress(&ModelStatus::Loading);
        assert!(loading.contains("本地加载"), "{loading}");
        assert!(!loading.contains("下载"), "{loading}");
    }

    #[test]
    fn progress_reports_bytes_and_completed_notes() {
        let progress = model_progress(&ModelStatus::Downloading { downloaded: 123, total: 456 });
        assert!(progress.contains("123 / 456 字节"), "{progress}");
        assert!(model_progress(&ModelStatus::Ready).contains("离线"));
        assert!(model_progress(&ModelStatus::Failed("disk full".into())).contains("disk full"));
        assert!(index_progress(&SemanticPhase::Indexing { done: 4, total: 9 }).contains("4 / 9"));
        let ready = index_progress(&SemanticPhase::Ready { notes: 8, chunks: 17 });
        assert!(ready.contains("8 篇") && ready.contains("17"), "{ready}");
        assert!(index_progress(&SemanticPhase::WaitingForIdle).contains("空闲"));
    }
}
