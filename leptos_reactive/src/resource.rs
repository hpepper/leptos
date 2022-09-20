use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::HashSet,
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    create_isomorphic_effect, create_memo, create_signal, queue_microtask, runtime::Runtime,
    spawn::spawn_local, use_context, Memo, ReadSignal, Scope, ScopeId, SuspenseContext,
    WriteSignal,
};

pub fn create_resource<S, T, Fu>(
    cx: Scope,
    source: impl Fn() -> S + 'static,
    fetcher: impl Fn(S) -> Fu + 'static,
) -> Resource<S, T>
where
    S: PartialEq + Debug + Clone + 'static,
    T: Debug + Clone + Serialize + DeserializeOwned + 'static,
    Fu: Future<Output = T> + 'static,
{
    create_resource_with_initial_value(cx, source, fetcher, None)
}

pub fn create_resource_with_initial_value<S, T, Fu>(
    cx: Scope,
    source: impl Fn() -> S + 'static,
    fetcher: impl Fn(S) -> Fu + 'static,
    initial_value: Option<T>,
) -> Resource<S, T>
where
    S: PartialEq + Debug + Clone + 'static,
    T: Debug + Clone + Serialize + DeserializeOwned + 'static,
    Fu: Future<Output = T> + 'static,
{
    let resolved = initial_value.is_some();
    let (value, set_value) = create_signal(cx, initial_value);
    let (loading, set_loading) = create_signal(cx, false);
    let (track, trigger) = create_signal(cx, 0);
    let fetcher = Rc::new(move |s| Box::pin(fetcher(s)) as Pin<Box<dyn Future<Output = T>>>);
    let source = create_memo(cx, move |_| source());

    // TODO hydration/streaming logic

    let r = Rc::new(ResourceState {
        scope: cx,
        value,
        set_value,
        loading,
        set_loading,
        track,
        trigger,
        source,
        fetcher,
        resolved: Rc::new(Cell::new(resolved)),
        scheduled: Rc::new(Cell::new(false)),
        suspense_contexts: Default::default(),
    });

    let id = cx.push_resource(Rc::clone(&r));

    #[cfg(any(feature = "csr", feature = "ssr", feature = "hydrate"))]
    create_isomorphic_effect(cx, {
        let r = Rc::clone(&r);
        move |_| {
            load_resource(cx, id, r.clone());
        }
    });

    Resource {
        runtime: cx.runtime,
        scope: cx.id,
        id,
        source_ty: PhantomData,
        out_ty: PhantomData,
    }
}

#[cfg(any(feature = "csr", feature = "ssr"))]
fn load_resource<S, T>(cx: Scope, _id: ResourceId, r: Rc<ResourceState<S, T>>)
where
    S: PartialEq + Debug + Clone + 'static,
    T: Debug + Clone + Serialize + DeserializeOwned + 'static,
{
    r.load(false)
}

