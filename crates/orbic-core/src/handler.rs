//! Plain `async fn` handlers whose arguments are [`Extract`]ors.

use std::any::TypeId;
use std::future::Future;

use hyve_api::{BoxFuture, ContextNeed, Wire};
use hyve_core::HyveResult;
use hyve_serde::to_json_data;

use hyve_validate::{Schema, SchemaRegistry};

use crate::extract::{Extract, Parts, RouteShape};
use crate::request::{Payload, Rail};

/// An `async fn(A1, ..., An) -> HyveResult<T>` whose arguments are
/// extractors (up to 8) and whose result is a wire type.
pub trait Handler<Args>: Clone + Send + Sync + 'static {
    /// The response model.
    type Response: Wire;

    /// The injected values the arguments need.
    fn needs() -> Vec<ContextNeed>;

    /// What the handler reads and answers.
    fn describe(registry: &mut SchemaRegistry) -> RouteShape;

    /// Extracts the arguments and runs the function.
    fn call(&self, parts: Parts) -> BoxFuture<'static, HyveResult<Self::Response>>;
}

macro_rules! handler {
    ($($a:ident),*) => {
        impl<F, Fut, R, $($a),*> Handler<($($a,)*)> for F
        where
            F: Fn($($a),*) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = HyveResult<R>> + Send + 'static,
            R: Wire,
            $($a: Extract,)*
        {
            type Response = R;

            fn needs() -> Vec<ContextNeed> {
                let needs: Vec<Option<ContextNeed>> = vec![$($a::need()),*];
                needs.into_iter().flatten().collect()
            }

            #[allow(unused_mut)]
            fn describe(registry: &mut SchemaRegistry) -> RouteShape {
                let mut shape = RouteShape {
                    response: <R as Schema>::schema(registry),
                    ..RouteShape::default()
                };
                $($a::describe(registry, &mut shape);)*
                shape
            }

            #[allow(non_snake_case, unused_mut, unused_variables)]
            fn call(&self, mut parts: Parts) -> BoxFuture<'static, HyveResult<R>> {
                let f = self.clone();
                Box::pin(async move {
                    $(let $a = $a::extract(&mut parts)?;)*
                    f($($a),*).await
                })
            }
        }
    };
}

handler!();
handler!(A1);
handler!(A1, A2);
handler!(A1, A2, A3);
handler!(A1, A2, A3, A4);
handler!(A1, A2, A3, A4, A5);
handler!(A1, A2, A3, A4, A5, A6);
handler!(A1, A2, A3, A4, A5, A6, A7);
handler!(A1, A2, A3, A4, A5, A6, A7, A8);

/// Encodes a response model for `rail`; `()` is [`Payload::Empty`].
pub fn encode<T: Wire>(value: T, rail: Rail) -> HyveResult<Payload> {
    if TypeId::of::<T>() == TypeId::of::<()>() {
        return Ok(Payload::Empty);
    }
    Ok(match rail {
        Rail::Standard => Payload::Json(to_json_data(&value)),
        Rail::Fast => Payload::Binary(value.to_binary()?),
        Rail::InProcess => Payload::Value(Box::new(value)),
    })
}
