use crate::dom::{Node, Window};
use ::moxie::embed::Runtime as MoxieRuntime;
use std::collections::HashMap;
use std::iter;
use winit::{
    event::Event,
    event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget},
    window::WindowId,
};

mod window;

pub struct Runtime {
    moxie_runtime: MoxieRuntime<Box<dyn FnMut() -> Vec<Node<Window>> + 'static>, Vec<Node<Window>>>,
    windows: HashMap<WindowId, window::Window>,
    window_ids: Vec<WindowId>,
}

impl Runtime {
    pub fn new(mut root: impl FnMut() + 'static) -> Runtime {
        Runtime {
            moxie_runtime: MoxieRuntime::new(Box::new(move || {
                topo::call!({ crate::moxie::elements::root(&mut root) })
            })),
            windows: HashMap::new(),
            window_ids: vec![],
        }
    }

    fn process(
        &mut self,
        event: Event<()>,
        _target: &EventLoopWindowTarget<()>,
        control_flow: &mut ControlFlow,
    ) {
        match event {
            Event::WindowEvent { event, window_id } => {
                self.windows.get_mut(&window_id).unwrap().process(event)
            }
            _ => *control_flow = ControlFlow::Wait,
        }
    }

    fn update_runtime(&mut self, event_loop: &EventLoop<()>) {
        let windows = self.moxie_runtime.run_once();

        let first_iter = windows.into_iter().map(Some).chain(iter::repeat(None));
        let second_iter = self
            .window_ids
            .drain(..)
            .collect::<Vec<_>>()
            .into_iter()
            .map(Some)
            .chain(iter::repeat(None));

        for (dom_window, window_id) in first_iter.zip(second_iter) {
            match (dom_window, window_id) {
                (Some(dom_window), Some(window_id)) => {
                    self.windows
                        .get_mut(&window_id)
                        .unwrap()
                        .set_dom_window(dom_window);
                    self.window_ids.push(window_id);
                }
                (Some(dom_window), None) => {
                    let window = window::Window::new(dom_window, event_loop);
                    let id = window.window_id();
                    self.windows.insert(id, window);
                    self.window_ids.push(id);
                }
                (None, Some(window_id)) => {
                    self.windows.remove(&window_id);
                }
                (None, None) => break,
            }
        }
    }

    pub fn start(mut self) {
        let event_loop = EventLoop::new();

        self.update_runtime(&event_loop);

        event_loop
            .run(move |event, target, control_flow| self.process(event, target, control_flow));
    }
}
