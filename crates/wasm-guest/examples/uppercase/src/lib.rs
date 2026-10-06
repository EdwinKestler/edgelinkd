//! Example plugin: upper-case `msg.payload`. Build with
//! `cargo build --release --target wasm32-unknown-unknown` (see `scripts/wasm-examples.sh`).

use n2link_wasm_guest::{export_node, manifest, Ctx, EveValue, Fill, Msg, Node, Shape};

manifest!("../plugin.toml");

pub struct Uppercase {
    handled: u64,
}

impl Node for Uppercase {
    fn init(_config: &Msg) -> Result<Self, String> {
        Ok(Self { handled: 0 })
    }

    fn on_input(&mut self, ctx: &mut Ctx, mut msg: Msg) -> Result<(), String> {
        let upper = match msg.payload() {
            Some(EveValue::String(text)) => text.to_uppercase(),
            _ => return Err("payload must be a string".to_string()),
        };
        msg.set_payload(EveValue::String(upper));
        self.handled += 1;
        ctx.status(Fill::Green, Shape::Dot, &format!("{} handled", self.handled));
        ctx.emit(0, &msg)
    }
}

export_node!(Uppercase);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_cases_and_counts() {
        let mut node = Uppercase::init(&Msg::new()).unwrap();
        let mut ctx = Ctx::new();
        let mut msg = Msg::new();
        msg.set_payload(EveValue::String("bolt".into()));
        node.on_input(&mut ctx, msg).unwrap();
        assert_eq!(ctx.record.outputs[0].1.get_str("payload"), Some("BOLT"));
        assert_eq!(ctx.record.status.as_ref().unwrap().2, "1 handled");
        let mut number = Msg::new();
        number.set_payload(EveValue::I64(1));
        assert!(node.on_input(&mut Ctx::new(), number).is_err());
    }
}
