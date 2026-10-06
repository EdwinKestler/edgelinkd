use std::sync::Arc;

use async_trait::*;
use tokio_util::sync::CancellationToken;

use n2link_core::Result;
use n2link_core::runtime::context::*;
use n2link_core::runtime::flow::*;
use n2link_core::runtime::model::json::*;
use n2link_core::runtime::model::*;
use n2link_core::runtime::nodes::*;
use n2link_macro::*;

#[flow_node("dummy", red_name = "dummy")]
struct DummyNode {
    base: BaseFlowNodeState,
}

impl DummyNode {
    fn build(
        _flow: &Flow,
        state: BaseFlowNodeState,
        _config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> Result<Box<dyn FlowNodeBehavior>> {
        let node = DummyNode { base: state };
        Ok(Box::new(node))
    }
}

#[async_trait]
impl FlowNodeBehavior for DummyNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let cancel = stop_token.child_token();
            with_uow(self.as_ref(), cancel.child_token(), |node: &DummyNode, msg: MsgHandle| async move {
                node.fan_out_one(Envelope { port: 0, msg }, cancel.child_token()).await?;
                Ok(())
            })
            .await;
        }
    }
}

pub fn foo() {}