#[cfg(feature = "hydrate")]
fn load_resource<S, T>(cx: Scope, id: ResourceId, r: Rc<ResourceState<S, T>>)
where
    S: PartialEq + Debug + Clone + 'static,
    T: Debug + Clone + Serialize + DeserializeOwned + 'static,
{
    use wasm_bindgen::{JsCast, UnwrapThrowExt};

    if let Some(ref mut context) = *cx.runtime.shared_context.borrow_mut() {
        let resource_id = StreamingResourceId(cx.id, id);
        log::debug!(
            "(create_resource) resolved resources = {:#?}",
            context.resolved_resources
        );

        if let Some(json) = context.resolved_resources.remove(&resource_id) {
            log::debug!("(create_resource) resource already resolved from server");
            r.resolved.set(true);
            let res = serde_json::from_str(&json).unwrap_throw();
            r.set_value.update(|n| *n = Some(res));
            r.set_loading.update(|n| *n = false);
        } else if context.pending_resources.remove(&resource_id) {
            log::debug!("(create_resource) resource pending from server");
            r.set_loading.update(|n| *n = true);
            r.trigger.update(|n| *n += 1);

            let resolve = {
                let resolved = r.resolved.clone();
                let set_value = r.set_value;
                let set_loading = r.set_loading;
                move |res: String| {
                    let res = serde_json::from_str(&res).ok();
                    resolved.set(true);
                    set_value.update(|n| *n = res);
                    set_loading.update(|n| *n = false);
                }
            };
            let resolve =
                wasm_bindgen::closure::Closure::wrap(Box::new(resolve) as Box<dyn Fn(String)>);
            let resource_resolvers = js_sys::Reflect::get(
                &web_sys::window().unwrap(),
                &wasm_bindgen::JsValue::from_str("__LEPTOS_RESOURCE_RESOLVERS"),
            )
            .unwrap();
            let id = serde_json::to_string(&id).unwrap();
            js_sys::Reflect::set(
                &resource_resolvers,
                &wasm_bindgen::JsValue::from_str(&id),
                resolve.as_ref().unchecked_ref(),
            );
        } else {
            log::debug!(
                "(create_resource) resource not found in hydration context, loading\n\n{:#?}",
                context.pending_resources
            );
            r.load(false);
        }
    } else {
        log::debug!("(create_resource) no hydration context, loading resource");
        r.load(false)
    }
}

