use rquickjs::{Ctx, Result, Value, class::Trace};

use crate::runtime::js::util;

#[derive(Clone, Trace, Default)]
#[rquickjs::class(frozen)]
pub(super) struct N2linkClass {}

unsafe impl<'js> rquickjs::JsLifetime<'js> for N2linkClass {
    type Changed<'to> = N2linkClass;
}

#[allow(non_snake_case)]
#[rquickjs::methods]
impl<'js> N2linkClass {
    /// Deep clone a JS object
    #[qjs(rename = "deepClone")]
    fn deep_clone(&self, obj: Value<'js>, ctx: Ctx<'js>) -> Result<Value<'js>> {
        util::deep_clone(ctx, obj)
    }
}
