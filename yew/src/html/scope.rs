use super::{Callback, Component, NodeRef, Renderable};
use crate::scheduler::{scheduler, ComponentRunnableType, Runnable, Shared};
use crate::virtual_dom::{VDiff, VNode};
use cfg_if::cfg_if;
use std::any::{Any, TypeId};
use std::cell::{Ref, RefCell};
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;
cfg_if! {
    if #[cfg(feature = "std_web")] {
        use stdweb::web::Element;
    } else if #[cfg(feature = "web_sys")] {
        use web_sys::Element;
    }
}

/// Updates for a `Component` instance. Used by scope sender.
pub(crate) enum ComponentUpdate<COMP: Component> {
    /// Force update
    Force,
    /// Wraps messages for a component.
    Message(COMP::Message),
    /// Wraps batch of messages for a component.
    MessageBatch(Vec<COMP::Message>),
    /// Wraps properties and new node ref for a component.
    Properties(COMP::Properties, NodeRef),
}

/// Untyped scope used for accessing parent scope
#[derive(Debug, Clone)]
pub struct AnyScope {
    pub(crate) type_id: TypeId,
    pub(crate) parent: Option<Rc<AnyScope>>,
    pub(crate) state: Rc<dyn Any>,
}

impl<COMP: Component> From<Scope<COMP>> for AnyScope {
    fn from(scope: Scope<COMP>) -> Self {
        AnyScope {
            type_id: TypeId::of::<COMP>(),
            parent: scope.parent,
            state: Rc::new(scope.state),
        }
    }
}

impl AnyScope {
    /// Returns the parent scope
    pub fn get_parent(&self) -> Option<&AnyScope> {
        self.parent.as_deref()
    }

    /// Returns the type of the linked component
    pub fn get_type_id(&self) -> &TypeId {
        &self.type_id
    }

    /// Attempts to downcast into a typed scope
    pub fn downcast<COMP: Component>(self) -> Scope<COMP> {
        Scope {
            parent: self.parent,
            state: self
                .state
                .downcast_ref::<Shared<Option<ComponentState<COMP>>>>()
                .expect("unexpected component type")
                .clone(),
        }
    }
}

/// A context which allows sending messages to a component.
pub struct Scope<COMP: Component> {
    parent: Option<Rc<AnyScope>>,
    state: Shared<Option<ComponentState<COMP>>>,
}

impl<COMP: Component> fmt::Debug for Scope<COMP> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Scope<_>")
    }
}

impl<COMP: Component> Clone for Scope<COMP> {
    fn clone(&self) -> Self {
        Scope {
            parent: self.parent.clone(),
            state: self.state.clone(),
        }
    }
}

impl<COMP: Component> Scope<COMP> {
    /// Returns the parent scope
    pub fn get_parent(&self) -> Option<&AnyScope> {
        self.parent.as_deref()
    }

    /// Returns the linked component if available
    pub fn get_component(&self) -> Option<impl Deref<Target = COMP> + '_> {
        self.state.try_borrow().ok().and_then(|state_ref| {
            state_ref.as_ref()?;
            Some(Ref::map(state_ref, |state| {
                state.as_ref().unwrap().component.as_ref()
            }))
        })
    }

    pub(crate) fn new(parent: Option<AnyScope>) -> Self {
        let parent = parent.map(Rc::new);
        let state = Rc::new(RefCell::new(None));
        Scope { parent, state }
    }

    /// Mounts a component with `props` to the specified `element` in the DOM.
    pub(crate) fn mount_in_place(
        self,
        element: Element,
        ancestor: Option<VNode>,
        node_ref: NodeRef,
        props: COMP::Properties,
    ) -> Scope<COMP> {
        *self.state.borrow_mut() = Some(ComponentState::new(
            element,
            ancestor,
            node_ref,
            self.clone(),
            props,
        ));
        self.update(ComponentUpdate::Force, true);
        self
    }

    /// Schedules a task to send an update to a component
    pub(crate) fn update(&self, update: ComponentUpdate<COMP>, first_update: bool) {
        let update = UpdateComponent {
            state: self.state.clone(),
            update,
        };
        scheduler().push_comp(ComponentRunnableType::Update, Box::new(update));
        self.rendered(first_update);
    }

    /// Schedules a task to call the rendered method on a component
    pub(crate) fn rendered(&self, first_render: bool) {
        let state = self.state.clone();
        let rendered = RenderedComponent {
            state,
            first_render,
        };
        scheduler().push_comp(ComponentRunnableType::Rendered, Box::new(rendered));
    }

    /// Schedules a task to destroy a component
    pub(crate) fn destroy(&mut self) {
        let state = self.state.clone();
        let destroy = DestroyComponent { state };
        scheduler().push_comp(ComponentRunnableType::Destroy, Box::new(destroy));
    }

    /// Send a message to the component
    pub fn send_message<T>(&self, msg: T)
    where
        T: Into<COMP::Message>,
    {
        self.update(ComponentUpdate::Message(msg.into()), false);
    }

    /// Send a batch of messages to the component
    pub fn send_message_batch(&self, messages: Vec<COMP::Message>) {
        self.update(ComponentUpdate::MessageBatch(messages), false);
    }

    /// Creates a `Callback` which will send a message to the linked component's
    /// update method when invoked.
    pub fn callback<F, IN, M>(&self, function: F) -> Callback<IN>
    where
        M: Into<COMP::Message>,
        F: Fn(IN) -> M + 'static,
    {
        let scope = self.clone();
        let closure = move |input| {
            let output = function(input);
            scope.send_message(output);
        };
        closure.into()
    }

    /// Creates a `Callback` from a FnOnce which will send a message to the linked
    /// component's update method when invoked.
    pub fn callback_once<F, IN, M>(&self, function: F) -> Callback<IN>
    where
        M: Into<COMP::Message>,
        F: FnOnce(IN) -> M + 'static,
    {
        let scope = self.clone();
        let closure = move |input| {
            let output = function(input);
            scope.send_message(output);
        };
        Callback::once(closure)
    }

    /// Creates a `Callback` which will send a batch of messages back to the linked
    /// component's update method when invoked.
    pub fn batch_callback<F, IN>(&self, function: F) -> Callback<IN>
    where
        F: Fn(IN) -> Vec<COMP::Message> + 'static,
    {
        let scope = self.clone();
        let closure = move |input| {
            let messages = function(input);
            scope.send_message_batch(messages);
        };
        closure.into()
    }
}