impl<S, T> Resource<S, T>
where
    S: Debug + Clone + 'static,
    T: Debug + Clone + 'static,
{
    pub fn read(&self) -> Option<T> {
        self.runtime
            .resource((self.scope, self.id), |resource: &ResourceState<S, T>| {
                resource.read()
            })
    }

    pub fn loading(&self) -> bool {
        self.runtime
            .resource((self.scope, self.id), |resource: &ResourceState<S, T>| {
                resource.loading.get()
            })
    }

    pub fn refetch(&self) {
        self.runtime
            .resource((self.scope, self.id), |resource: &ResourceState<S, T>| {
                resource.refetch()
            })
    }

    #[cfg(feature = "ssr")]
    pub async fn to_serialization_resolver(&self) -> (StreamingResourceId, String)
    where
        T: Serialize + DeserializeOwned,
    {
        self.runtime
            .resource((self.scope, self.id), |resource: &ResourceState<S, T>| {
                resource.to_serialization_resolver(StreamingResourceId(self.scope, self.id))
            })
            .await
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Resource<S, T>
where
    S: Debug + Clone + 'static,
    T: Debug + Clone + 'static,
{
    runtime: &'static Runtime,
    pub(crate) scope: ScopeId,
    pub(crate) id: ResourceId,
    pub(crate) source_ty: PhantomData<S>,
    pub(crate) out_ty: PhantomData<T>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamingResourceId(pub(crate) ScopeId, pub(crate) ResourceId);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ResourceId(pub(crate) usize);

impl<S, T> Clone for Resource<S, T>
where
    S: Debug + Clone + 'static,
    T: Debug + Clone + 'static,
{
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime,
            scope: self.scope,
            id: self.id,
            source_ty: PhantomData,
            out_ty: PhantomData,
        }
    }
}

impl<S, T> Copy for Resource<S, T>
where
    S: Debug + Clone + 'static,
    T: Debug + Clone + 'static,
{
}

#[derive(Clone)]
pub struct ResourceState<S, T>
where
    S: 'static,
    T: Clone + Debug + 'static,
{
    scope: Scope,
    value: ReadSignal<Option<T>>,
    set_value: WriteSignal<Option<T>>,
    pub loading: ReadSignal<bool>,
    set_loading: WriteSignal<bool>,
    track: ReadSignal<usize>,
    trigger: WriteSignal<usize>,
    source: Memo<S>,
    fetcher: Rc<dyn Fn(S) -> Pin<Box<dyn Future<Output = T>>>>,
    resolved: Rc<Cell<bool>>,
    scheduled: Rc<Cell<bool>>,
    suspense_contexts: Rc<RefCell<HashSet<SuspenseContext>>>,
}

impl<S, T> ResourceState<S, T>
where
    S: Debug + Clone + 'static,
    T: Debug + Clone + 'static,
{
    pub fn read(&self) -> Option<T> {
        let suspense_cx = use_context::<SuspenseContext>(self.scope);

        let v = self.value.get();

        let suspense_contexts = self.suspense_contexts.clone();
        let has_value = v.is_some();

        let increment = move |_| {
            if let Some(s) = &suspense_cx {
                let mut contexts = suspense_contexts.borrow_mut();
                if !contexts.contains(s) {
                    contexts.insert(s.clone());

                    // on subsequent reads, increment will be triggered in load()
                    // because the context has been tracked here
                    // on the first read, resource is already loading without having incremented
                    if !has_value {
                        s.increment();
                    }
                }
            }
        };

        create_isomorphic_effect(self.scope, increment);

        v
    }

    pub fn refetch(&self) {
        self.load(true);
    }

    fn load(&self, refetching: bool) {
        // doesn't refetch if already refetching
        if refetching && self.scheduled.get() {
            return;
        }

        self.scheduled.set(false);

        let loaded_under_transition = self.scope.runtime.running_transition().is_some();

        let fut = (self.fetcher)(self.source.get());

        // `scheduled` is true for the rest of this code only
        self.scheduled.set(true);
        queue_microtask({
            let scheduled = Rc::clone(&self.scheduled);
            move || {
                scheduled.set(false);
            }
        });

        self.set_loading.update(|n| *n = true);
        self.trigger.update(|n| *n += 1);

        // increment counter everywhere it's read
        let suspense_contexts = self.suspense_contexts.clone();
        let running_transition = self.scope.runtime.running_transition();

        for suspense_context in suspense_contexts.borrow().iter() {
            suspense_context.increment();
            log::debug!(
                "[Transition] resource: running transition? {}",
                running_transition.is_some()
            );

            if let Some(transition) = &running_transition {
                log::debug!("[Transition] adding resource");
                transition
                    .resources
                    .borrow_mut()
                    .insert(suspense_context.pending_resources);
            }
        }

        // run the Future
        spawn_local({
            let resolved = self.resolved.clone();
            let scope = self.scope;
            let set_value = self.set_value;
            let set_loading = self.set_loading;
            async move {
                let res = fut.await;

                resolved.set(true);

                // TODO hydration

                if let Some(transition) = scope.runtime.transition() {
                    // TODO transition
                }

                set_value.update(|n| *n = Some(res));
                set_loading.update(|n| *n = false);

                for suspense_context in suspense_contexts.borrow().iter() {
                    suspense_context.decrement();
                }
            }
        })
    }

    #[cfg(feature = "ssr")]
    pub fn resource_to_serialization_resolver(
        &self,
        id: StreamingResourceId,
    ) -> std::pin::Pin<Box<dyn futures::Future<Output = (StreamingResourceId, String)>>>
    where
        T: Serialize,
    {
        let fut = (self.fetcher)(self.source.get());
        Box::pin(async move {
            let res = fut.await;
            (id, serde_json::to_string(&res).unwrap())
        })
    }
}

pub(crate) trait AnyResource {
    fn as_any(&self) -> &dyn Any;

    #[cfg(feature = "ssr")]
    fn to_serialization_resolver(
        &self,
        id: StreamingResourceId,
    ) -> Pin<Box<dyn Future<Output = (StreamingResourceId, String)>>>;
}

impl<S, T> AnyResource for ResourceState<S, T>
where
    S: Debug + Clone,
    T: Clone + Debug + Serialize + DeserializeOwned,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    #[cfg(feature = "ssr")]
    fn to_serialization_resolver(
        &self,
        id: StreamingResourceId,
    ) -> Pin<Box<dyn Future<Output = (StreamingResourceId, String)>>> {
        let fut = self.resource_to_serialization_resolver(id);
        Box::pin(fut)
    }
}