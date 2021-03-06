use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::{FutureExt, LocalBoxFuture};
use pin_project::pin_project;

use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::httprequest::HttpRequest;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;

/// Async fn handler
pub trait Handler<T, Err>: Clone + 'static
where
    Err: ErrorRenderer,
{
    type Output: Responder<Err>;
    type Future: Future<Output = Self::Output> + 'static;

    fn call(&self, param: T) -> Self::Future;
}

impl<F, R, Err> Handler<(), Err> for F
where
    F: Fn() -> R + Clone + 'static,
    R: Future + 'static,
    R::Output: Responder<Err>,
    Err: ErrorRenderer,
{
    type Future = R;
    type Output = R::Output;

    fn call(&self, _: ()) -> R {
        (self)()
    }
}

pub(super) trait HandlerFn<Err: ErrorRenderer> {
    fn call(
        &self,
        _: WebRequest<Err>,
    ) -> LocalBoxFuture<'static, Result<WebResponse, Err::Container>>;

    fn clone_handler(&self) -> Box<dyn HandlerFn<Err>>;
}

pub(super) struct HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err>,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    hnd: F,
    _t: PhantomData<(T, Err)>,
}

impl<F, T, Err> HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err>,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    pub(super) fn new(hnd: F) -> Self {
        HandlerWrapper {
            hnd,
            _t: PhantomData,
        }
    }
}

impl<F, T, Err> HandlerFn<Err> for HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err> + 'static,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    fn call(
        &self,
        req: WebRequest<Err>,
    ) -> LocalBoxFuture<'static, Result<WebResponse, Err::Container>> {
        let (req, mut payload) = req.into_parts();

        HandlerWrapperResponse {
            hnd: self.hnd.clone(),
            fut1: Some(T::from_request(&req, &mut payload)),
            fut2: None,
            fut3: None,
            req: Some(req),
        }
        .boxed_local()
    }

    fn clone_handler(&self) -> Box<dyn HandlerFn<Err>> {
        Box::new(HandlerWrapper {
            hnd: self.hnd.clone(),
            _t: PhantomData,
        })
    }
}

impl<F, T, Err> Clone for HandlerWrapper<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err>,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    fn clone(&self) -> Self {
        Self {
            hnd: self.hnd.clone(),
            _t: PhantomData,
        }
    }
}

#[pin_project]
pub(super) struct HandlerWrapperResponse<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err>,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    hnd: F,
    #[pin]
    fut1: Option<T::Future>,
    #[pin]
    fut2: Option<F::Future>,
    #[pin]
    fut3: Option<<F::Output as Responder<Err>>::Future>,
    req: Option<HttpRequest>,
}

impl<F, T, Err> Future for HandlerWrapperResponse<F, T, Err>
where
    F: Handler<T, Err>,
    T: FromRequest<Err>,
    T::Error: Into<Err::Container>,
    <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Some(fut) = this.fut1.as_pin_mut() {
            return match fut.poll(cx) {
                Poll::Ready(Ok(param)) => {
                    let fut = this.hnd.call(param);
                    this = self.as_mut().project();
                    this.fut1.set(None);
                    this.fut2.set(Some(fut));
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Ok(WebResponse::from_err::<Err, _>(
                    e,
                    this.req.take().unwrap(),
                ))),
            };
        }

        if let Some(fut) = this.fut2.as_pin_mut() {
            return match fut.poll(cx) {
                Poll::Ready(res) => {
                    let fut = res.respond_to(this.req.as_ref().unwrap());
                    this = self.as_mut().project();
                    this.fut2.set(None);
                    this.fut3.set(Some(fut));
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            };
        }

        if let Some(fut) = this.fut3.as_pin_mut() {
            return match fut.poll(cx) {
                Poll::Ready(Ok(res)) => {
                    Poll::Ready(Ok(WebResponse::new(this.req.take().unwrap(), res)))
                }
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Ok(WebResponse::from_err::<Err, _>(
                    e,
                    this.req.take().unwrap(),
                ))),
            };
        }

        unreachable!();
    }
}

/// FromRequest trait impl for tuples
macro_rules! factory_tuple ({ $(($n:tt, $T:ident)),+} => {
    impl<Func, $($T,)+ Res, Err> Handler<($($T,)+), Err> for Func
    where Func: Fn($($T,)+) -> Res + Clone + 'static,
          Res: Future + 'static,
          Res::Output: Responder<Err>,
          Err: ErrorRenderer,
    {
        type Future = Res;
        type Output = Res::Output;

        fn call(&self, param: ($($T,)+)) -> Res {
            (self)($(param.$n,)+)
        }
    }
});

#[rustfmt::skip]
mod m {
    use super::*;

factory_tuple!((0, A));
factory_tuple!((0, A), (1, B));
factory_tuple!((0, A), (1, B), (2, C));
factory_tuple!((0, A), (1, B), (2, C), (3, D));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I));
factory_tuple!((0, A), (1, B), (2, C), (3, D), (4, E), (5, F), (6, G), (7, H), (8, I), (9, J));
}