struct ComponentState<COMP: Component> {
    element: Element,
    node_ref: NodeRef,
    scope: Scope<COMP>,
    component: Box<COMP>,
    last_root: Option<VNode>,
    rendered: bool,
}

impl<COMP: Component> ComponentState<COMP> {
    fn new(
        element: Element,
        ancestor: Option<VNode>,
        node_ref: NodeRef,
        scope: Scope<COMP>,
        props: COMP::Properties,
    ) -> Self {
        let component = Box::new(COMP::create(props, scope.clone()));
        Self {
            element,
            node_ref,
            scope,
            component,
            last_root: ancestor,
            rendered: false,
        }
    }
}

struct UpdateComponent<COMP>
where
    COMP: Component,
{
    state: Shared<Option<ComponentState<COMP>>>,
    update: ComponentUpdate<COMP>,
}

impl<COMP> Runnable for UpdateComponent<COMP>
where
    COMP: Component,
{
    fn run(self: Box<Self>) {
        if let Some(mut state) = self.state.borrow_mut().as_mut() {
            let should_update = match self.update {
                ComponentUpdate::Force => true,
                ComponentUpdate::Message(message) => state.component.update(message),
                ComponentUpdate::MessageBatch(messages) => messages
                    .into_iter()
                    .fold(false, |acc, msg| state.component.update(msg) || acc),
                ComponentUpdate::Properties(props, node_ref) => {
                    // When components are updated, they receive a new node ref that
                    // must be linked to previous one.
                    node_ref.link(state.node_ref.clone());
                    state.component.change(props)
                }
            };

            if should_update {
                state.rendered = false;
                let mut root = state.component.render();
                let last_root = state.last_root.take();
                if let Some(node) =
                    root.apply(&state.scope.clone().into(), &state.element, None, last_root)
                {
                    state.node_ref.set(Some(node));
                } else if let VNode::VComp(child) = &root {
                    // If the root VNode is a VComp, we won't have access to the rendered DOM node
                    // because components render asynchronously. In order to bubble up the DOM node
                    // from the VComp, we need to link the currently rendering component with its
                    // root child component.
                    state.node_ref.link(child.node_ref.clone());
                }
                state.last_root = Some(root);
            };
        }
    }
}

struct RenderedComponent<COMP>
where
    COMP: Component,
{
    state: Shared<Option<ComponentState<COMP>>>,
    first_render: bool,
}

impl<COMP> Runnable for RenderedComponent<COMP>
where
    COMP: Component,
{
    fn run(self: Box<Self>) {
        if let Some(mut state) = self.state.borrow_mut().as_mut() {
            if !state.rendered {
                state.rendered = true;
                state.component.rendered(self.first_render);
            }
        }
    }
}

struct DestroyComponent<COMP>
where
    COMP: Component,
{
    state: Shared<Option<ComponentState<COMP>>>,
}

impl<COMP> Runnable for DestroyComponent<COMP>
where
    COMP: Component,
{
    fn run(self: Box<Self>) {
        if let Some(mut state) = self.state.borrow_mut().take() {
            drop(state.component);
            if let Some(last_frame) = &mut state.last_root {
                last_frame.detach(&state.element);
            }
        }
    }
}
