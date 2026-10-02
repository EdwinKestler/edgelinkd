//! Status lamp for the process scan task.
//!
//! The period lives in `edgelinkd.toml` (`runtime.scan.period_ms`), not on this node.
//! The node has no message of its own. The scan task turns it green (`ok`) or red (`overrun`).

use std::sync::Arc;

use crate::runtime::flow::Flow;
use crate::runtime::model::json::RedFlowNodeConfig;
use crate::runtime::nodes::*;
use edgelink_macro::*;

#[derive(Debug)]
#[flow_node("scan", red_name = "scan")]
struct ScanNode {
    base: BaseFlowNodeState,
}

impl ScanNode {
    fn build(
        _flow: &Flow,
        base: BaseFlowNodeState,
        _config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        Ok(Box::new(ScanNode { base }))
    }
}

#[async_trait::async_trait]
impl FlowNodeBehavior for ScanNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        stop_token.cancelled().await;
    }
}
