use std::sync::Arc;

use crate::runtime::flow::Flow;
use crate::runtime::nodes::*;
use n2link_macro::*;

#[flow_node("test-once", red_name = "test-once", module = "n2link_core", inputs = 1, outputs = 0)]
struct TestOnceNode {
    base: BaseFlowNodeState,
}

impl TestOnceNode {
    fn build(
        _flow: &Flow,
        state: BaseFlowNodeState,
        _config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let node = TestOnceNode { base: state };
        Ok(Box::new(node))
    }
}

#[async_trait]
impl FlowNodeBehavior for TestOnceNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }

    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        while !stop_token.is_cancelled() {
            let engine = self.engine().expect("The engine cannot be released");

            match self.recv_msg(stop_token.clone()).await {
                Ok(msg) => engine.recv_final_msg(msg).expect("Shoud send final msg to the engine"),
                Err(e) => {
                    if !e.is_cancelled() {
                        eprintln!("Failed to recv_msg(): {e:?}");
                    }
                    break;
                }
            }
        }
    }
}
