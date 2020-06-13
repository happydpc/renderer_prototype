use crate::features::demo::DemoRenderFeature;
use renderer_base::{RenderFeatureIndex, RenderFeature, SubmitNodeId, FeatureCommandWriter};
use crate::DemoCommandWriterContext;

pub struct DemoCommandWriter {}

impl FeatureCommandWriter<DemoCommandWriterContext> for DemoCommandWriter {
    fn apply_setup(
        &self,
        _write_context: &mut DemoCommandWriterContext,
    ) {
        log::debug!("apply_setup {}", self.feature_debug_name());
    }

    fn render_element(
        &self,
        _write_context: &mut DemoCommandWriterContext,
        index: SubmitNodeId,
    ) {
        log::info!("render_element {} id: {}", self.feature_debug_name(), index);
    }

    fn revert_setup(
        &self,
        _write_context: &mut DemoCommandWriterContext,
    ) {
        log::debug!("revert_setup {}", self.feature_debug_name());
    }

    fn feature_debug_name(&self) -> &'static str {
        DemoRenderFeature::feature_debug_name()
    }

    fn feature_index(&self) -> RenderFeatureIndex {
        DemoRenderFeature::feature_index()
    }
}